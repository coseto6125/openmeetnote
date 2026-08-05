//! 效能量測（BLUEPRINT.md §12）。
//!
//! 藍圖要求「以 profiling 找出熱點再改，不得以主觀感覺宣稱最佳化完成」。
//! 這裡量的是實際會議負載下每個引擎的成本，數字寫進斷言，退化就會紅燈。

use std::time::Instant;

use openmeetnote_lib::stt::{load_wav_16k_mono, paraformer::Paraformer, whisper::Whisper};

fn assets() -> Option<std::path::PathBuf> {
    let d = std::path::PathBuf::from(
        std::env::var("OMN_TEST_ASSETS").unwrap_or_else(|_| "/home/enor/whisper-bench".into()),
    );
    d.is_dir().then_some(d)
}

#[test]
fn test_the_pipeline_keeps_up_with_real_time() {
    let Some(dir) = assets() else {
        eprintln!("略過：找不到測試素材");
        return;
    };
    let (wm, pd, wav) = (
        dir.join("models/ggml-large-v3-turbo-q5_0.bin"),
        dir.join("sherpa-onnx-paraformer-zh-2023-09-14"),
        dir.join("near.wav"),
    );
    if !wm.exists() || !pd.is_dir() || !wav.exists() {
        eprintln!("略過：缺少模型或音訊");
        return;
    }

    let samples = load_wav_16k_mono(wav.to_str().unwrap()).expect("讀音訊");
    let audio_s = samples.len() as f64 / 16_000.0;

    // 載入成本：使用者按下開始錄音之後要等多久才真的開始
    let t = Instant::now();
    let whisper = Whisper::load(wm.to_str().unwrap(), 4).expect("載入 whisper");
    let whisper_load = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let mut para = Paraformer::load(pd.to_str().unwrap(), 4).expect("載入 Paraformer");
    let para_load = t.elapsed().as_secs_f64();

    // 推論成本
    let t = Instant::now();
    let _ = para.tokens(&samples);
    let para_rtf = t.elapsed().as_secs_f64() / audio_s;

    let t = Instant::now();
    let _ = whisper.transcribe(&samples).expect("定稿");
    let whisper_rtf = t.elapsed().as_secs_f64() / audio_s;

    println!("載入：whisper {whisper_load:.2}s、Paraformer {para_load:.2}s");
    println!("RTF：whisper {whisper_rtf:.3}、Paraformer {para_rtf:.3}");

    // 兩個引擎跑在各自的執行緒上，所以界線要分開設 —— 把兩個 RTF 相加
    // 不對應任何真實情況。真正會壞的是定稿追不上錄音：RTF 到 1.0 就
    // 開始越積越多，一小時的會議永遠處理不完。留 20% 餘裕當防線。
    assert!(
        whisper_rtf < 0.8,
        "定稿 RTF {whisper_rtf:.3} 太高，長會議會越積越多"
    );

    // 即時稿要在說完話的當下就出現，慢下來使用者會直接看到字卡住
    assert!(para_rtf < 0.1, "即時稿 RTF {para_rtf:.3} 太高，畫面會延遲");

    // VAD 閘門跑在每一批定稿之前，它的成本直接加在定稿延遲上
    let vad_model = std::env::var("OMN_VAD_MODEL")
        .unwrap_or_else(|_| dir.join("silero_vad.onnx").to_string_lossy().into_owned());
    if std::path::Path::new(&vad_model).exists() {
        let t = Instant::now();
        let voiced = openmeetnote_lib::stt::gate_voiced_ms(&vad_model, &samples).expect("閘門");
        let gate_rtf = t.elapsed().as_secs_f64() / audio_s;
        println!("閘門 RTF {gate_rtf:.4}（判出 {voiced} ms 人聲）");
        assert!(voiced > 0, "閘門把真實會議音訊判成沒有人聲");
        assert!(
            gate_rtf < 0.02,
            "閘門 RTF {gate_rtf:.4} 太高，它跑在每一批定稿前面"
        );
    }

    // 載入時間直接就是使用者按下開始錄音後的空窗，模型都在本機不該超過十秒
    assert!(
        whisper_load + para_load < 10.0,
        "模型載入要 {:.1}s，開始錄音的等待太久",
        whisper_load + para_load
    );
}
