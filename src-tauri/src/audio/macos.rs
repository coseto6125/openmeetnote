//! macOS 雙軌擷取：系統音訊走 ScreenCaptureKit，麥克風走 CoreAudio（cpal）。
//!
//! 兩個來源而不是一個，理由與 Windows 那邊相同：`track` 是「本機 vs 遠端」的
//! 先驗，混掉之後就再也分不出來。
//!
//! 為什麼不是同一個 API 拿兩軌：ScreenCaptureKit 從 macOS 15 才能擷取麥克風
//! （`microphoneCaptureEnabled`）。綁在 15 上等於讓 13 與 14 的使用者錄不到
//! 自己的聲音，而那是這個產品的一半。cpal 走 CoreAudio，每個版本都有。
//!
//! 兩條路徑對取樣率的處理是相反的，這一點值得寫下來：
//!
//! - ScreenCaptureKit 可以直接指定 16 kHz 單聲道，錄下來就是要的格式。
//! - CoreAudio 的輸入裝置給的是它自己的原生格式（內建麥克風多半是 48 kHz），
//!   因此麥克風那一路要降取樣。用 `rubato` 而不是自己抽樣：直接每三個取一個
//!   會把 8 kHz 以上的內容摺回可聽範圍，而那正是齒音與子音所在的位置。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::audioadapter_buffers::direct::SequentialSlice;
use rubato::{Fft, FixedSync, Resampler};
use screencapturekit::prelude::*;

use super::{AudioCapture, AudioError, Chunk, SAMPLE_RATE};
use crate::model::Track;

/// 每批送出的樣本數。與 Windows 一致的 100 ms。
const CHUNK_FRAMES: usize = SAMPLE_RATE as usize / 10;

/// 停止旗標的輪詢間隔。擷取本身是回呼驅動的，這條執行緒只負責等停止。
const POLL: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Default)]
pub struct MacCapture {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Result<(), String>>>,
}

impl AudioCapture for MacCapture {
    fn start(&mut self) -> Result<Receiver<Chunk>, AudioError> {
        let (tx, rx) = channel();
        self.stop = Arc::new(AtomicBool::new(false));

        // 兩軌各自一條執行緒。任何一軌開不起來只影響那一軌 —— 沒有螢幕錄製
        // 權限時麥克風仍該錄得到，反過來也是。哪一軌死了會寫進 stt.log，
        // 畫面上的音量條也會停住，那是使用者判斷「有沒有在收音」的依據。
        let (t, s) = (tx.clone(), self.stop.clone());
        self.threads
            .push(std::thread::spawn(move || system_audio(t, s)));
        let s = self.stop.clone();
        self.threads
            .push(std::thread::spawn(move || microphone(tx, s)));
        Ok(rx)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.threads.drain(..) {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => crate::stt::live::log(&format!("音訊擷取結束於錯誤：{e}")),
                Err(_) => crate::stt::live::log("音訊擷取執行緒異常結束"),
            }
        }
    }
}

/* ── 送出：兩條路徑共用的分批邏輯 ──────────────────────────────── */

/// 累積到 100 ms 才送一批，並維護該軌的擷取時間軸。
///
/// 時間軸用「送出去的樣本數」算，不用系統時鐘：暫停與丟幀都不該讓引用
/// 定位到錯的地方，而樣本數是唯一與音訊內容嚴格對應的量。
struct Batcher {
    track: Track,
    tx: Sender<Chunk>,
    pending: Vec<f32>,
    sent_frames: u64,
}

impl Batcher {
    fn new(track: Track, tx: Sender<Chunk>) -> Self {
        Self {
            track,
            tx,
            pending: Vec::with_capacity(CHUNK_FRAMES * 2),
            sent_frames: 0,
        }
    }

    /// 回傳 false 代表接收端已關閉，呼叫端該收工。那不是錯誤，是會議結束。
    fn push(&mut self, samples: &[f32]) -> bool {
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= CHUNK_FRAMES {
            let rest = self.pending.split_off(CHUNK_FRAMES);
            let samples = std::mem::replace(&mut self.pending, rest);
            let chunk = Chunk {
                track: self.track,
                captured_start_ms: self.sent_frames * 1000 / SAMPLE_RATE as u64,
                samples,
            };
            self.sent_frames += CHUNK_FRAMES as u64;
            if self.tx.send(chunk).is_err() {
                return false;
            }
        }
        true
    }
}

/* ── 系統音訊：ScreenCaptureKit ────────────────────────────────── */

/// SCK 的回呼在它自己的執行緒上跑，因此分批狀態要能跨執行緒共享。
struct SystemAudioSink {
    batcher: Mutex<Batcher>,
}

impl SCStreamOutputTrait for SystemAudioSink {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio {
            return;
        }
        let Some(list) = sample.audio_buffer_list() else {
            return;
        };
        let Ok(mut batcher) = self.batcher.lock() else {
            return;
        };
        for buffer in list.iter() {
            let bytes = buffer.data();
            // 設定要的是 32-bit float 單聲道；不是四的倍數代表格式與預期不符，
            // 硬解會產生噪音，寧可丟掉這一批並讓音量條停住
            if bytes.len() % 4 != 0 {
                continue;
            }
            let samples: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            if !batcher.push(&samples) {
                return;
            }
        }
    }
}

fn system_audio(tx: Sender<Chunk>, stop: Arc<AtomicBool>) -> Result<(), String> {
    let content = SCShareableContent::get()
        .map_err(|e| format!("取得可擷取內容失敗（多半是沒有螢幕錄製權限）：{e}"))?;
    let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or("找不到任何顯示器，無法擷取系統音訊")?;

    // 擷取整個顯示器的音訊。畫面內容不使用，但 SCK 的音訊是掛在內容篩選器上的，
    // 沒有篩選器就沒有串流。
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build();

    // 直接要 16 kHz 單聲道，錄下來就是轉錄要的格式，省掉一次重取樣。
    let config = SCStreamConfiguration::new()
        .with_captures_audio(true)
        .with_sample_rate(SAMPLE_RATE as i32)
        .with_channel_count(1);

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(
        SystemAudioSink {
            batcher: Mutex::new(Batcher::new(Track::System, tx)),
        },
        SCStreamOutputType::Audio,
    );
    stream
        .start_capture()
        .map_err(|e| format!("啟動系統音訊擷取失敗：{e}"))?;
    crate::stt::live::log("已開啟 system 軌（ScreenCaptureKit）");

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
    }
    stream
        .stop_capture()
        .map_err(|e| format!("停止系統音訊擷取失敗：{e}"))?;
    Ok(())
}

/* ── 麥克風：CoreAudio（cpal） ─────────────────────────────────── */

fn microphone(tx: Sender<Chunk>, stop: Arc<AtomicBool>) -> Result<(), String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("找不到預設麥克風")?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("讀取麥克風格式失敗：{e}"))?;
    let sample_format = supported.sample_format();
    if sample_format != cpal::SampleFormat::F32 {
        // 只接受 f32 而不是自己轉：cpal 的預設選擇順序把 F32 排在最前，
        // 走到這裡代表裝置真的不支援，那時該讓使用者知道而不是靜默降級。
        return Err(format!("麥克風格式不是 f32 而是 {sample_format:?}"));
    }
    let config: cpal::StreamConfig = supported.into();
    let in_rate = config.sample_rate as usize;
    let channels = config.channels as usize;
    crate::stt::live::log(&format!(
        "已開啟 mic 軌（CoreAudio {in_rate} Hz {channels} 聲道）"
    ));

    let mut pipe = MicPipeline::new(in_rate, Batcher::new(Track::Mic, tx))?;
    let alive = Arc::new(AtomicBool::new(true));
    let closed = alive.clone();

    let stream = device
        .build_input_stream(
            config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if !pipe.feed(data, channels) {
                    closed.store(false, Ordering::Relaxed);
                }
            },
            |e| crate::stt::live::log(&format!("麥克風串流錯誤：{e}")),
            None,
        )
        .map_err(|e| format!("建立麥克風串流失敗：{e}"))?;
    // 0.18 起所有後端都是暫停狀態返回，少了這一行會錄到一片空白
    stream.play().map_err(|e| format!("啟動麥克風失敗：{e}"))?;

    while !stop.load(Ordering::Relaxed) && alive.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
    }
    // Stream 在 macOS 上不是 Send，因此它從建立到 drop 都留在這條執行緒
    drop(stream);
    Ok(())
}

/// 降混成單聲道、降取樣到 16 kHz、再分批。
struct MicPipeline {
    /// 來源已經是 16 kHz 時就沒有重取樣器，直接送
    resampler: Option<Fft<f32>>,
    /// 重取樣器一次要固定長度的輸入，湊不滿的留到下一次回呼
    carry: Vec<f32>,
    frames_in: usize,
    /// 重取樣輸出的暫存。音訊回呼裡不配置記憶體，配置的耗時不可預測，
    /// 撞上一次就是一段掉幀
    out: Vec<f32>,
    batcher: Batcher,
}

impl MicPipeline {
    fn new(in_rate: usize, batcher: Batcher) -> Result<Self, String> {
        if in_rate == SAMPLE_RATE as usize {
            return Ok(Self {
                resampler: None,
                carry: Vec::new(),
                frames_in: 0,
                out: Vec::new(),
                batcher,
            });
        }
        // 固定輸入長度：回呼給的長度不保證穩定，自己控制輸入端比較好對齊。
        // 1024 幀在 48 kHz 是 21 ms，遠小於一批 100 ms，不會拖慢即時稿。
        let resampler = Fft::<f32>::new(in_rate, SAMPLE_RATE as usize, 1024, 1, FixedSync::Input)
            .map_err(|e| format!("建立重取樣器失敗（{in_rate} Hz → 16 kHz）：{e}"))?;
        let frames_in = resampler.input_frames_next();
        let out = vec![0.0; resampler.output_frames_max()];
        Ok(Self {
            resampler: Some(resampler),
            carry: Vec::with_capacity(frames_in * 2),
            frames_in,
            out,
            batcher,
        })
    }

    /// 回傳 false 代表下游已關閉。
    fn feed(&mut self, data: &[f32], channels: usize) -> bool {
        // 降混而不是取第一個聲道：外接麥克風常把訊號放在第二軌，
        // 只取第一軌會錄到一片安靜而且沒有任何錯誤
        let mono: Vec<f32> = if channels <= 1 {
            data.to_vec()
        } else {
            data.chunks(channels)
                .map(|f| f.iter().sum::<f32>() / f.len() as f32)
                .collect()
        };

        let Some(resampler) = self.resampler.as_mut() else {
            return self.batcher.push(&mono);
        };

        self.carry.extend_from_slice(&mono);
        while self.carry.len() >= self.frames_in {
            let rest = self.carry.split_off(self.frames_in);
            let block = std::mem::replace(&mut self.carry, rest);
            // 兩個 `expect` 都是恆等式：frames 就是各自緩衝區自己的長度，
            // 而 SizeError 只在 buf.len() < channels * frames 時發生
            let out_frames = self.out.len();
            let input =
                SequentialSlice::new(&block, 1, block.len()).expect("frames 取自 block 自身的長度");
            let mut output = SequentialSlice::new_mut(&mut self.out, 1, out_frames)
                .expect("out 的長度就是 output_frames_max()");
            match resampler.process_into_buffer(&input, &mut output, None) {
                // 寫進去的幀數每次可能不同，只送實際寫到的那一段
                Ok((_, written)) => {
                    if !self.batcher.push(&self.out[..written]) {
                        return false;
                    }
                }
                Err(e) => {
                    crate::stt::live::log(&format!("麥克風重取樣失敗：{e}"));
                    return false;
                }
            }
            self.frames_in = resampler.input_frames_next();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分批的時間軸是「送出去的樣本數」，不是系統時鐘。
    ///
    /// 拿時鐘當時間軸的話，暫停與丟幀都會讓引用定位到錯的地方 —— 那是
    /// Windows 那邊踩過的坑（BLUEPRINT.md §17.2 的第一列）。
    #[test]
    fn test_each_batch_carries_the_offset_of_the_audio_it_contains() {
        let (tx, rx) = channel();
        let mut b = Batcher::new(Track::Mic, tx);
        // 三批的量一次餵進去
        assert!(b.push(&vec![0.1; CHUNK_FRAMES * 3]));
        drop(b);

        let chunks: Vec<Chunk> = rx.into_iter().collect();
        assert_eq!(chunks.len(), 3);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.samples.len(), CHUNK_FRAMES);
            assert_eq!(c.captured_start_ms, i as u64 * 100, "第 {i} 批的位置不對");
        }
    }

    #[test]
    fn test_partial_batches_wait_instead_of_going_out_short() {
        // 不足 100 ms 就送的話，下游的能量與 VAD 判斷都會在半批上做，
        // 而那些門檻是以 100 ms 為單位校準的
        let (tx, rx) = channel();
        let mut b = Batcher::new(Track::System, tx);
        assert!(b.push(&vec![0.2; CHUNK_FRAMES - 1]));
        assert!(rx.try_recv().is_err(), "不足一批就送出去了");
        assert!(b.push(&[0.2]));
        assert_eq!(
            rx.try_recv().expect("湊滿了該送").samples.len(),
            CHUNK_FRAMES
        );
    }

    #[test]
    fn test_a_closed_receiver_is_reported_as_done_not_as_an_error() {
        let (tx, rx) = channel();
        let mut b = Batcher::new(Track::Mic, tx);
        drop(rx);
        assert!(
            !b.push(&vec![0.0; CHUNK_FRAMES]),
            "接收端關閉時沒有回報收工"
        );
    }

    #[test]
    fn test_multi_channel_input_is_mixed_down_not_truncated() {
        // 外接麥克風常把訊號放在第二軌。只取第一軌會錄到一片安靜，
        // 而且不會有任何錯誤 —— 那種失敗要到聽錄音才發現
        let (tx, rx) = channel();
        let mut p = MicPipeline::new(SAMPLE_RATE as usize, Batcher::new(Track::Mic, tx))
            .expect("16 kHz 不需要重取樣器");
        // 左聲道全靜音，右聲道有訊號
        let interleaved: Vec<f32> = (0..CHUNK_FRAMES).flat_map(|_| [0.0, 1.0]).collect();
        assert!(p.feed(&interleaved, 2));
        let chunk = rx.try_recv().expect("該送出一批");
        assert!(
            chunk.samples.iter().all(|s| (*s - 0.5).abs() < 1e-6),
            "沒有降混，右聲道的訊號不見了"
        );
    }

    #[test]
    fn test_a_16k_source_skips_the_resampler_entirely() {
        // ScreenCaptureKit 直接給 16 kHz，麥克風偶爾也是。多做一次重取樣
        // 只會損失品質
        let (tx, _rx) = channel();
        let p = MicPipeline::new(SAMPLE_RATE as usize, Batcher::new(Track::Mic, tx)).unwrap();
        assert!(p.resampler.is_none());

        let (tx, _rx) = channel();
        let p = MicPipeline::new(48_000, Batcher::new(Track::Mic, tx)).unwrap();
        assert!(p.resampler.is_some(), "48 kHz 沒有接上重取樣器");
    }
}
