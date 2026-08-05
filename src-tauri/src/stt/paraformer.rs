//! Paraformer 即時稿引擎（sherpa-onnx）。
//!
//! RTF 約 0.012，比 whisper turbo 快二十餘倍，代價是關鍵詞命中 7/12 對 9/12。
//! 它負責錄音期間看得到字，正確性由後續的 whisper 定稿與分歧標記補上。

use sherpa_rs::paraformer::{ParaformerConfig, ParaformerRecognizer};

use super::{Result, Segment, SttError, Token};

pub struct Paraformer {
    rec: ParaformerRecognizer,
}

impl Paraformer {
    /// `model_dir` 需含 `model.int8.onnx` 與 `tokens.txt`。
    ///
    /// 用 int8 而非 fp32：實測兩者品質相同，記憶體差三倍（587 MB 對 1.95 GB）。
    pub fn load(model_dir: &str, threads: i32) -> Result<Self> {
        let rec = ParaformerRecognizer::new(ParaformerConfig {
            model: format!("{model_dir}/model.int8.onnx"),
            tokens: format!("{model_dir}/tokens.txt"),
            num_threads: Some(threads),
            ..Default::default()
        })
        .map_err(|e| SttError::Load(e.to_string()))?;
        Ok(Self { rec })
    }

    /// 帶時間點的 token 串。比對階段要的是這個，不是合併後的段。
    pub fn tokens(&mut self, samples: &[f32]) -> Vec<Token> {
        let r = self.rec.transcribe(16_000, samples);
        r.tokens
            .iter()
            .zip(r.timestamps.iter())
            .map(|(text, t)| Token {
                at_ms: (t * 1000.0) as u64,
                text: text.clone(),
            })
            .collect()
    }

    /// 供即時稿顯示：token 間隔超過 `gap_ms` 就換一段。
    ///
    /// Paraformer 自己不斷句，而未斷句的長串在 UI 上無法閱讀，也無法對應到
    /// 逐字稿的片段身分。
    pub fn segments(&mut self, samples: &[f32], gap_ms: u64) -> Vec<Segment> {
        let mut out: Vec<Segment> = Vec::new();
        for t in self.tokens(samples) {
            match out.last_mut() {
                Some(seg) if t.at_ms.saturating_sub(seg.end_ms) <= gap_ms => {
                    seg.text.push_str(&t.text);
                    seg.end_ms = t.at_ms;
                }
                _ => out.push(Segment {
                    no_speech: 0.0,
                    start_ms: t.at_ms,
                    end_ms: t.at_ms,
                    text: t.text,
                }),
            }
        }
        out
    }
}
