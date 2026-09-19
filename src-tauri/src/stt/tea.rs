//! TEA-ASR 定稿引擎。
//!
//! JacobLinCool/TEA-ASR-1.1 是 Qwen3-ASR 1.7B 針對台灣華語的微調，這裡透過
//! llama.cpp 的多模態（mtmd）音訊路徑在行程內執行：文字解碼器
//! `TEA-ASR-1.1.Q4_K_M.gguf` 加音訊編碼器 `TEA-ASR-1.1.mmproj-Q8_0.gguf`。
//! Q4_K_M 與 Q8_0 準確度相同而更快，CPU 四執行緒實測 RTF 約 0.1 到 0.15。
//!
//! 模型不給時間戳。每句的時間是按字數比例從送進去的音訊長度分配出來的近似值
//! （見 [`timed_sentences`]），要精確的時間得另外接強制對齊。

use std::num::NonZeroU32;
use std::sync::OnceLock;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::mtmd::{
    mtmd_default_marker, MtmdBitmap, MtmdContext, MtmdContextParams, MtmdInputText,
};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::{send_logs_to_tracing, LogOptions};

use super::{Result, Segment, SttError};

/// 模型吃的取樣率。擷取端已經是 16 kHz 單聲道，這裡只用來換算長度。
const SAMPLE_RATE: f64 = 16_000.0;

/// 送進模型的熱詞上限。實測 30 個與這場無關的詞也不傷準確度，64 是給
/// 長期累積的詞表留的餘裕，同時讓 prompt 長度有界。
const MAX_HOTWORDS: usize = 64;

/// 尾端同一個 n-gram 連續出現幾次算失控。
const LOOP_REPEATS: usize = 8;

/// 偵測失控時看的 n-gram 長度上限（token 數）。
const LOOP_MAX_N: usize = 4;

/// prompt 與音訊 embedding 每次送進解碼器的 token 數，也是 ubatch 大小。
/// 與 llama.cpp 的預設 ubatch 相同；調大只會讓計算緩衝區變大，
/// Qwen3-ASR 的音訊段是因果注意力，不需要整段落在同一個 ubatch。
const EVAL_BATCH: u32 = 512;

/// 單句超過這個字數時，再從逗號切開。
const MAX_SENTENCE_CHARS: usize = 40;

/// llama.cpp 的 backend 一個行程只能初始化一次，第二次會回錯。
/// 測試與收尾路徑都可能在同一個行程裡載入兩次模型。
fn backend() -> Result<&'static LlamaBackend> {
    static BACKEND: OnceLock<std::result::Result<LlamaBackend, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            // llama.cpp、mtmd 與 ggml 的日誌預設直接寫 stderr，每批幾十行。
            // 失敗會以 Err 回到呼叫端並寫進 stt.log，那些日誌不需要。
            send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
            LlamaBackend::init().map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| SttError::Load(e.clone()))
}

pub struct Tea {
    model: LlamaModel,
    mtmd: MtmdContext,
    threads: i32,
}

impl Tea {
    pub fn load(model_path: &str, mmproj_path: &str, threads: i32) -> Result<Self> {
        let backend = backend()?;
        // llama.cpp 在路徑不存在時只回 null，錯誤訊息說不出是哪一個檔案
        for p in [model_path, mmproj_path] {
            if !std::path::Path::new(p).exists() {
                return Err(SttError::Load(format!("找不到檔案：{p}")));
            }
        }
        let model = LlamaModel::load_from_file(backend, model_path, &LlamaModelParams::default())
            .map_err(|e| SttError::Load(format!("{model_path}：{e}")))?;
        let params = MtmdContextParams {
            use_gpu: false,
            print_timings: false,
            n_threads: threads,
            ..MtmdContextParams::default()
        };
        let mtmd = MtmdContext::init_from_file(mmproj_path, &model, &params)
            .map_err(|e| SttError::Load(format!("{mmproj_path}：{e}")))?;
        if !mtmd.support_audio() {
            return Err(SttError::Load(format!("{mmproj_path} 不是音訊編碼器")));
        }
        Ok(Self {
            model,
            mtmd,
            threads,
        })
    }

    /// 轉錄一段音訊，回傳按句切開、時間按字數比例分配的片段。
    ///
    /// 時間相對於 `samples` 的開頭。`hotwords` 是 `、` 串起來的詞（可為空），
    /// 放在 system 那一格，見 [`hotword_prompt`]。
    pub fn transcribe(&self, samples: &[f32], hotwords: &str) -> Result<Vec<Segment>> {
        let text = self.text(samples, hotwords)?;
        Ok(timed_sentences(&text, 0, duration_ms(samples.len())))
    }

    /// 轉錄一段音訊，回傳整段文字（已去掉私用區字元並截掉失控的重複）。
    pub fn text(&self, samples: &[f32], hotwords: &str) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let decode = |e: &dyn std::fmt::Display| SttError::Decode(e.to_string());

        let bitmap = MtmdBitmap::from_audio_data(samples).map_err(|e| decode(&e))?;
        // 原始 prompt，不套 chat template：模型訓練時就是這個形狀，
        // 助手回合預先填好語言標籤，模型接著吐轉錄文字。
        let prompt = format!(
            "<|im_start|>system\n{hotwords}<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\nlanguage Chinese<asr_text>",
            mtmd_default_marker()
        );
        let chunks = self
            .mtmd
            .tokenize(
                MtmdInputText {
                    text: prompt,
                    add_special: false,
                    parse_special: true,
                },
                &[&bitmap],
            )
            .map_err(|e| decode(&e))?;

        let cap = token_cap(samples.len() as f64 / SAMPLE_RATE);
        // 每批開一個 context 而不是長駐一個：context 借用 model，長駐的話
        // Tea 會變成自我參照的結構。開 context 是配置 KV cache，相對於
        // 數十秒音訊的推論可以忽略。
        let n_ctx = u32::try_from(chunks.total_tokens() + cap + 16).map_err(|e| decode(&e))?;
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(n_ctx))
            .with_n_batch(EVAL_BATCH)
            .with_n_ubatch(EVAL_BATCH)
            .with_n_threads(self.threads)
            .with_n_threads_batch(self.threads);
        let mut ctx = self
            .model
            .new_context(backend()?, ctx_params)
            .map_err(|e| decode(&e))?;
        let mut n_past = chunks
            .eval_chunks(&self.mtmd, &ctx, 0, 0, EVAL_BATCH as i32, true)
            .map_err(|e| decode(&e))?;

        let mut sampler = LlamaSampler::greedy();
        let mut batch = LlamaBatch::new(1, 1);
        let mut tokens = Vec::with_capacity(cap);
        let mut looped = None;
        while tokens.len() < cap {
            let token = sampler.sample(&ctx, -1);
            if self.model.is_eog_token(token) {
                break;
            }
            tokens.push(token);
            if let Some(keep) = loop_cut(&tokens) {
                looped = Some(tokens.len());
                tokens.truncate(keep);
                break;
            }
            batch.clear();
            batch
                .add(token, n_past, &[0], true)
                .map_err(|e| decode(&e))?;
            n_past += 1;
            ctx.decode(&mut batch).map_err(|e| decode(&e))?;
        }

        // 一個 token 不一定是完整的 UTF-8 字元，所以整串 bytes 接好才解碼
        let mut bytes = Vec::new();
        for &t in &tokens {
            bytes.extend(
                self.model
                    .token_to_piece_bytes(t, 64, false, None)
                    .map_err(|e| decode(&e))?,
            );
        }
        let text = strip_pua(&String::from_utf8_lossy(&bytes));
        if let Some(n) = looped {
            super::live::log(&format!(
                "定稿重複迴圈：第 {n} 個 token 偵測到，截回 {} 個 token → {text:?}",
                tokens.len()
            ));
        } else if tokens.len() == cap {
            super::live::log(&format!("定稿達到 token 上限 {cap}，後面可能被截掉"));
        }
        Ok(text.trim().to_owned())
    }
}

fn duration_ms(samples: usize) -> u64 {
    (samples as f64 * 1000.0 / SAMPLE_RATE) as u64
}

/// 刪掉 Unicode 私用區（U+E000..U+F8FF）的字元。
///
/// 模型的 tokenizer 在詞與詞之間輸出私用區的分隔字元，HF 的 decoder 會把它們
/// 拿掉，llama.cpp 不會。它們必須在任何東西碰到文字之前消失：留著的話
/// 繁簡轉換、校正表與比對都會把它們當成內容。
pub fn strip_pua(s: &str) -> String {
    s.chars()
        .filter(|c| !('\u{E000}'..='\u{F8FF}').contains(c))
        .collect()
}

/// 一段音訊最多產生幾個 token。
///
/// 正常語速遠低於每秒 8 個 token，這個上限只擋失控：約 1-2% 的批次會卡在
/// 「嗯嗯嗯…」或「這個這個…」一路生成，沒有上限就是一路算到 context 用完。
pub fn token_cap(audio_seconds: f64) -> usize {
    (audio_seconds.max(0.0) * 8.0).ceil() as usize + 16
}

/// 尾端是不是同一個 n-gram（n = 1..=4）連續重複 [`LOOP_REPEATS`] 次以上。
///
/// 是的話回傳該保留的長度：保留到重複段的第一次出現為止，後面全部截掉。
pub fn loop_cut<T: PartialEq>(tokens: &[T]) -> Option<usize> {
    (1..=LOOP_MAX_N).find_map(|n| {
        if tokens.len() < n * LOOP_REPEATS {
            return None;
        }
        // 從尾端往前，只要 tokens[i] == tokens[i + n] 就還在同一段重複裡
        let mut start = tokens.len() - n;
        while start > 0 && tokens[start - 1] == tokens[start - 1 + n] {
            start -= 1;
        }
        let reps = (tokens.len() - start) / n;
        (reps >= LOOP_REPEATS).then_some(start + n)
    })
}

/// 把轉錄文字切成句子，時間按字數比例分配在 `start_ms..end_ms` 裡。
///
/// 模型不給時間戳，這是近似：語速不均時句界會偏幾百毫秒。它夠用的理由是
/// 下游只拿時間做兩件事 —— 語者歸屬取重疊最多的一段、引用定位到大致的位置
/// —— 兩者都容得下這個誤差。
///
/// 在「。？！」切句；切完仍超過 [`MAX_SENTENCE_CHARS`] 的句子再從「，」
/// 切開，逐段累積到不超過上限為止。
///
/// 切完還超過兩倍上限的段落（沒有任何標點的長串）平均切成不超過上限的
/// 幾段。TEA-ASR 常常整段不下標點（實測 90 秒會議音訊只出一個逗號），
/// 標點模型又是可選的；不切的話一整批就是畫面上一行幾百字、時間跨一分半
/// 的片段，語者歸屬與引用定位都失去意義。切在字中間比那好。
pub fn timed_sentences(text: &str, start_ms: u64, end_ms: u64) -> Vec<Segment> {
    let sentences: Vec<String> = split_on(text, &['。', '？', '！', '?', '!'])
        .into_iter()
        .flat_map(|s| {
            if s.chars().count() > MAX_SENTENCE_CHARS {
                pack(split_on(&s, &['，', ',']), MAX_SENTENCE_CHARS)
            } else {
                vec![s]
            }
        })
        .flat_map(|s| cut_evenly(s, MAX_SENTENCE_CHARS * 2, MAX_SENTENCE_CHARS))
        .collect();
    let total: usize = sentences.iter().map(|s| s.chars().count()).sum();
    if total == 0 {
        return Vec::new();
    }
    let span = end_ms.saturating_sub(start_ms);
    let at = |chars: usize| start_ms + (span as u128 * chars as u128 / total as u128) as u64;
    let mut done = 0;
    sentences
        .into_iter()
        .map(|text| {
            let from = at(done);
            done += text.chars().count();
            Segment {
                start_ms: from,
                end_ms: at(done),
                text,
                no_speech: 0.0,
            }
        })
        .collect()
}

/// 在分隔符之後切開，分隔符留在前一段；去掉空白段。
fn split_on(text: &str, delims: &[char]) -> Vec<String> {
    text.split_inclusive(delims)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 超過 `limit` 字就平均切成每段不超過 `size` 字；沒超過的原樣回傳。
fn cut_evenly(s: String, limit: usize, size: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= limit {
        return vec![s];
    }
    let parts = chars.len().div_ceil(size);
    (0..parts)
        .map(|i| {
            chars[i * chars.len() / parts..(i + 1) * chars.len() / parts]
                .iter()
                .collect()
        })
        .collect()
}

/// 把短段依序合併，每段不超過 `max` 字；單段本身就超過的維持原樣。
fn pack(parts: Vec<String>, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        match out.last_mut() {
            Some(last) if last.chars().count() + p.chars().count() <= max => last.push_str(&p),
            _ => out.push(p),
        }
    }
    out
}

/// 詞表右欄組成熱詞：去掉空白與重複（保留第一次出現的順序），最多
/// [`MAX_HOTWORDS`] 個，以「、」串起。
pub fn hotword_prompt<'a>(terms: impl IntoIterator<Item = &'a str>) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for t in terms.into_iter().map(str::trim) {
        if seen.len() == MAX_HOTWORDS {
            break;
        }
        if !t.is_empty() && !seen.contains(&t) {
            seen.push(t);
        }
    }
    seen.join("、")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_pua_empty_returns_empty() {
        assert_eq!(strip_pua(""), "");
    }

    #[test]
    fn test_strip_pua_only_sentinels_returns_empty() {
        assert_eq!(strip_pua("\u{E000}\u{F8FF}\u{E123}"), "");
    }

    #[test]
    fn test_strip_pua_mixed_keeps_everything_else() {
        assert_eq!(
            strip_pua("今天\u{E001}開會\u{F8FF}，\u{E000}OK。"),
            "今天開會，OK。"
        );
        // 範圍兩端外側的字不能被吃掉
        assert_eq!(strip_pua("\u{D7FF}\u{F900}"), "\u{D7FF}\u{F900}");
    }

    #[test]
    fn test_loop_cut_no_repeat_returns_none() {
        let t: Vec<u32> = (0..100).collect();
        assert_eq!(loop_cut(&t), None);
    }

    #[test]
    fn test_loop_cut_empty_returns_none() {
        assert_eq!(loop_cut::<u32>(&[]), None);
    }

    #[test]
    fn test_loop_cut_unigram_loop_keeps_first_occurrence() {
        // 「好 嗯嗯嗯嗯嗯嗯嗯嗯」→ 保留「好 嗯」
        let mut t = vec![1, 2];
        t.extend([9; 8]);
        assert_eq!(loop_cut(&t), Some(3));
    }

    #[test]
    fn test_loop_cut_fourgram_loop_keeps_first_occurrence() {
        let mut t = vec![7, 7, 1];
        for _ in 0..8 {
            t.extend([1, 2, 3, 4]);
        }
        // 前綴的 1 不屬於這段重複（它後面接的不是 2 3 4 1）
        assert_eq!(loop_cut(&t), Some(3 + 4));
    }

    #[test]
    fn test_loop_cut_repeat_below_threshold_returns_none() {
        let mut t = vec![1];
        t.extend([9; 7]);
        assert_eq!(loop_cut(&t), None);
        let mut t = vec![5];
        for _ in 0..7 {
            t.extend([1, 2]);
        }
        assert_eq!(loop_cut(&t), None);
    }

    #[test]
    fn test_token_cap_zero_seconds_is_the_floor() {
        assert_eq!(token_cap(0.0), 16);
        assert_eq!(token_cap(-1.0), 16);
    }

    #[test]
    fn test_token_cap_half_second_rounds_up() {
        assert_eq!(token_cap(0.5), 20);
        assert_eq!(token_cap(0.01), 17);
    }

    #[test]
    fn test_token_cap_ninety_seconds() {
        assert_eq!(token_cap(90.0), 736);
    }

    #[test]
    fn test_timed_sentences_empty_text_returns_nothing() {
        assert!(timed_sentences("", 0, 10_000).is_empty());
        assert!(timed_sentences("  \n ", 0, 10_000).is_empty());
    }

    #[test]
    fn test_timed_sentences_no_punctuation_is_one_segment_over_the_whole_range() {
        let s = timed_sentences("今天討論預算", 100, 5_100);
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].start_ms, s[0].end_ms), (100, 5_100));
        assert_eq!(s[0].text, "今天討論預算");
    }

    #[test]
    fn test_timed_sentences_one_very_long_clause_is_cut_evenly() {
        let long: String = ('一'..).take(100).collect();
        let s = timed_sentences(&long, 0, 9_000);
        assert_eq!(
            s.iter().map(|x| x.text.chars().count()).collect::<Vec<_>>(),
            [33, 33, 34]
        );
        assert_eq!(s.iter().map(|x| x.text.as_str()).collect::<String>(), long);
        assert_eq!((s[0].start_ms, s[2].end_ms), (0, 9_000));
    }

    #[test]
    fn test_timed_sentences_clause_up_to_twice_the_cap_stays_whole() {
        let clause = "字".repeat(MAX_SENTENCE_CHARS * 2);
        assert_eq!(timed_sentences(&clause, 0, 1_000).len(), 1);
    }

    #[test]
    fn test_timed_sentences_long_sentence_splits_on_commas_within_the_cap() {
        let clause = format!("{}，", "字".repeat(15));
        let text = format!("{}完。", clause.repeat(4));
        let s = timed_sentences(&text, 0, 10_000);
        assert!(s.len() > 1, "{s:?}");
        assert!(s
            .iter()
            .all(|x| x.text.chars().count() <= MAX_SENTENCE_CHARS));
        assert_eq!(s.iter().map(|x| x.text.as_str()).collect::<String>(), text);
    }

    #[test]
    fn test_timed_sentences_timings_are_monotonic_and_cover_the_range() {
        let s = timed_sentences("第一句。第二句比較長一點？好！最後", 2_000, 12_000);
        assert_eq!(s.len(), 4);
        assert_eq!(s[0].start_ms, 2_000);
        assert_eq!(s.last().unwrap().end_ms, 12_000);
        for w in s.windows(2) {
            assert_eq!(w[0].end_ms, w[1].start_ms);
        }
        assert!(s.iter().all(|x| x.start_ms <= x.end_ms));
        // 比例：「第二句比較長一點？」9 字、「好！」2 字
        assert!(s[1].end_ms - s[1].start_ms > s[2].end_ms - s[2].start_ms);
    }

    #[test]
    fn test_timed_sentences_inverted_range_does_not_underflow() {
        let s = timed_sentences("一。二。", 5_000, 1_000);
        assert!(s.iter().all(|x| x.start_ms == 5_000 && x.end_ms == 5_000));
    }

    #[test]
    fn test_hotword_prompt_empty_vocab_is_empty() {
        assert_eq!(hotword_prompt([]), "");
        assert_eq!(hotword_prompt(["", "   "]), "");
    }

    #[test]
    fn test_hotword_prompt_duplicates_are_dropped_in_order() {
        assert_eq!(
            hotword_prompt(["達悟族", "召委", " 達悟族 ", "拼板舟", "召委"]),
            "達悟族、召委、拼板舟"
        );
    }

    #[test]
    fn test_hotword_prompt_caps_at_64_terms() {
        let terms: Vec<String> = (0..100).map(|i| format!("詞{i}")).collect();
        let p = hotword_prompt(terms.iter().map(String::as_str));
        assert_eq!(p.split('、').count(), MAX_HOTWORDS);
        assert!(p.starts_with("詞0、詞1"));
        assert!(p.ends_with("詞63"));
    }

    #[test]
    fn test_load_missing_model_file_is_a_load_error() {
        let Err(e) = Tea::load("/nonexistent/tea.gguf", "/nonexistent/mmproj.gguf", 1) else {
            panic!("不存在的模型不能載入成功");
        };
        assert!(matches!(e, SttError::Load(_)), "{e}");
    }
}
