//! 校準聲紋門檻：對同一段多人音訊掃描不同的相似度門檻，看各自分出幾位語者。
//!
//! 門檻是這個功能唯一的旋鈕，而它的取捨是不對稱的：分太多位使用者可以手動
//! 合併，把兩個人併成一位卻會讓會議紀錄把話安到別人頭上。所以要挑的是
//! 「寧可偏多」那一側的最小值，而不是看起來最漂亮的數字。

use openmeetnote_lib::stt::load_wav_16k_mono;
use sherpa_rs::embedding_manager::EmbeddingManager;
use sherpa_rs::silero_vad::{SileroVad, SileroVadConfig};
use sherpa_rs::speaker_id::{EmbeddingExtractor, ExtractorConfig};

const SAMPLE_RATE: u32 = 16_000;
const VAD_WINDOW: usize = 512;
/// 太短的片段聲紋不穩定，比對結果形同亂數
const MIN_EMBED_MS: u64 = 1_000;

fn vad_config_with(model: &str, min_silence: f32) -> SileroVadConfig {
    SileroVadConfig {
        model: model.to_owned(),
        threshold: 0.4,
        min_speech_duration: 0.25,
        min_silence_duration: min_silence,
        max_speech_duration: 20.0,
        sample_rate: SAMPLE_RATE,
        window_size: VAD_WINDOW as i32,
        num_threads: Some(1),
        provider: None,
        debug: false,
    }
}

/// 用 VAD 把音訊切成一段段的發言，附上起訖時間。
fn utterances(vad_model: &str, samples: &[f32], min_silence: f32) -> Vec<(f32, Vec<f32>)> {
    let mut vad = SileroVad::new(vad_config_with(vad_model, min_silence), 60.0).expect("VAD");
    let mut out = Vec::new();
    let mut consumed = 0usize;
    for w in samples.chunks(VAD_WINDOW) {
        vad.accept_waveform(w.to_vec());
        consumed += w.len();
        while !vad.is_empty() {
            let seg = vad.front().samples;
            vad.pop();
            let start = (consumed.saturating_sub(seg.len())) as f32 / SAMPLE_RATE as f32;
            out.push((start, seg));
        }
    }
    vad.flush();
    while !vad.is_empty() {
        let seg = vad.front().samples;
        vad.pop();
        out.push((consumed as f32 / SAMPLE_RATE as f32, seg));
    }
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let wav = std::env::args().nth(1).expect("用法：spkcheck <wav>");
    let bench = "/home/enor/whisper-bench";
    let samples = load_wav_16k_mono(&wav)?;
    let min_silence: f32 = std::env::var("MIN_SILENCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.5);
    let utts = utterances(&format!("{bench}/silero_vad.onnx"), &samples, min_silence);
    println!("min_silence_duration = {min_silence}s");
    let usable: Vec<_> = utts
        .iter()
        .filter(|(_, s)| s.len() as u64 * 1000 / u64::from(SAMPLE_RATE) >= MIN_EMBED_MS)
        .collect();
    println!(
        "音訊 {:.1}s，VAD 切出 {} 段發言，其中 {} 段夠長可抽聲紋\n",
        samples.len() as f64 / 16_000.0,
        utts.len(),
        usable.len()
    );

    let mut extractor = EmbeddingExtractor::new(ExtractorConfig {
        model: format!("{bench}/emb.onnx"),
        num_threads: Some(4),
        provider: None,
        debug: false,
    })?;
    let dim = extractor.embedding_size as i32;

    // 聲紋只抽一次，門檻掃描重複使用
    let embeddings: Vec<(f32, Vec<f32>)> = usable
        .iter()
        .filter_map(|(t, s)| {
            extractor
                .compute_speaker_embedding(s.to_vec(), SAMPLE_RATE)
                .ok()
                .map(|e| (*t, e))
        })
        .collect();

    for threshold in [0.3f32, 0.4, 0.5, 0.6, 0.7, 0.8] {
        let mut known = EmbeddingManager::new(dim);
        let mut count = 0usize;
        let mut assigned = Vec::new();
        for (t, emb) in &embeddings {
            match known.search(emb, threshold) {
                Some(name) => assigned.push((*t, name)),
                None => {
                    count += 1;
                    let name = format!("s{count}");
                    let mut e = emb.clone();
                    known.add(name.clone(), &mut e).ok();
                    assigned.push((*t, name));
                }
            }
        }
        let seq: Vec<String> = assigned
            .iter()
            .take(14)
            .map(|(t, n)| format!("{t:.0}s:{n}"))
            .collect();
        println!("門檻 {threshold:.1} → {count} 位語者  {}", seq.join(" "));
    }
    Ok(())
}
