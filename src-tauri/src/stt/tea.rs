//! TEA-ASR 定稿引擎。
//!
//! JacobLinCool/TEA-ASR-1.1 是 Qwen3-ASR 1.7B 針對台灣華語的微調，這裡透過
//! llama.cpp 的多模態（mtmd）音訊路徑在行程內執行：文字解碼器
//! `TEA-ASR-1.1.Q4_K_M.gguf` 加音訊編碼器 `TEA-ASR-1.1.mmproj-Q8_0.gguf`。
//! Q4_K_M 與 Q8_0 準確度相同而更快，CPU 四執行緒實測 RTF 約 0.1 到 0.15。
//!
//! 模型不給時間戳。每句的時間是按字數比例分配出來的近似值：定稿路徑分配在
//! 每段有聲音的範圍內（見 `live::transcribe_runs`），[`Tea::transcribe`] 分配在整段上
//! （見 [`timed_sentences`]），要精確的時間得另外接強制對齊。

use std::num::NonZeroU32;
use std::sync::OnceLock;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::mtmd::{
    mtmd_default_marker, MtmdBitmap, MtmdContext, MtmdContextParams, MtmdInputChunks, MtmdInputText,
};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use llama_cpp_2::token_type::LlamaTokenAttr;
use llama_cpp_2::{send_logs_to_tracing, LogOptions, TokenToStringError};

use super::{Result, Segment, SttError};

/// 模型吃的取樣率。擷取端已經是 16 kHz 單聲道，這裡只用來換算長度。
const SAMPLE_RATE: f64 = 16_000.0;

/// 送進模型的熱詞上限。實測 30 個與這場無關的詞也不傷準確度，64 是給
/// 長期累積的詞表留的餘裕，同時讓 prompt 長度有界。
const MAX_HOTWORDS: usize = 64;

/// 單一熱詞的字數上限。超過的整個丟掉而不是截斷：截一半的詞是錯的提示。
const MAX_HOTWORD_CHARS: usize = 32;

/// 串起來的熱詞文字的位元組上限。超過預算的詞整個丟掉。
const MAX_HOTWORD_BYTES: usize = 1024;

/// 尾端同一個 n-gram 連續出現幾次算失控。
const LOOP_REPEATS: usize = 8;

/// 偵測失控時看的 n-gram 長度上限（token 數）。
const LOOP_MAX_N: usize = 4;

/// 失控重解時，重複懲罰回看的 token 數與懲罰倍率。
///
/// 只在第一次解碼已經失控時才用：正常的批次維持純 greedy，結果不受影響。
/// 1.15 是常見的溫和值，足以把解碼器推出「嗯嗯嗯…」的吸引子，又不至於
/// 懲罰到正常的重複用字（「對對對」這種三次以內的重複）。
const PENALTY_LAST_N: i32 = 64;
const PENALTY_REPEAT: f32 = 1.15;

/// prompt 與音訊 embedding 每次送進解碼器的 token 數，也是 ubatch 大小。
/// 與 llama.cpp 的預設 ubatch 相同；調大只會讓計算緩衝區變大，
/// Qwen3-ASR 的音訊段是因果注意力，不需要整段落在同一個 ubatch。
const EVAL_BATCH: u32 = 512;

/// token 轉文字的第一次緩衝區大小。放不下時 llama.cpp 回報實際需要的長度，
/// 照那個長度再轉一次。
const PIECE_BUF: usize = 64;

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

fn decode_err(e: impl std::fmt::Display) -> SttError {
    SttError::Decode(e.to_string())
}

/// 把幾段音訊轉成文字的引擎。
///
/// 抽成 trait 是為了測試：`live::transcribe_runs` 的切段、失敗隔離、時間分配
/// 與語者歸屬都跟模型無關，用回傳腳本文字的假引擎就能驗證。
pub trait RunEngine {
    /// 依序轉錄每一段音訊，結果與 `runs` 一一對應。
    ///
    /// 每段各自成敗：一段失敗不連累同一批的其他段。
    fn texts(&self, runs: &[&[f32]], hotwords: &str) -> Vec<Result<String>>;
}

pub struct Tea {
    model: LlamaModel,
    mtmd: MtmdContext,
    threads: i32,
}

/// 一次解碼的結果。
struct Decoded {
    tokens: Vec<LlamaToken>,
    /// 偵測到失控時的 token 數（截斷前）；沒失控是 `None`。
    looped: Option<usize>,
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
        // 預設的 n_gpu_layers 是 -1（全部卸載）。Apple 的組建有開 Metal，
        // 不指定的話解碼器在 macOS 上會跑 GPU，而音訊編碼器（use_gpu: false）
        // 跑 CPU。引擎在每個平台都是純 CPU，量過的 RTF 才適用。
        let model_params = LlamaModelParams::default().with_n_gpu_layers(0);
        let model = LlamaModel::load_from_file(backend, model_path, &model_params)
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
        self.texts(&[samples], hotwords)
            .pop()
            .unwrap_or_else(|| Ok(String::new()))
    }

    /// 把一段音訊與 prompt 轉成模型的輸入；空音訊回 `None`。
    fn prepare(&self, samples: &[f32], hotwords: &str) -> Result<Option<(MtmdInputChunks, usize)>> {
        if samples.is_empty() {
            return Ok(None);
        }
        let bitmap = MtmdBitmap::from_audio_data(samples).map_err(decode_err)?;
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
            .map_err(decode_err)?;
        Ok(Some((
            chunks,
            token_cap(samples.len() as f64 / SAMPLE_RATE),
        )))
    }

    /// 開一個裝得下 `n_ctx` 個 token 的 context。
    fn context(&self, n_ctx: usize) -> Result<LlamaContext<'_>> {
        let n_ctx = u32::try_from(n_ctx).map_err(decode_err)?;
        let params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(n_ctx))
            .with_n_batch(EVAL_BATCH)
            .with_n_ubatch(EVAL_BATCH)
            .with_n_threads(self.threads)
            .with_n_threads_batch(self.threads);
        self.model
            .new_context(backend()?, params)
            .map_err(decode_err)
    }

    /// 從乾淨的 KV cache 開始解一段。遇到 EOG 或任何控制 token 就停。
    fn generate(
        &self,
        ctx: &mut LlamaContext<'_>,
        chunks: &MtmdInputChunks,
        cap: usize,
        mut sampler: LlamaSampler,
    ) -> Result<Decoded> {
        ctx.clear_kv_cache();
        let mut n_past = chunks
            .eval_chunks(&self.mtmd, ctx, 0, 0, EVAL_BATCH as i32, true)
            .map_err(decode_err)?;
        let mut batch = LlamaBatch::new(1, 1);
        let mut tokens = Vec::with_capacity(cap);
        let mut looped = None;
        while tokens.len() < cap {
            let token = sampler.sample(ctx, -1);
            // 控制 token（<|im_end|> 以外的 <|...|>、<asr_text> 等）不是轉錄
            // 內容，轉成文字也會失敗。模型吐出它代表這段已經說完了。
            if self.model.is_eog_token(token)
                || self
                    .model
                    .token_attr(token)
                    .contains(LlamaTokenAttr::Control)
            {
                break;
            }
            tokens.push(token);
            if let Some(keep) = loop_cut(&tokens) {
                looped = Some(tokens.len());
                tokens.truncate(keep);
                break;
            }
            batch.clear();
            batch.add(token, n_past, &[0], true).map_err(decode_err)?;
            n_past += 1;
            ctx.decode(&mut batch).map_err(decode_err)?;
        }
        Ok(Decoded { tokens, looped })
    }

    /// 解一段；第一次失控就加重複懲罰重解一次。
    fn decode_run(
        &self,
        ctx: &mut LlamaContext<'_>,
        chunks: &MtmdInputChunks,
        cap: usize,
    ) -> Result<String> {
        let first = self.generate(ctx, chunks, cap, LlamaSampler::greedy())?;
        let Some(n) = first.looped else {
            if first.tokens.len() == cap {
                super::live::log(&format!("定稿達到 token 上限 {cap}，後面可能被截掉"));
            }
            return self.render(&first.tokens);
        };
        // 失控截斷會悄悄吃掉合法的重複之後的內容（「對對對對…」之後那句
        // 正事）。加懲罰重解一次；重解不再失控就用重解的結果。
        let penalized = LlamaSampler::chain_simple([
            LlamaSampler::penalties(
                self.model.n_vocab(),
                PENALTY_LAST_N,
                PENALTY_REPEAT,
                0.0,
                0.0,
            ),
            LlamaSampler::greedy(),
        ]);
        match self.generate(ctx, chunks, cap, penalized) {
            Ok(retry) if retry.looped.is_none() => {
                let text = self.render(&retry.tokens)?;
                super::live::log(&format!(
                    "定稿重複迴圈：第 {n} 個 token 偵測到，加重複懲罰重解成功 → {text:?}"
                ));
                Ok(text)
            }
            other => {
                let text = self.render(&first.tokens)?;
                let why = match other {
                    Ok(_) => "重解仍失控".to_owned(),
                    Err(e) => format!("重解失敗：{e}"),
                };
                super::live::log(&format!(
                    "定稿重複迴圈：第 {n} 個 token 偵測到，{why}，截回 {} 個 token → {text:?}",
                    first.tokens.len()
                ));
                Ok(text)
            }
        }
    }

    /// token 轉成文字。
    ///
    /// 一個 token 不一定是完整的 UTF-8 字元，所以整串 bytes 接好才解碼。
    /// 超過緩衝區的片段照 llama.cpp 回報的長度重轉；轉不出東西的 token
    /// （llama.cpp 回 0 個位元組）不帶任何文字，跳過。
    fn render(&self, tokens: &[LlamaToken]) -> Result<String> {
        let mut bytes = Vec::new();
        for &t in tokens {
            let piece = match self.model.token_to_piece_bytes(t, PIECE_BUF, false, None) {
                Err(TokenToStringError::InsufficientBufferSpace(need)) => self
                    .model
                    .token_to_piece_bytes(t, need.unsigned_abs() as usize, false, None),
                other => other,
            };
            match piece {
                Ok(b) => bytes.extend(b),
                Err(TokenToStringError::UnknownTokenType) => {}
                Err(e) => return Err(decode_err(e)),
            }
        }
        Ok(strip_pua(&String::from_utf8_lossy(&bytes))
            .trim()
            .to_owned())
    }
}

impl RunEngine for Tea {
    /// 同一批的各段共用一個 context，段與段之間清空 KV cache。
    ///
    /// context 借用 model，長駐在 `Tea` 裡會變成自我參照的結構，所以每批
    /// 開一個。大小取這批最長那段所需，先把每段 tokenize 完才知道。
    fn texts(&self, runs: &[&[f32]], hotwords: &str) -> Vec<Result<String>> {
        let prepared: Vec<Result<Option<(MtmdInputChunks, usize)>>> =
            runs.iter().map(|s| self.prepare(s, hotwords)).collect();
        let need = prepared
            .iter()
            .filter_map(|p| p.as_ref().ok()?.as_ref())
            .map(|(chunks, cap)| chunks.total_tokens() + cap + 16)
            .max();
        let mut ctx = match need.map(|n| self.context(n)).transpose() {
            Ok(ctx) => ctx,
            Err(e) => {
                let msg = e.to_string();
                return prepared
                    .into_iter()
                    .map(|p| match p? {
                        Some(_) => Err(SttError::Decode(msg.clone())),
                        None => Ok(String::new()),
                    })
                    .collect();
            }
        };
        prepared
            .into_iter()
            .map(|p| match (p?, ctx.as_mut()) {
                (None, _) => Ok(String::new()),
                (Some((chunks, cap)), Some(ctx)) => self.decode_run(ctx, &chunks, cap),
                // need 是 Some 時 ctx 一定開好了；走到這裡代表上面的邏輯改壞了
                (Some(_), None) => Err(SttError::Decode("沒有可用的 context".into())),
            })
            .collect()
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
    let text = collapse_stacked_stops(text);
    let sentences: Vec<String> = merge_bare_marks(split_on(&text, &['。', '？', '！', '?', '!']))
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

/// 「！。」「?。」這類疊在一起的句末標點只留第一個。
///
/// sherpa 的 CT-transformer 標點模型會在已經以「！？!?」結尾的文字後面再補
/// 一個「。」：「太好了！」→「太好了！。」。不收掉的話那個「。」會被切成
/// 自己一行定稿。
fn collapse_stacked_stops(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev = None;
    for c in text.chars() {
        if c == '。' && matches!(prev, Some('！' | '!' | '?' | '？')) {
            continue;
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// 沒有任何字母、數字或漢字的片段（只有標點）併進前一段。
///
/// 開頭就是純標點的片段沒有前一段可併，直接丟掉：一行只有「。」的定稿
/// 對誰都沒有意義。
fn merge_bare_marks(pieces: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(pieces.len());
    for p in pieces {
        if p.chars().any(char::is_alphanumeric) {
            out.push(p);
        } else if let Some(last) = out.last_mut() {
            last.push_str(&p);
        }
    }
    out
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
///
/// prompt 是以 `parse_special: true` tokenize 的，詞表內容會原樣進到控制
/// token 的解析：含 `<|` 或 `|>` 的詞可能變成控制 token，含媒體標記的詞會
/// 多出第二個音訊位置。這兩種詞整個丟掉。單詞超過 [`MAX_HOTWORD_CHARS`] 字
/// 的也丟掉，串起來超過 [`MAX_HOTWORD_BYTES`] 位元組之後的詞不再加入，
/// prompt 長度因此有界。
pub fn hotword_prompt<'a>(terms: impl IntoIterator<Item = &'a str>) -> String {
    let marker = mtmd_default_marker();
    let mut seen: Vec<&str> = Vec::new();
    let mut bytes = 0;
    for t in terms.into_iter().map(str::trim) {
        if seen.len() == MAX_HOTWORDS {
            break;
        }
        let unsafe_term = t.contains("<|") || t.contains("|>") || t.contains(marker);
        if t.is_empty() || unsafe_term || t.chars().count() > MAX_HOTWORD_CHARS || seen.contains(&t)
        {
            continue;
        }
        // 分隔的「、」也算在預算裡
        let cost = t.len() + if seen.is_empty() { 0 } else { '、'.len_utf8() };
        if bytes + cost > MAX_HOTWORD_BYTES {
            break;
        }
        bytes += cost;
        seen.push(t);
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
    fn test_timed_sentences_stop_after_exclamation_stays_one_segment() {
        // 標點模型把「太好了！」補成「太好了！。」，那個「。」不能自成一行
        let s = timed_sentences("太好了！。", 0, 1_000);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].text, "太好了！");
        assert_eq!((s[0].start_ms, s[0].end_ms), (0, 1_000));
    }

    #[test]
    fn test_timed_sentences_stacked_stops_collapse_for_every_mark() {
        let s = timed_sentences("真的嗎？。好啊!。OK?。", 0, 3_000);
        assert_eq!(
            s.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(),
            ["真的嗎？", "好啊!", "OK?"]
        );
    }

    #[test]
    fn test_timed_sentences_bare_mark_piece_merges_into_previous() {
        // 標點之間只有空白：split 後是一段純標點，要併進前一句
        let s = timed_sentences("好。 。對。", 0, 2_000);
        assert_eq!(
            s.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(),
            ["好。。", "對。"]
        );
    }

    #[test]
    fn test_timed_sentences_only_marks_returns_nothing() {
        assert!(timed_sentences("。", 0, 1_000).is_empty());
        assert!(timed_sentences("！。？", 0, 1_000).is_empty());
    }

    #[test]
    fn test_timed_sentences_digit_only_sentence_is_kept() {
        let s = timed_sentences("好。3。", 0, 1_000);
        assert_eq!(s.len(), 2, "{s:?}");
    }

    #[test]
    fn test_hotword_prompt_control_token_terms_are_dropped() {
        assert_eq!(
            hotword_prompt(["達悟族", "<|im_end|>", "a|>b", "x<|y", "召委"]),
            "達悟族、召委"
        );
    }

    #[test]
    fn test_hotword_prompt_media_marker_term_is_dropped() {
        let bad = format!("詞{}詞", mtmd_default_marker());
        assert_eq!(hotword_prompt(["達悟族", bad.as_str()]), "達悟族");
    }

    #[test]
    fn test_hotword_prompt_overlong_term_is_dropped() {
        let long = "字".repeat(MAX_HOTWORD_CHARS + 1);
        let edge = "字".repeat(MAX_HOTWORD_CHARS);
        assert_eq!(
            hotword_prompt([long.as_str(), edge.as_str()]),
            edge.as_str()
        );
    }

    #[test]
    fn test_hotword_prompt_stops_at_the_byte_budget() {
        // 每個詞 30 字 × 3 位元組 = 90 位元組，加分隔符 93：第 12 個詞超過 1024
        let terms: Vec<String> = (0..20)
            .map(|i| format!("{}{i:02}", "字".repeat(28)))
            .collect();
        let p = hotword_prompt(terms.iter().map(String::as_str));
        assert!(p.len() <= MAX_HOTWORD_BYTES, "{}", p.len());
        let n = p.split('、').count();
        assert!(n < 20 && n > 1, "{n}");
        assert!(p.starts_with(terms[0].as_str()));
    }

    #[test]
    fn test_load_missing_model_file_is_a_load_error() {
        let Err(e) = Tea::load("/nonexistent/tea.gguf", "/nonexistent/mmproj.gguf", 1) else {
            panic!("不存在的模型不能載入成功");
        };
        assert!(matches!(e, SttError::Load(_)), "{e}");
    }

    #[test]
    fn test_load_file_that_is_not_gguf_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("broken.gguf");
        std::fs::write(&bad, b"not a gguf file").unwrap();
        let bad = bad.to_str().unwrap();
        let Err(e) = Tea::load(bad, bad, 1) else {
            panic!("壞掉的模型檔不能載入成功");
        };
        assert!(matches!(e, SttError::Load(_)), "{e}");
    }

    #[test]
    fn test_backend_initialized_twice_is_the_same_instance() {
        // llama.cpp 的 backend 第二次初始化會回錯；收尾與測試都會載兩次模型
        let a = backend().expect("第一次") as *const LlamaBackend;
        let b = backend().expect("第二次") as *const LlamaBackend;
        assert_eq!(a, b);
    }
}
