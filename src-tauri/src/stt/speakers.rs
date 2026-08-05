//! 語者分離與跨批次對應（BLUEPRINT.md §8）。
//!
//! 系統音訊軌可能坐著好幾個人，而 `track` 只分得出「本機 vs 遠端」。這裡對
//! 每一段發言抽聲紋，跟已經聽過的人比對：像就歸給同一位，不像就是新的人。
//!
//! # 為什麼不用 pyannote segmentation
//!
//! segmentation 的分離品質明顯較好（120 秒三人協商切出 22 段，連重疊發言都
//! 抓得到，聲紋比對只切得出 9 段），但 `sherpa-onnx-c-api.dll` 在 Windows 上
//! 執行 diarization 會固定崩在同一個位移（0xc0000005 @ 0x7b5c7），Linux 的
//! 同版本 .so 卻沒事。那是預編二進位本身的問題，Rust 這側補不了 —— 已經在
//! `vendor/sherpa-rs` 補掉 null 解參考與資源洩漏兩個上游 bug，崩潰依舊。
//!
//! 所以走聲紋這條：切點來自 VAD 而不是語者變化，快速對答時兩人之間沒有足夠
//! 停頓就會混在一段裡，代價是分辨力較粗。等 DLL 修好可以換回去，`split` 的
//! 介面不會變。
//!
//! # 線上而非會後
//!
//! 分群要看完整段音訊才能決定，那代表錄音當下畫面上只能寫「遠端」，散會後才
//! 變成張三李四。線上比對的代價是早期判斷比較不穩（聲紋樣本少），所以語者
//! 名稱本來就設計成可由使用者更正。

use std::sync::{Mutex, OnceLock};

use sherpa_rs::embedding_manager::EmbeddingManager;
use sherpa_rs::silero_vad::{SileroVad, SileroVadConfig};
use sherpa_rs::speaker_id::{EmbeddingExtractor, ExtractorConfig};

use super::Result;
use crate::audio::SAMPLE_RATE;

/// 判定為同一人的聲紋相似度。
///
/// 實測 0.3 到 0.5 都分出 3 位（該段實際是委員、部會、主席三人），0.6 起
/// 開始把同一個人拆成多位。取捨是不對稱的：分太多位使用者可以合併，把兩個
/// 人併成一位卻會讓會議紀錄把話安到別人頭上，而讀的人不會發現。取 0.5。
const SAME_SPEAKER: f32 = 0.5;

/// 抽聲紋需要的最短音訊。太短的片段聲紋不穩定，比對結果形同亂數。
const MIN_EMBED_MS: u64 = 1_000;

/// VAD 每次處理的樣本數，必須與 `window_size` 一致。
const VAD_WINDOW: usize = 512;

/// 切發言用的靜音長度。
///
/// 比定稿用的 VAD 短：那裡要的是完整句子，這裡要的是換人講話的位置。
/// 實測 0.5 秒把 120 秒三人協商只切成 7 段（一段裡混了好幾個人），
/// 0.2 秒切出 9 段才分得出三位。
const SPLIT_SILENCE_S: f32 = 0.2;

/// 一段某人的發言。時間相對於送進來的那批音訊。
pub struct SpeakerSpan {
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: String,
}

/// 全行程共用的聲紋比對表。
///
/// 語者身分要跨批次、跨軌一致，那個對應表就不能每批重建。放全域也順帶避免
/// 重複載入模型（每次幾百毫秒）。
static BOOK: OnceLock<Mutex<Registry>> = OnceLock::new();

struct Registry {
    extractor: EmbeddingExtractor,
    known: EmbeddingManager,
    count: usize,
}

pub struct SpeakerBook {
    vad_model: String,
}

impl SpeakerBook {
    /// 準備語者辨識。第一次呼叫會載入模型，之後共用同一份對應表。
    pub fn load(vad_model: &str, embedding: &str) -> Result<Self> {
        if BOOK.get().is_none() {
            let extractor = EmbeddingExtractor::new(ExtractorConfig {
                model: embedding.to_owned(),
                num_threads: Some(1),
                provider: None,
                debug: false,
            })
            .map_err(|e| super::SttError::Load(e.to_string()))?;
            let dim = extractor.embedding_size as i32;
            let _ = BOOK.set(Mutex::new(Registry {
                extractor,
                known: EmbeddingManager::new(dim),
                count: 0,
            }));
        }
        Ok(Self {
            vad_model: vad_model.to_owned(),
        })
    }

    /// 切出這批音訊裡各段是誰講的，語者名稱跨批次一致。
    ///
    /// 回傳空的代表這批分不出來，呼叫端應沿用軌道的預設語者 —— 這條路徑
    /// 必須永遠可用，語者分不出來只是少了一個資訊，不該讓逐字稿跟著沒有。
    pub fn split(&mut self, samples: &[f32]) -> Vec<SpeakerSpan> {
        let Ok(mut vad) = SileroVad::new(
            SileroVadConfig {
                model: self.vad_model.clone(),
                threshold: 0.4,
                min_speech_duration: 0.25,
                min_silence_duration: SPLIT_SILENCE_S,
                max_speech_duration: 20.0,
                sample_rate: SAMPLE_RATE,
                window_size: VAD_WINDOW as i32,
                num_threads: Some(1),
                provider: None,
                debug: false,
            },
            (samples.len() as f32 / SAMPLE_RATE as f32) + 5.0,
        ) else {
            return Vec::new();
        };

        // 一次只餵一個 window：sherpa-onnx 的 VAD 是串流偵測器，整段丟進去
        // 它只處理第一個 window，其餘直接丟掉。
        let mut spans = Vec::new();
        let mut consumed = 0usize;
        for w in samples.chunks(VAD_WINDOW) {
            vad.accept_waveform(w.to_vec());
            consumed += w.len();
            while !vad.is_empty() {
                let speech = vad.front().samples;
                vad.pop();
                let start = consumed.saturating_sub(speech.len());
                if let Some(span) = self.attribute(&speech, start) {
                    spans.push(span);
                }
            }
        }
        vad.flush();
        while !vad.is_empty() {
            let speech = vad.front().samples;
            vad.pop();
            let start = consumed.saturating_sub(speech.len());
            if let Some(span) = self.attribute(&speech, start) {
                spans.push(span);
            }
        }
        spans
    }

    /// 把一段發言歸給某個人，必要時登記一位新語者。
    fn attribute(&self, speech: &[f32], start_sample: usize) -> Option<SpeakerSpan> {
        let ms = speech.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
        if ms < MIN_EMBED_MS {
            return None;
        }
        let lock = BOOK.get()?;
        let mut reg = lock.lock().ok()?;

        let mut embedding = reg
            .extractor
            .compute_speaker_embedding(speech.to_vec(), SAMPLE_RATE)
            .ok()?;
        let speaker = match reg.known.search(&embedding, SAME_SPEAKER) {
            Some(name) => name,
            None => {
                reg.count += 1;
                let name = format!("s{}", reg.count);
                reg.known.add(name.clone(), &mut embedding).ok()?;
                name
            }
        };

        let start_ms = start_sample as u64 * 1000 / u64::from(SAMPLE_RATE);
        Some(SpeakerSpan {
            start_ms,
            end_ms: start_ms + ms,
            speaker,
        })
    }
}

/// 找出這個時間點是誰在講話：取重疊最多的那一段。
///
/// whisper 的斷句與發言切點不會一致，一句話可能橫跨兩位語者（對答插話時），
/// 取重疊最長的那位是最不容易錯的歸屬。
pub fn speaker_at(spans: &[SpeakerSpan], start_ms: u64, end_ms: u64) -> Option<&str> {
    spans
        .iter()
        .filter_map(|s| {
            let lo = start_ms.max(s.start_ms);
            let hi = end_ms.min(s.end_ms);
            (hi > lo).then(|| (hi - lo, s.speaker.as_str()))
        })
        .max_by_key(|(overlap, _)| *overlap)
        .map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start_ms: u64, end_ms: u64, speaker: &str) -> SpeakerSpan {
        SpeakerSpan {
            start_ms,
            end_ms,
            speaker: speaker.into(),
        }
    }

    #[test]
    fn test_a_sentence_is_attributed_to_the_speaker_it_overlaps_most() {
        // 一句話橫跨兩位語者時（插話），歸給說得比較多的那位
        let spans = [span(0, 1000, "s1"), span(900, 3000, "s2")];
        assert_eq!(speaker_at(&spans, 800, 2800), Some("s2"));
        assert_eq!(speaker_at(&spans, 0, 950), Some("s1"));
    }

    #[test]
    fn test_a_sentence_outside_every_span_has_no_speaker() {
        // 沒有歸屬時回 None，讓呼叫端沿用軌道預設，而不是硬指定一位
        let spans = [span(0, 1000, "s1")];
        assert_eq!(speaker_at(&spans, 2000, 3000), None);
        assert_eq!(speaker_at(&[], 0, 1000), None);
    }

    #[test]
    fn test_touching_spans_do_not_count_as_overlap() {
        // 邊界相接不算重疊，否則每句都會意外歸到前一位語者
        let spans = [span(0, 1000, "s1")];
        assert_eq!(speaker_at(&spans, 1000, 2000), None);
    }
}
