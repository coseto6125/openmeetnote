//! 量測 VAD 對真實會議音訊回報多少人聲，用來確認閘門門檻是否合理。
//!
//! 存在的理由：定稿一段都沒出來，而 `voiced_ms` 的回傳值在 app 裡看不到。
//! 與其猜門檻，不如把數字印出來。

use openmeetnote_lib::stt::load_wav_16k_mono;
use sherpa_rs::silero_vad::{SileroVad, SileroVadConfig};

const SAMPLE_RATE: u32 = 16_000;
const VAD_WINDOW: usize = 512;

fn config(model: &str) -> SileroVadConfig {
    SileroVadConfig {
        model: model.to_owned(),
        threshold: 0.35,
        min_speech_duration: 0.25,
        min_silence_duration: 0.5,
        max_speech_duration: 20.0,
        sample_rate: SAMPLE_RATE,
        window_size: 512,
        num_threads: Some(1),
        provider: None,
        debug: false,
    }
}

fn voiced_ms(vad: &mut SileroVad, samples: &[f32]) -> u64 {
    // 一次只餵一個 window。sherpa-onnx 的 VAD 是串流偵測器，整段丟進去
    // 它只會處理第一個 window，其餘直接丟掉（實測不論音訊多長都固定回報
    // 314 ms）。這裡的分批不是效能考量，是這個 API 唯一正確的用法。
    let mut frames = 0usize;
    for w in samples.chunks(VAD_WINDOW) {
        vad.accept_waveform(w.to_vec());
        while !vad.is_empty() {
            frames += vad.front().samples.len();
            vad.pop();
        }
    }
    vad.flush();
    while !vad.is_empty() {
        frames += vad.front().samples.len();
        vad.pop();
    }
    vad.clear();
    frames as u64 * 1000 / SAMPLE_RATE as u64
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let wav = std::env::args().nth(1).expect("用法：vadcheck <wav>");
    let model = std::env::var("OMN_VAD_MODEL")
        .unwrap_or_else(|_| "/home/enor/whisper-bench/silero_vad.onnx".into());
    let samples = load_wav_16k_mono(&wav)?;
    println!(
        "音訊 {:.1}s，VAD 模型 {model}\n",
        samples.len() as f64 / 16_000.0
    );

    // 就像 app 那樣切成 12 秒一段逐段判斷，並且沿用同一個 VAD 實例
    let window = 12 * SAMPLE_RATE as usize;
    let mut vad = SileroVad::new(config(&model), 14.0)?;
    for (i, chunk) in samples.chunks(window).enumerate() {
        let ms = voiced_ms(&mut vad, chunk);
        println!(
            "第 {i} 段（{:.1}s）→ 人聲 {ms} ms（門檻 700 ms）{}",
            chunk.len() as f64 / 16_000.0,
            if ms >= 700 { "通過" } else { "被擋掉" }
        );
    }

    // 對照：每段都用全新的 VAD 實例，藉此看出跨輪沿用是否才是問題
    println!("\n── 每段新建 VAD ──");
    for (i, chunk) in samples.chunks(window).enumerate() {
        let mut fresh = SileroVad::new(config(&model), 14.0)?;
        println!("第 {i} 段 → 人聲 {} ms", voiced_ms(&mut fresh, chunk));
    }
    Ok(())
}
