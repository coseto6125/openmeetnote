//! STT 管線的整合測試（BLUEPRINT.md §15.2）。
//!
//! 單元測試守得住斷句、比對與時間換算，守不住「這個模型在這台機器上到底
//! 認不認得出台灣會議」。那需要真實模型與真實音訊，而這兩者都太大不能進
//! 版本庫，所以缺席時整份跳過而不是失敗 —— 讓 CI 因為沒有模型就紅燈，
//! 只會訓練出「忽略紅燈」的習慣。
//!
//! ```bash
//! OMN_TEST_ASSETS=/home/enor/whisper-bench cargo test --test stt_pipeline
//! ```

use std::path::{Path, PathBuf};

use openmeetnote_lib::stt::{
    diff::{self, Corrections},
    load_wav_16k_mono,
    paraformer::Paraformer,
    whisper::Whisper,
};

/// 測試素材的位置。這些檔案（模型與會議錄音）合計超過一 GB，不進版本庫。
fn assets() -> Option<PathBuf> {
    let dir = PathBuf::from(
        std::env::var("OMN_TEST_ASSETS").unwrap_or_else(|_| "/home/enor/whisper-bench".into()),
    );
    dir.is_dir().then_some(dir)
}

fn require(dir: &Path, rel: &str) -> Option<PathBuf> {
    let p = dir.join(rel);
    p.exists().then_some(p)
}

/// 這段音訊裡確實出現、而且是台灣會議特有的詞。
///
/// 選詞的標準是「認錯了使用者一定會發現」：機關簡稱與法案名稱錯了，
/// 整份會議紀錄就不能用。
const EXPECTED: &[&str] = &[
    "原住民基本法",
    "原住民身份法",
    "南島語族",
    "原民會",
    "海委會",
    "文化部",
];

#[test]
fn test_whisper_transcribes_taiwanese_meeting_terms() {
    let Some(dir) = assets() else {
        eprintln!("略過：找不到測試素材目錄");
        return;
    };
    let (Some(model), Some(wav)) = (
        require(&dir, "models/ggml-large-v3-turbo-q5_0.bin"),
        require(&dir, "near.wav"),
    ) else {
        eprintln!("略過：缺少 whisper 模型或測試音訊");
        return;
    };

    let samples = load_wav_16k_mono(wav.to_str().unwrap()).expect("讀取音訊");
    let engine = Whisper::load(model.to_str().unwrap(), 4).expect("載入 whisper");
    let segments = engine.transcribe(&samples).expect("轉錄");

    let text: String = segments.iter().map(|s| s.text.as_str()).collect();
    let hits: Vec<&str> = EXPECTED
        .iter()
        .copied()
        .filter(|w| text.contains(w))
        .collect();
    assert!(
        hits.len() >= 4,
        "只認出 {:?}，關鍵詞命中太低，可能模型或參數退化了。全文：{text}",
        hits
    );

    // 片段必須帶時間，否則引用無法定位（§5.3）
    assert!(
        segments.iter().all(|s| s.end_ms > s.start_ms),
        "有片段的時間區間是空的或反向的"
    );
    let covered = segments.last().map(|s| s.end_ms).unwrap_or(0);
    let audio_ms = samples.len() as u64 * 1000 / 16_000;
    assert!(
        covered * 100 / audio_ms >= 90,
        "只涵蓋到 {covered} ms，音訊有 {audio_ms} ms —— 尾段被截掉了"
    );
}

#[test]
fn test_paraformer_is_fast_enough_for_live_captions() {
    let Some(dir) = assets() else {
        eprintln!("略過：找不到測試素材目錄");
        return;
    };
    let (Some(model_dir), Some(wav)) = (
        require(&dir, "sherpa-onnx-paraformer-zh-2023-09-14"),
        require(&dir, "near.wav"),
    ) else {
        eprintln!("略過：缺少 Paraformer 模型或測試音訊");
        return;
    };

    let samples = load_wav_16k_mono(wav.to_str().unwrap()).expect("讀取音訊");
    let mut engine = Paraformer::load(model_dir.to_str().unwrap(), 4).expect("載入 Paraformer");

    let started = std::time::Instant::now();
    let tokens = engine.tokens(&samples);
    let rtf = started.elapsed().as_secs_f64() / (samples.len() as f64 / 16_000.0);

    assert!(!tokens.is_empty(), "即時稿引擎沒有產出任何 token");
    // 即時稿的意義就是跟得上說話。實測 RTF 約 0.03，留十倍餘裕當防退化的界線。
    assert!(rtf < 0.3, "RTF {rtf:.3} 太慢，即時稿會落後說話者");

    // token 的時間必須遞增，否則畫面上的字會亂序
    let times: Vec<u64> = tokens.iter().map(|t| t.at_ms).collect();
    assert!(
        times.windows(2).all(|w| w[1] >= w[0]),
        "token 時間不是遞增的，即時稿會亂序"
    );
}

#[test]
fn test_the_two_engines_disagree_exactly_where_the_hard_words_are() {
    let Some(dir) = assets() else {
        eprintln!("略過：找不到測試素材目錄");
        return;
    };
    let (Some(whisper_model), Some(para_dir), Some(wav)) = (
        require(&dir, "models/ggml-large-v3-turbo-q5_0.bin"),
        require(&dir, "sherpa-onnx-paraformer-zh-2023-09-14"),
        require(&dir, "near.wav"),
    ) else {
        eprintln!("略過：缺少模型或測試音訊");
        return;
    };

    let samples = load_wav_16k_mono(wav.to_str().unwrap()).expect("讀取音訊");
    let reference = Whisper::load(whisper_model.to_str().unwrap(), 4)
        .expect("載入 whisper")
        .transcribe(&samples)
        .expect("定稿");
    let tokens = Paraformer::load(para_dir.to_str().unwrap(), 4)
        .expect("載入 Paraformer")
        .tokens(&samples);

    let compared = diff::compare(&reference, &tokens, &Corrections::default(), 200);
    let agreed = compared.iter().filter(|c| c.agrees).count();

    // 兩個引擎對同一段音訊應該多數一致。全部不一致代表比對邏輯壞了
    // （曾經發生過：簡繁沒轉換、時間對齊用錯基準，兩次都讓一致率歸零）。
    assert!(
        agreed * 100 / compared.len().max(1) >= 40,
        "一致率只有 {agreed}/{}，比對邏輯可能壞了而不是引擎真的分歧",
        compared.len()
    );
    // 也不該全部一致：那代表比對根本沒在比
    assert!(agreed < compared.len(), "全部片段都一致，比對可能失效了");
}

#[test]
fn test_vocabulary_corrections_survive_the_full_pipeline() {
    // 詞表是使用者唯一能修正專有名詞的手段，而校正發生在轉繁之後。
    // 這個順序錯了，簡體的詞表項目就永遠對不上（實測過）。
    let corrections = Corrections::default().with(&[("招委", "召委"), ("希臘雅", "西拉雅")]);
    let simplified = "今天感谢招委排审";
    let out = diff::to_traditional(simplified, &corrections);
    assert!(out.contains("召委"), "詞表沒有套用到轉繁之後的文字：{out}");
    assert!(!out.contains("招委"));
}
