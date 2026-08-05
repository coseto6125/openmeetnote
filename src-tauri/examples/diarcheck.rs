//! 驗證 pyannote 語者分離對真實會議對答的效果。
//!
//! VAD 只知道「有沒有聲音」，快速對答時兩人之間沒有足以切開的停頓，
//! 一段裡就混了好幾個人。segmentation 模型認的是語者切換本身。

use openmeetnote_lib::stt::load_wav_16k_mono;
use sherpa_rs::diarize::{Diarize, DiarizeConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let wav = std::env::args().nth(1).expect("用法：diarcheck <wav>");
    let bench = "/home/enor/whisper-bench";
    let samples = load_wav_16k_mono(&wav)?;
    println!("音訊 {:.1}s\n", samples.len() as f64 / 16_000.0);

    for threshold in [0.5f32, 0.7, 0.9] {
        let mut d = Diarize::new(
            format!("{bench}/sherpa-onnx-pyannote-segmentation-3-0/model.onnx"),
            format!("{bench}/emb.onnx"),
            DiarizeConfig {
                // 不指定人數，讓門檻決定分出幾位：真實會議不知道會有幾個人講話
                num_clusters: Some(-1),
                threshold: Some(threshold),
                ..Default::default()
            },
        )?;
        let t0 = std::time::Instant::now();
        let segs = d.compute(samples.clone(), None)?;
        let speakers: std::collections::BTreeSet<i32> = segs.iter().map(|s| s.speaker).collect();
        println!(
            "門檻 {threshold:.1} → {} 位語者、{} 段（{:.1}s，RTF {:.3}）",
            speakers.len(),
            segs.len(),
            t0.elapsed().as_secs_f64(),
            t0.elapsed().as_secs_f64() / (samples.len() as f64 / 16_000.0),
        );
        for s in segs.iter().take(10) {
            println!("  {:6.1}s–{:6.1}s  語者 {}", s.start, s.end, s.speaker);
        }
    }
    Ok(())
}
