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
//!
//! # 名單為什麼會長出不存在的人
//!
//! 一場 25 分鐘的三人會議登記出 16 位，其中 13 位各只出現過一次。原因是
//! 「比對不到就是新的人」這條規則對短片段成立得太容易：一兩秒的聲紋方向
//! 本來就飄，跟誰都不像很正常。
//!
//! 所以現在先看長度，再看相似度（見 [`judge`]）。夠長的片段聲紋是穩的，可以
//! 照相似度辦事：夠像就併入並修正那個人的聲紋中心，不夠像就是另一個人，登記
//! 進名單。太短的片段不足以定義一個人，只拿來靠向最像的那位而不動他的中心，
//! 連像都談不上就不標，那句話沿用軌道的預設語者。
//!
//! 長度這道閘門要排在相似度前面。反過來的話，聲音跟既有語者有幾分像的人不管
//! 講多久都會被歸給那位，永遠登記不進名單 —— 而「把話安到別人頭上」正是這裡
//! 最不能犯的錯：多一位不存在的人使用者看得見也改得掉，話被安錯人看不出來。

use std::sync::{Mutex, OnceLock};

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

/// 「跟誰都不像」的界線。
///
/// 相似度落在這裡與 `SAME_SPEAKER` 之間的片段，歸給最像的那位，但不併進他的
/// 聲紋中心：不確定的樣本一旦進了中心，之後每一次比對都被它拉偏。
const NEW_SPEAKER: f32 = 0.35;

/// 抽聲紋需要的最短音訊。太短的片段聲紋不穩定，比對結果形同亂數。
const MIN_EMBED_MS: u64 = 1_000;

/// 登記一位新語者需要的最短音訊。
///
/// 一秒的聲紋足以拿去比對，卻不足以定義一個人。比對不到又不夠長的片段回
/// `None`，那句話沿用軌道的預設語者。
const MIN_ENROLL_MS: u64 = 2_000;

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

/// 一位已經聽過的語者：名字，加上他歷來聲紋的平均方向。
struct Voice {
    name: String,
    /// 歷來聲紋的總和。方向就是這個人的聲紋中心。
    ///
    /// 存總和而不是存平均：每併入一段就重新正規化，累積的長度會被丟掉，
    /// 下一次再取加權平均等於假設舊向量全部同向，於是歷史方向被過度加權。
    /// 100 個 60 度的樣本加 100 個 0 度的，真正的平均方向是 30 度，逐次
    /// 正規化的版本得到 32.18 度，而且會議越長偏得越多、越跟不上聲音的變化。
    sum: Vec<f32>,
    /// [`Voice::sum`] 正規化後的方向。每段話都要跟名單上每個人比一次，不重算。
    centroid: Vec<f32>,
}

impl Voice {
    /// 用第一段聲紋登記一位語者。傳進來的向量已經是單位長度。
    fn new(name: String, embedding: Vec<f32>) -> Self {
        Self {
            name,
            centroid: embedding.clone(),
            sum: embedding,
        }
    }

    /// 把一段確定是本人的聲紋併進中心。
    ///
    /// 取平均而不是換掉：登記當下拿到的往往是最差的一個樣本（第一次開口
    /// 通常很短又只有半句），後面每一段都讓這個人準一點。
    fn absorb(&mut self, embedding: &[f32]) {
        for (acc, e) in self.sum.iter_mut().zip(embedding) {
            *acc += e;
        }
        self.centroid.copy_from_slice(&self.sum);
        normalize(&mut self.centroid);
    }
}

struct Registry {
    extractor: EmbeddingExtractor,
    known: Vec<Voice>,
}

/// 一段聲紋跟名單比對之後的結論。
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// 就是這位，而且夠像，可以拿來修正他的聲紋中心。
    Same(usize),
    /// 最像這位，但不夠像到能當他的樣本。
    Likely(usize),
    /// 跟誰都不像，長度也夠，登記一位新的。
    New,
    /// 分不出來，交給呼叫端沿用軌道預設。
    Unknown,
}

/// 依相似度與片段長度決定這段話算誰的。
///
/// 分支順序是這裡的重點。`New` 要排在 `Likely` 前面：反過來的話，聲音跟既有
/// 語者有幾分像的人（相似度落在 [`NEW_SPEAKER`] 與 [`SAME_SPEAKER`] 之間）
/// 不管講多久都會判成 `Likely`，於是永遠登記不進名單，他講的每一句都掛在
/// 那位既有語者名下 —— 而那正是「把話安到別人頭上」這件事本身。
///
/// 兩條界線量的是不同的東西：相似度回答「像不像」，長度回答「這段聲紋可不可
/// 信」。夠長的片段聲紋是穩的，所以不夠像就是別人；太短的片段本來就飄，不足
/// 以定義一個人，只拿來靠向最像的那位。
///
/// 名單為空又每段都不足 [`MIN_ENROLL_MS`] 時，結果一路是 `Unknown`，沒有人會
/// 被登記。那是刻意的：一秒的碎片定義不了一個人，那些話沿用軌道的預設語者
/// （遠端軌就是「遠端」），比憑碎片捏出一個人誠實。
fn judge(nearest: Option<(usize, f32)>, ms: u64) -> Verdict {
    match nearest {
        Some((i, sim)) if sim >= SAME_SPEAKER => Verdict::Same(i),
        _ if ms >= MIN_ENROLL_MS => Verdict::New,
        Some((i, sim)) if sim >= NEW_SPEAKER => Verdict::Likely(i),
        _ => Verdict::Unknown,
    }
}

/// 把向量正規化成單位長度，相似度就等於內積。
fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

/// 兩個單位向量的餘弦相似度。
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// 名單裡最像這段聲紋的那位。
fn nearest(known: &[Voice], embedding: &[f32]) -> Option<(usize, f32)> {
    known
        .iter()
        .enumerate()
        .map(|(i, v)| (i, cosine(&v.centroid, embedding)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
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
            let _ = BOOK.set(Mutex::new(Registry {
                extractor,
                known: Vec::new(),
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
        normalize(&mut embedding);

        let speaker = match judge(nearest(&reg.known, &embedding), ms) {
            Verdict::Same(i) => {
                reg.known[i].absorb(&embedding);
                reg.known[i].name.clone()
            }
            Verdict::Likely(i) => reg.known[i].name.clone(),
            Verdict::New => {
                let name = format!("s{}", reg.known.len() + 1);
                reg.known.push(Voice::new(name.clone(), embedding));
                name
            }
            Verdict::Unknown => return None,
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

    fn voice(name: &str, centroid: &[f32]) -> Voice {
        let mut centroid = centroid.to_vec();
        normalize(&mut centroid);
        Voice::new(name.into(), centroid)
    }

    /// 平面上的單位向量，方便用角度描述聲紋方向。
    fn at(deg: f32) -> Vec<f32> {
        let r = deg.to_radians();
        vec![r.cos(), r.sin()]
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

    #[test]
    fn test_a_short_clip_that_matches_nobody_is_left_unattributed() {
        // 一兩秒的聲紋跟誰都不像很正常，那不是新的人，是資訊不足
        assert_eq!(judge(Some((0, 0.2)), 1_500), Verdict::Unknown);
        assert_eq!(judge(None, 1_500), Verdict::Unknown);
    }

    #[test]
    fn test_a_long_clip_that_matches_nobody_enrolls_a_new_speaker() {
        // 夠長又跟誰都不像才是真的多了一個人
        assert_eq!(judge(Some((0, 0.2)), 2_000), Verdict::New);
        assert_eq!(judge(None, 3_000), Verdict::New);
    }

    #[test]
    fn test_a_short_clip_still_joins_a_speaker_it_matches() {
        // 長度只限制「開新的人」，不限制歸給既有的人
        assert_eq!(judge(Some((1, 0.7)), 1_000), Verdict::Same(1));
    }

    #[test]
    fn test_a_short_middling_match_joins_the_nearest_speaker_without_teaching_it() {
        // 半像不像又短的片段歸給最像的那位，但不能拿去修正他的聲紋中心
        assert_eq!(judge(Some((2, 0.4)), 1_500), Verdict::Likely(2));
    }

    #[test]
    fn test_a_long_middling_match_enrolls_instead_of_joining_the_nearest_speaker() {
        // 講了五秒還只有 0.4 像，那是另一個人。判成 Likely 的話他永遠登記
        // 不進名單，講一整場都會掛在既有語者名下。
        assert_eq!(judge(Some((2, 0.4)), 5_000), Verdict::New);
    }

    #[test]
    fn test_a_speaker_who_half_matches_an_existing_one_can_still_enroll() {
        // judge 的分支順序退回去的話，這一條會變成 Likely(0) 而永遠不登記
        let known = [voice("s1", &at(0.0))];
        let probe = at(60.0); // cos 60° = 0.5 剛好在 SAME_SPEAKER 上，取 61 度落進中間帶
        let probe = if cosine(&known[0].centroid, &probe) >= SAME_SPEAKER {
            at(61.0)
        } else {
            probe
        };
        let sim = cosine(&known[0].centroid, &probe);
        assert!(
            (NEW_SPEAKER..SAME_SPEAKER).contains(&sim),
            "這個測試要的是落在中間帶的相似度，實際 {sim}"
        );
        assert_eq!(judge(nearest(&known, &probe), 5_000), Verdict::New);
    }

    #[test]
    fn test_the_centroid_is_the_mean_direction_of_every_sample_absorbed() {
        // 逐次正規化會把累積長度丟掉，舊方向於是被過度加權：100 個 60 度
        // 加 100 個 0 度，正確的平均方向是 30 度，那個版本得到 32.18 度。
        let mut v = voice("s1", &at(60.0));
        for _ in 0..99 {
            v.absorb(&at(60.0));
        }
        for _ in 0..100 {
            v.absorb(&at(0.0));
        }
        let deg = v.centroid[1].atan2(v.centroid[0]).to_degrees();
        assert!((deg - 30.0).abs() < 0.01, "平均方向應為 30 度，實際 {deg}");
    }

    #[test]
    fn test_the_nearest_speaker_is_the_one_with_the_highest_similarity() {
        let known = [voice("s1", &[1.0, 0.0]), voice("s2", &[0.0, 1.0])];
        let mut probe = vec![0.2, 1.0];
        normalize(&mut probe);
        let (i, sim) = nearest(&known, &probe).expect("名單非空就一定有最近的一位");
        assert_eq!(i, 1);
        assert!(sim > 0.9, "相似度應該接近 1，實際 {sim}");
        assert_eq!(nearest(&[], &probe), None);
    }

    #[test]
    fn test_absorbing_a_sample_moves_the_centroid_toward_it() {
        // 第一個樣本往往最差，後續的發言要能把中心拉回來
        let mut v = voice("s1", &[1.0, 0.0]);
        let mut sample = vec![0.0, 1.0];
        normalize(&mut sample);
        let before = cosine(&v.centroid, &sample);
        v.absorb(&sample);
        let after = cosine(&v.centroid, &sample);
        assert!(after > before, "併入後應更接近該樣本：{before} → {after}");
        assert!(
            (cosine(&v.centroid, &v.centroid.clone()) - 1.0).abs() < 1e-5,
            "中心必須維持單位長度，否則相似度不再是餘弦"
        );
    }
}
