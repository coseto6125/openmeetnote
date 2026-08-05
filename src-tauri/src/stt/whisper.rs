//! whisper.cpp 定稿引擎。
//!
//! 實測 large-v3-turbo-q5 在 12 個台灣會議關鍵詞上命中 9 個，是測過的本機方案裡
//! 最準的（比較與已排除選項見 BLUEPRINT.md §5.3.1）。

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::{Result, Segment, SttError};

pub struct Whisper {
    ctx: WhisperContext,
    threads: i32,
}

impl Whisper {
    pub fn load(model_path: &str, threads: i32) -> Result<Self> {
        let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
            .map_err(|e| SttError::Load(e.to_string()))?;
        Ok(Self { ctx, threads })
    }

    /// 轉錄整段音訊，回傳帶時間區間的片段。
    ///
    /// 一律帶時間戳。`set_no_timestamps(true)` 會抑制時間戳 token 並改變解碼路徑，
    /// 微調模型在該模式下會提前輸出 EOT 而截斷（實測 Belle-zh 少掉三分之一內容）；
    /// 何況引用驗證本來就需要時間戳定位，關掉它沒有任何好處。
    pub fn transcribe(&self, samples: &[f32]) -> Result<Vec<Segment>> {
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| SttError::Decode(e.to_string()))?;

        // 與 whisper.cpp CLI 的預設一致。greedy best_of=1 會明顯掉品質：
        // 同一個模型、同一段音訊，greedy 轉出「達物族」，beam search 轉出「達悟族」。
        let mut p = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        });
        p.set_n_threads(self.threads);
        p.set_language(Some("zh"));
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        state
            .full(p, samples)
            .map_err(|e| SttError::Decode(e.to_string()))?;

        (0..state.full_n_segments())
            .map(|i| {
                let seg = state
                    .get_segment(i)
                    .ok_or_else(|| SttError::Decode(format!("取不到第 {i} 段")))?;
                Ok(Segment {
                    // whisper 的時間單位是 centisecond
                    start_ms: seg.start_timestamp() as u64 * 10,
                    end_ms: seg.end_timestamp() as u64 * 10,
                    no_speech: seg.no_speech_probability(),
                    text: seg
                        .to_str()
                        .map_err(|e| SttError::Decode(e.to_string()))?
                        .trim()
                        .to_owned(),
                })
            })
            .collect()
    }
}
