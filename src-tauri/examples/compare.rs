//! 離線雙引擎比較：吃一個 16 kHz 單聲道 WAV，輸出兩份逐字稿與分歧位置。
//!
//! 用途是拿真實會議錄音跟其他工具（Notion AI 之類）並排比對，因此在音訊擷取
//! （M1）完成前就能驗證轉錄品質與分歧標記的實際效果。
//!
//! ```text
//! OMN_WHISPER_MODEL=/path/ggml-large-v3-turbo-q5_0.bin \
//! OMN_PARAFORMER_DIR=/path/sherpa-onnx-paraformer-zh-2023-09-14 \
//! cargo run --release --example compare -- meeting.wav
//! ```

use std::time::Instant;

use openmeetnote_lib::stt::{
    diff::{self, Corrections},
    load_wav_16k_mono,
    paraformer::Paraformer,
    whisper::Whisper,
};

/// 兩個引擎各用幾個執行緒。定稿的吞吐直接決定會議結束後要等多久，
/// 所以這裡要能改：`OMN_THREADS=8 cargo run --release --example compare -- x.wav`
fn threads() -> i32 {
    std::env::var("OMN_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_owned())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let wav = std::env::args()
        .nth(1)
        .ok_or("用法：compare <wav>（16 kHz 單聲道）")?;
    let whisper_model = env_or(
        "OMN_WHISPER_MODEL",
        "/home/enor/whisper-bench/models/ggml-large-v3-turbo-q5_0.bin",
    );
    let paraformer_dir = env_or(
        "OMN_PARAFORMER_DIR",
        "/home/enor/whisper-bench/sherpa-onnx-paraformer-zh-2023-09-14",
    );

    let samples = load_wav_16k_mono(&wav)?;
    let audio_s = samples.len() as f64 / 16_000.0;
    println!("音訊 {audio_s:.1}s（{wav}）\n");

    let t = Instant::now();
    let mut fast = Paraformer::load(&paraformer_dir, threads())?;
    let tokens = fast.tokens(&samples);
    let fast_s = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let slow = Whisper::load(&whisper_model, threads())?.transcribe(&samples)?;
    let slow_s = t.elapsed().as_secs_f64();

    // 使用者詞表之後從設定讀。現在放這裡是為了讓輸出示範校正表的效果，
    // 而不是把專有名詞寫死在程式碼裡。
    let corrections = Corrections::default();
    let compared = diff::compare(&slow, &tokens, &corrections, 200);
    let agreed = compared.iter().filter(|c| c.agrees).count();

    println!(
        "Paraformer {fast_s:.2}s（RTF {:.3}）{} token\n\
         whisper    {slow_s:.2}s（RTF {:.3}）{} 段\n\
         一致 {agreed}/{} 段\n",
        fast_s / audio_s,
        tokens.len(),
        slow_s / audio_s,
        slow.len(),
        compared.len(),
    );

    println!("──── 定稿（whisper）────");
    let final_text: String = slow.iter().map(|s| s.text.as_str()).collect();
    println!("{}\n", diff::to_traditional(&final_text, &corrections));

    println!("──── 即時稿（Paraformer）────");
    let live_text: String = tokens.iter().map(|t| t.text.as_str()).collect();
    println!("{}\n", diff::to_traditional(&live_text, &corrections));

    println!("──── 待確認位置 ────");
    for c in compared.iter().filter(|c| !c.agrees) {
        println!(
            "[{:>6.1}s–{:>6.1}s] 相似度 {:.2}\n  定稿：{}\n  即時：{}",
            c.segment.start_ms as f64 / 1000.0,
            c.segment.end_ms as f64 / 1000.0,
            c.similarity,
            diff::to_traditional(&c.segment.text, &corrections),
            c.counterpart,
        );
    }
    Ok(())
}
