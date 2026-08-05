//! 驗證 Diarize 實例能不能重複使用。
//!
//! app 裡是同一個實例每十幾秒呼叫一次，離線測試卻是每次建新實例只呼叫一次。
//! 兩者的差別如果就是崩潰的原因，那設計上就得每批重建。

use openmeetnote_lib::stt::load_wav_16k_mono;
use sherpa_rs::diarize::{Diarize, DiarizeConfig};

fn make() -> Diarize {
    let bench = "/home/enor/whisper-bench";
    Diarize::new(
        format!("{bench}/sherpa-onnx-pyannote-segmentation-3-0/model.onnx"),
        format!("{bench}/emb.onnx"),
        DiarizeConfig {
            num_clusters: Some(-1),
            threshold: Some(0.9),
            ..Default::default()
        },
    )
    .expect("diarize")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let samples = load_wav_16k_mono("/home/enor/whisper-bench/multi.wav")?;
    // 切成 app 實際會送的大小
    let batch: Vec<f32> = samples.iter().take(13 * 16_000).copied().collect();

    println!("── 同一個實例連續呼叫 ──");
    let mut d = make();
    for i in 1..=4 {
        match d.compute(batch.clone(), None) {
            Ok(segs) => println!("第 {i} 次：{} 段", segs.len()),
            Err(e) => println!("第 {i} 次失敗：{e}"),
        }
    }

    println!("\n── 不釋放實例 ──");
    // Diarize 的 Drop 會 core dump，所以刻意不讓它跑
    let mut kept = std::mem::ManuallyDrop::new(make());
    for i in 1..=3 {
        match kept.compute(batch.clone(), None) {
            Ok(segs) => println!("第 {i} 次：{} 段", segs.len()),
            Err(e) => println!("第 {i} 次失敗：{e}"),
        }
    }
    std::mem::forget(d);
    println!("結束（沒有釋放任何 Diarize）");
    Ok(())
}
