//! 校準聲紋門檻：對同一段多人音訊掃描不同的相似度門檻，看各自分出幾位語者。
//!
//! 門檻是這個功能唯一的旋鈕，而它的取捨是不對稱的：分太多位使用者可以手動
//! 合併，把兩個人併成一位卻會讓會議紀錄把話安到別人頭上。所以要挑的是
//! 「寧可偏多」那一側的最小值，而不是看起來最漂亮的數字。
//!
//! # 兩種用法
//!
//! - 單一 wav：掃描門檻，看每個門檻各自分出幾位。
//! - 多個 wav：回放模式。把它們接成一整場會議，切成跟正式流程一樣長的批次，
//!   交給真正的 `SpeakerBook` 跑一遍，報告最後登記出幾位、各自被聽到幾次。
//!   掃描看的是單一旋鈕，回放看的是整組規則在真實會議上的結果 —— 一場 25
//!   分鐘的三人會議登記出 16 位，就是回放才問得出來的問題。

use std::collections::BTreeMap;

use openmeetnote_lib::stt::load_wav_16k_mono;
use openmeetnote_lib::stt::speakers::SpeakerBook;
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

/// 正式流程送進定稿的批次長度，回放要照著切。
///
/// 語者只在批次內比對 VAD 切點，批次邊界會把一段發言切成兩半，讓兩邊都變短。
/// 整段 wav 一次餵進去測不出這件事，而它正是短片段誤登記的來源。
const BATCH_MS: u64 = 10_000;

/// 用真正的 `SpeakerBook` 把整場會議跑一遍，看規則實際登記出幾位。
fn replay(wavs: &[String], bench: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut samples = Vec::new();
    for w in wavs {
        samples.extend(load_wav_16k_mono(w)?);
    }
    let mut book = SpeakerBook::load(
        &format!("{bench}/silero_vad.onnx"),
        &format!("{bench}/emb.onnx"),
    )?;

    let batch = (BATCH_MS * u64::from(SAMPLE_RATE) / 1000) as usize;
    let mut heard: BTreeMap<String, usize> = BTreeMap::new();
    let mut spans_total = 0usize;
    for chunk in samples.chunks(batch) {
        for span in book.split(chunk) {
            *heard.entry(span.speaker).or_default() += 1;
            spans_total += 1;
        }
    }

    println!(
        "回放 {} 檔、{:.1} 分鐘，批次 {BATCH_MS} ms",
        wavs.len(),
        samples.len() as f64 / f64::from(SAMPLE_RATE) / 60.0
    );
    println!("切出 {spans_total} 段發言，登記 {} 位語者", heard.len());
    let mut by_count: Vec<_> = heard.into_iter().collect();
    by_count.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (name, n) in &by_count {
        println!("  {name}: {n} 段");
    }
    let once = by_count.iter().filter(|(_, n)| *n == 1).count();
    println!("只出現一次的有 {once} 位（多半是誤登記）");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bench = "/home/enor/whisper-bench";
    if args.len() > 1 {
        return replay(&args, bench);
    }
    let wav = args.into_iter().next().expect("用法：spkcheck <wav>...");
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
