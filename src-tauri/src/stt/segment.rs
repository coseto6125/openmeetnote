//! 語者切點來源：pyannote segmentation 3.0 的直接 ONNX 推論。
//!
//! 切點與身分是兩個問題。這個模組只回答「哪一毫秒換人說話了」：把批次音訊
//! 過一次分割模型，得到每幀最多三位語者的活動度，再沿主導語者的變化切出
//! 轉場。誰是誰仍然由 [`super::speakers::SpeakerBook`] 的聲紋比對決定 ——
//! 分割模型對同一段音訊每次都給「語者 A、B、C」，跨批次的標籤不連續，
//! 拿它當身分來源等於每十分鐘換一批人。
//!
//! # 為什麼自己跑模型，不走 sherpa-onnx 的 diarization C API
//!
//! 那條路在 Windows 上固定崩潰（0xc0000005 @ 0x7b5c7，預編二進位的問題，
//! BLUEPRINT §18 記錄在案），而且 diarization 管線附帶的分群我們用不到。
//! 這裡只需要「音訊進、活動度出」這一步，用 `ort` 直接跑分割模型反而三個
//! 平台行為一致，崩潰面也小得多。
//!
//! # 模型規格（實測自 bench 的 model.onnx）
//!
//! 輸入 `[N, 1, T]` float32、16 kHz；metadata：`window_size=160000`（10 秒）、
//! `sample_rate=16000`。輸出 `[N, 589, 7]`：589 幀約每幀 17 ms，7 類是
//! powerset 編碼 —— 第 0 類無人說話、第 1 到 3 類單人、第 4 到 6 類兩人重疊。
//! 解碼成三位語者的活動度：某位的活動度是他所有包含他的類別機率之和。
//!
//! # 成本
//!
//! 10 秒窗、1 秒步長。開發機 CPU 實測 120 秒音訊推論 1.29 秒（RTFx ≈ 93），
//! 即時餵進定稿迴圈綽綽有餘。

use std::path::Path;

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;

use super::{Result, SttError};

/// 模型的窗長與輸出幀數。改模型要一起核對 metadata 與這些常數。
const WINDOW_SAMPLES: usize = 160_000;
const FRAMES_PER_WINDOW: usize = 589;
const NUM_CLASSES: usize = 7;
/// 滑窗步長：1 秒。實測 RTFx ≈ 93，再密一倍也只是把即時變成更即時。
const HOP_SAMPLES: usize = 16_000;
/// 一幀多少取樣點。10 秒 / 589 幀 ≈ 271.65，時間換算一律走 f64 不累積誤差。
const SAMPLES_PER_FRAME: f64 = WINDOW_SAMPLES as f64 / FRAMES_PER_WINDOW as f64;
/// 中位數濾波窗（幀）。5 幀 ≈ 85 ms，壓得掉單幀閃爍，也不會吃掉真轉場。
const MEDIAN_K: usize = 5;
/// 活動度高於此值才算在說話。單人發聲的幀通常在 0.9 以上，重疊段兩位都會過線。
const ACTIVE_T: f32 = 0.5;
/// 一個轉場至少要站穩這麼多幀才承認。比這短的翻轉多半是模型抖動：
/// 前後都是同一位時併回去，前後不同人時保留原狀（真的插話）交給上層處理。
const MIN_TURN_FRAMES: usize = 24;

/// 一段同一個人連續說話的區間，時間相對於送進來的音訊。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub start_sample: usize,
    pub end_sample: usize,
    /// 主導語者編號（0 起）。只是分割模型的座位號，不是名單上的名字。
    pub dominant: usize,
}

pub struct Segmenter {
    session: Session,
}

impl Segmenter {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let session = Session::builder()
            .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level3))
            .and_then(|b| b.with_intra_threads(2))
            .and_then(|b| b.commit_from_file(path.as_ref()))
            .map_err(|e| SttError::Load(format!("載入分割模型失敗：{e}")))?;
        Ok(Self { session })
    }

    /// 整段音訊的逐幀活動度：重疊滑窗平均之後再過中位數濾波。
    fn activity(&mut self, samples: &[f32]) -> Result<Vec<[f32; 3]>> {
        // 時間軸以絕對位置對齊，不用窗序號累加：步長不是幀長的整數倍，
        // 用窗序號算基底會讓誤差隨窗數線性漂移。
        let total_frames = (samples.len() as f64 / SAMPLES_PER_FRAME).ceil() as usize + 1;
        let mut acc = vec![[0f32; 3]; total_frames];
        let mut cov = vec![0u32; total_frames];

        // 範圍恆含 0，所以 starts 不為空；不足一窗的短批次也會跑一次補零的窗。
        let mut starts: Vec<usize> = (0..=samples.len().saturating_sub(WINDOW_SAMPLES))
            .step_by(HOP_SAMPLES)
            .collect();
        let last_start = samples.len().saturating_sub(WINDOW_SAMPLES);
        if samples.len() > WINDOW_SAMPLES && *starts.last().unwrap() != last_start {
            starts.push(last_start); // 補上尾端不足一步長的殘窗
        }

        for &s in &starts {
            let mut buf = vec![0f32; WINDOW_SAMPLES];
            let n = WINDOW_SAMPLES.min(samples.len() - s);
            buf[..n].copy_from_slice(&samples[s..s + n]);
            let window = self.infer_window(&buf)?;
            let base = ((s as f64) / SAMPLES_PER_FRAME).round() as i64;
            for (f, a) in window.iter().enumerate() {
                let g = base + f as i64;
                if g >= 0 && (g as usize) < total_frames {
                    let g = g as usize;
                    for (acc_v, a_v) in acc[g].iter_mut().zip(a) {
                        *acc_v += a_v;
                    }
                    cov[g] += 1;
                }
            }
        }

        let mut averaged: Vec<[f32; 3]> = acc
            .into_iter()
            .zip(cov)
            .map(|(a, c)| {
                let c = c.max(1) as f32;
                [a[0] / c, a[1] / c, a[2] / c]
            })
            .collect();
        for t in 0..3 {
            median_filter(&mut averaged, t, MEDIAN_K);
        }
        Ok(averaged)
    }

    /// 一個 10 秒窗的推論與 powerset 解碼。
    fn infer_window(&mut self, window: &[f32]) -> Result<Vec<[f32; 3]>> {
        let input = Tensor::from_array(([1usize, 1, WINDOW_SAMPLES], window.to_vec()))
            .map_err(|e| SttError::Decode(format!("分割模型輸入建構失敗：{e}")))?;
        let outputs = self
            .session
            .run(ort::inputs!["x" => input])
            .map_err(|e| SttError::Decode(format!("分割模型推論失敗：{e}")))?;
        // rc.10 的 try_extract_tensor 回 (形狀, 資料) 兩件東西；形狀是 i64。
        // 形狀檢查不能只放在 debug_assert：release 換錯模型會在切片上炸出
        // 難查的 panic，這裡直接回 Decode 把模型名帶進錯誤訊息。
        let (shape, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| SttError::Decode(format!("分割模型輸出解讀失敗：{e}")))?;
        if shape.len() != 3 || shape[2] != NUM_CLASSES as i64 {
            return Err(SttError::Decode(format!(
                "分割模型輸出形狀不符預期 [N, 幀, {NUM_CLASSES}]，實得 {shape:?}"
            )));
        }

        let frames = shape[1] as usize;
        let mut out = Vec::with_capacity(frames);
        for f in 0..frames {
            let mut row = [0f32; NUM_CLASSES];
            row.copy_from_slice(&data[f * NUM_CLASSES..(f + 1) * NUM_CLASSES]);
            out.push(powerset_to_activity(&row));
        }
        Ok(out)
    }

    /// 音訊進、轉場出。回傳空代表整段沒有人開口。
    pub fn turns(&mut self, samples: &[f32]) -> Result<Vec<Turn>> {
        let activity = self.activity(samples)?;
        let mut turns = turns_from_activity(&activity);
        // 幀到取樣的換算用進位，最後一段可能越過音訊末端；夾住它，
        // 切片越界在定稿迴圈裡是整個行程陪葬等級的事故。
        let end = samples.len();
        for t in &mut turns {
            t.start_sample = t.start_sample.min(end);
            t.end_sample = t.end_sample.min(end);
        }
        turns.retain(|t| t.start_sample < t.end_sample);
        Ok(turns)
    }
}

/// powerset 對數機率 → 三位語者的活動度。
///
/// 類別順序（pyannote powerset-3）：0=∅、1={A}、2={B}、3={C}、
/// 4={A,B}、5={A,C}、6={B,C}。某位的活動度是所有包含他的類別之和，
/// 所以重疊發言會讓兩位同時過門檻，這正是靜音切點永遠做不到的事。
fn powerset_to_activity(logits: &[f32; NUM_CLASSES]) -> [f32; 3] {
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let exps: [f32; NUM_CLASSES] = [
        (logits[0] - max).exp(),
        (logits[1] - max).exp(),
        (logits[2] - max).exp(),
        (logits[3] - max).exp(),
        (logits[4] - max).exp(),
        (logits[5] - max).exp(),
        (logits[6] - max).exp(),
    ];
    let sum: f32 = exps.iter().sum();
    let p = |i: usize| exps[i] / sum;
    [p(1) + p(4) + p(5), p(2) + p(4) + p(6), p(3) + p(5) + p(6)]
}

/// 對活動度的第 t 條軌做奇數窗中位數濾波，邊緣複製最外側值。
fn median_filter(seq: &mut [[f32; 3]], track: usize, k: usize) {
    let k = k.max(1) | 1; // 強制奇數
    let half = k / 2;
    let original: Vec<f32> = seq.iter().map(|a| a[track]).collect();
    for (i, a) in seq.iter_mut().enumerate() {
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(original.len());
        let mut win: Vec<f32> = original[lo..hi].to_vec();
        win.sort_by(f32::total_cmp);
        a[track] = win[win.len() / 2];
    }
}

/// 從逐幀活動度切出轉場。
///
/// 先標每幀的主導語者與有聲判定，再把短於 [`MIN_TURN_FRAMES`] 的翻轉收掉：
/// 只收「前後是同一位」的（那是模型抖動）；前後不同人的短翻轉可能是真的
/// 插話，寧可留著讓上層按片段長度決定歸屬，也不把它併進隔壁錯認的人。
fn turns_from_activity(activity: &[[f32; 3]]) -> Vec<Turn> {
    let dom: Vec<Option<usize>> = activity
        .iter()
        .map(|a| {
            let (i, v) = a
                .iter()
                .enumerate()
                .max_by(|x, y| x.1.total_cmp(y.1))
                .unwrap_or((0, &0.0));
            (*v >= ACTIVE_T).then_some(i)
        })
        .collect();

    // 連續同一位的有聲幀先收成原始段：(起幀、迄幀不含、語者)
    let mut raw: Vec<(usize, usize, usize)> = Vec::new();
    for (f, d) in dom.iter().enumerate() {
        match (d, raw.last_mut()) {
            (Some(i), Some((_, e, s))) if *s == *i && *e == f => *e = f + 1,
            (Some(i), _) => raw.push((f, f + 1, *i)),
            _ => {}
        }
    }

    // 抖動收編：短段夾在兩個同語者的段中間才併，併完順手收攏相鄰同語者的段。
    let mut turns: Vec<(usize, usize, usize)> = Vec::new();
    for (idx, (s, e, spk)) in raw.iter().enumerate() {
        let short = e - s < MIN_TURN_FRAMES;
        let sandwiched =
            idx > 0 && idx + 1 < raw.len() && short && raw[idx - 1].2 == raw[idx + 1].2;
        if sandwiched {
            if let Some(last) = turns.last_mut() {
                if last.2 == raw[idx - 1].2 {
                    last.1 = raw[idx + 1].0; // 先延伸到下一段的起點，下一段稍後自然合併
                    continue;
                }
            }
        }
        if let Some(last) = turns.last_mut() {
            if last.2 == *spk && last.1 == *s {
                last.1 = *e;
                continue;
            }
        }
        turns.push((*s, *e, *spk));
    }

    turns
        .into_iter()
        .map(|(sf, ef, spk)| Turn {
            start_sample: (sf as f64 * SAMPLES_PER_FRAME).floor() as usize,
            end_sample: (ef as f64 * SAMPLES_PER_FRAME).ceil() as usize,
            dominant: spk,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一個類別的獨熱 logits，softmax 後該類機率為 1。
    fn one_hot(cls: usize) -> [f32; NUM_CLASSES] {
        let mut l = [0f32; NUM_CLASSES];
        l[cls] = 10.0;
        l
    }

    #[test]
    fn test_silence_decodes_to_no_speaker() {
        let a = powerset_to_activity(&one_hot(0));
        assert!(a.iter().all(|&v| v < 1e-3), "{a:?}");
    }

    #[test]
    fn test_single_speaker_class_activates_only_that_speaker() {
        let a = powerset_to_activity(&one_hot(1)); // {A}
        assert!(a[0] > 0.999 && a[1] < 1e-3 && a[2] < 1e-3, "{a:?}");
    }

    #[test]
    fn test_overlap_class_activates_both_speakers() {
        let a = powerset_to_activity(&one_hot(4)); // {A,B}
        assert!(a[0] > 0.999 && a[1] > 0.999 && a[2] < 1e-3, "{a:?}");
    }

    #[test]
    fn test_a_flicker_between_the_same_speaker_is_absorbed() {
        // A 說 100 幀、B 抖了 10 幀、A 繼續 100 幀 → 只剩一段 A。
        // 抖動夾在同一位中間是模型雜訊，併回去；切成三段反而製造兩個假切點。
        let mut act = vec![[1.0, 0.0, 0.0]; 100];
        act.extend(vec![[0.0, 1.0, 0.0]; 10]);
        act.extend(vec![[1.0, 0.0, 0.0]; 100]);
        let turns = turns_from_activity(&act);
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert_eq!(turns[0].dominant, 0);
        assert_eq!(turns[0].start_sample, 0);
        assert_eq!(
            turns[0].end_sample,
            (210f64 * SAMPLES_PER_FRAME).ceil() as usize
        );
    }

    #[test]
    fn test_a_short_interjection_between_two_speakers_survives() {
        // A 說完、B 短短插一句、C 接手。前後不同人的短翻轉不是雜訊，
        // 併進隔壁等於把話安到錯的人頭上，所以保留原狀。
        let mut act = vec![[1.0, 0.0, 0.0]; 100];
        act.extend(vec![[0.0, 1.0, 0.0]; 10]);
        act.extend(vec![[0.0, 0.0, 1.0]; 100]);
        let turns = turns_from_activity(&act);
        assert_eq!(turns.len(), 3, "{turns:?}");
        assert_eq!(
            turns.iter().map(|t| t.dominant).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn test_silence_splits_turns_and_is_not_included() {
        let mut act = vec![[1.0, 0.0, 0.0]; 100];
        act.extend(vec![[0.0; 3]; 50]); // 全靜音
        act.extend(vec![[0.0, 1.0, 0.0]; 100]);
        let turns = turns_from_activity(&act);
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[0].dominant, 0);
        assert_eq!(turns[1].dominant, 1);
        // 兩段互不重疊，而且都落在靜音帶的邊界上（幀到取樣的捨入容許 ±1）。
        // 靜音帶本身（100–150 幀）不屬於任何一段。
        let gap_lo = (100f64 * SAMPLES_PER_FRAME).floor() as i64;
        let gap_hi = (150f64 * SAMPLES_PER_FRAME).ceil() as i64;
        assert!(
            (turns[0].end_sample as i64).abs_diff(gap_lo) <= 1,
            "{turns:?}"
        );
        assert!(
            (turns[1].start_sample as i64).abs_diff(gap_hi) <= 1,
            "{turns:?}"
        );
        assert!(turns[0].end_sample <= turns[1].start_sample + 1);
    }

    #[test]
    fn test_median_filter_kills_a_single_frame_spike() {
        let mut act = vec![[0.0, 0.0, 0.0]; 3];
        act.push([9.0, 0.0, 0.0]);
        act.push([0.0, 0.0, 0.0]);
        median_filter(&mut act, 0, MEDIAN_K);
        assert!(act.iter().all(|a| a[0] == 0.0), "{act:?}");
    }

    #[test]
    fn test_median_filter_keeps_a_sustained_level() {
        let mut act = vec![[0.0, 0.0, 0.0]; 2];
        act.extend(vec![[1.0, 0.0, 0.0]; 10]);
        median_filter(&mut act, 0, MEDIAN_K);
        // 中段維持 1.0，只有前兩幀被邊緣效應拉低
        assert!(act.iter().skip(4).all(|a| a[0] == 1.0), "{act:?}");
    }
}
