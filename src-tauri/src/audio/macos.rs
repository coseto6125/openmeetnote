//! macOS 雙軌擷取：系統音訊走 ScreenCaptureKit，麥克風走 CoreAudio（cpal）。
//!
//! 兩個來源而不是一個，理由與 Windows 那邊相同：`track` 是「本機 vs 遠端」的
//! 先驗，混掉之後就再也分不出來。
//!
//! 為什麼不是同一個 API 拿兩軌：ScreenCaptureKit 從 macOS 15 才能擷取麥克風
//! （`microphoneCaptureEnabled`）。綁在 15 上等於讓 13 與 14 的使用者錄不到
//! 自己的聲音，而那是這個產品的一半。cpal 走 CoreAudio，每個版本都有。
//!
//! 兩條路徑都可能拿到非 16 kHz 單聲道的音訊，因此共用同一條降混與重取樣的
//! 管線（[`Downmix`]）。SCK 的設定要得到 16 kHz 不代表交付的就是 16 kHz：
//! 那是請求不是保證，而把 48 kHz 當成 16 kHz 用會讓時間軸縮成三分之一，
//! 引用因此定位到完全錯誤的地方。實際格式從 `CMSampleBuffer` 的
//! format description 讀，不從設定推。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::audioadapter_buffers::direct::SequentialSlice;
use rubato::{Fft, FixedSync, Resampler};
use screencapturekit::cm::AudioBufferList;
use screencapturekit::prelude::*;

use super::{
    await_tracks, AudioCapture, AudioError, Chunk, TrackReady, CAPTURE_BACKLOG_CHUNKS, SAMPLE_RATE,
};
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
        let (tx, rx) = sync_channel(CAPTURE_BACKLOG_CHUNKS);
        self.stop = Arc::new(AtomicBool::new(false));

        // 兩軌各自一條執行緒，但兩軌都開起來才算開始。任一軌失敗就整體
        // 失敗：沒有螢幕錄製權限時「只錄得到麥克風」不是降級，那是把使用者
        // 以為錄到的內容換成別的東西。
        let (ready_tx, ready_rx) = channel();

        let (t, s, r) = (tx.clone(), self.stop.clone(), ready_tx.clone());
        self.threads
            .push(std::thread::spawn(move || system_audio(t, s, &r)));
        let s = self.stop.clone();
        self.threads
            .push(std::thread::spawn(move || microphone(tx, s, &ready_tx)));

        match await_tracks(&ready_rx, 2) {
            Ok(()) => Ok(rx),
            Err(e) => {
                // 開失敗時把已經起來的那一軌收掉，否則它會一直佔著裝置
                self.stop();
                Err(e)
            }
        }
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
    tx: SyncSender<Chunk>,
    pending: Vec<f32>,
    sent_frames: u64,
    /// 佇列滿的時候丟掉了幾批。丟掉不記錄，就會變成一段沒有人知道
    /// 為什麼不見的逐字稿。
    dropped: Arc<AtomicU64>,
}

impl Batcher {
    fn new(track: Track, tx: SyncSender<Chunk>) -> Self {
        Self {
            track,
            tx,
            pending: Vec::with_capacity(CHUNK_FRAMES * 2),
            sent_frames: 0,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 回傳 false 代表接收端已關閉，呼叫端該收工。那不是錯誤，是會議結束。
    fn push(&mut self, samples: &[f32]) -> bool {
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= CHUNK_FRAMES {
            let rest = self.pending.split_off(CHUNK_FRAMES);
            let samples = std::mem::replace(&mut self.pending, rest);
            if !self.send(samples) {
                return false;
            }
        }
        true
    }

    /// 把不滿一批的尾巴送出去。只有停止時呼叫，因此「短批」最多一次。
    ///
    /// 不送的話每軌最後最多 100 ms 會被丟掉，而那正是使用者按下停止之前
    /// 說的最後一個字的韻尾。
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let samples = std::mem::take(&mut self.pending);
        self.send(samples);
    }

    fn send(&mut self, samples: Vec<f32>) -> bool {
        let frames = samples.len() as u64;
        let chunk = Chunk {
            track: self.track,
            captured_start_ms: self.sent_frames * 1000 / SAMPLE_RATE as u64,
            samples: samples.into(),
        };
        self.sent_frames += frames;
        match self.tx.try_send(chunk) {
            Ok(()) => true,
            // 滿了就丟這一批而不是等：這裡是音訊回呼，阻塞在這條路徑上會
            // 讓整個裝置的回呼變慢，掉的就不只這一批
            Err(TrySendError::Full(_)) => {
                let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    crate::stt::live::log(&format!(
                        "{} 軌因下游積壓丟棄了 {n} 批音訊（每批 100 ms）",
                        self.track.as_str()
                    ));
                }
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

/* ── 系統音訊：ScreenCaptureKit ────────────────────────────────── */

/// SCK 的回呼在它自己的執行緒上跑，因此分批狀態要能跨執行緒共享。
///
/// `add_output_handler` 取走所有權，但停止時還要從外面把尾巴 flush 出來，
/// 所以狀態放在 `Arc` 裡，兩邊各拿一份把手。
#[derive(Clone)]
struct SystemAudioSink {
    inner: Arc<Mutex<SystemAudioState>>,
}

struct SystemAudioState {
    /// 第一批音訊到達時才建立：實際格式要從 sample buffer 讀，設定裡寫的
    /// 是請求不是保證。
    pipe: Option<Downmix>,
    batcher: Option<Batcher>,
    /// 格式不受支援時只抱怨一次，不是每 100 ms 一次。
    complained: bool,
}

impl SCStreamOutputTrait for SystemAudioSink {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio {
            return;
        }
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        if state.batcher.is_none() {
            return; // 已經收工
        }

        let Some(desc) = sample.format_description() else {
            return;
        };
        let (Some(rate), Some(channels)) = (desc.audio_sample_rate(), desc.audio_channel_count())
        else {
            return;
        };
        // 只解 32-bit little-endian float。int16 硬當成 f32 解出來是噪音，
        // 而噪音會被轉錄成看起來像內容的字。
        if !desc.audio_is_float() || desc.audio_is_big_endian() || channels == 0 {
            if !state.complained {
                state.complained = true;
                crate::stt::live::log(&format!(
                    "系統音訊的格式不是小端 32-bit float（{} bit、{channels} 聲道），這一軌停止",
                    desc.audio_bits_per_channel().unwrap_or(0)
                ));
            }
            state.batcher = None;
            return;
        }

        if state.pipe.is_none() {
            let Some(batcher) = state.batcher.take() else {
                return;
            };
            let rate = rate as usize;
            match Downmix::new(rate, batcher) {
                Ok(p) => {
                    crate::stt::live::log(&format!(
                        "系統音訊實際格式：{rate} Hz {channels} 聲道 f32"
                    ));
                    state.pipe = Some(p);
                }
                Err(e) => {
                    crate::stt::live::log(&format!("系統音訊無法重取樣：{e}"));
                    return;
                }
            }
        }

        let Some(list) = sample.audio_buffer_list() else {
            return;
        };
        let mono = interleave_to_mono(&list, channels as usize);
        let Some(pipe) = state.pipe.as_mut() else {
            return;
        };
        if !pipe.feed(&mono, 1) {
            state.pipe = None;
        }
    }
}

/// 把 CoreAudio 的緩衝清單攤成單聲道。
///
/// 兩種擺法都要處理，而它們的差別從位元組長度看不出來：
///
/// - **交錯**：一個 buffer，`number_channels` 是 n，樣本是 L R L R…
/// - **分平面**：n 個 buffer，每個 `number_channels` 是 1，各自是一整條聲道
///
/// 舊的做法是把所有 buffer 首尾相接，分平面立體聲因此變成「左聲道播完再播
/// 右聲道」，長度翻倍而且內容錯位。
fn interleave_to_mono(list: &AudioBufferList, declared_channels: usize) -> Vec<f32> {
    let planes: Vec<Vec<f32>> = list
        .iter()
        .map(|b| {
            let bytes = b.data();
            bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        })
        .collect();

    match planes.len() {
        0 => Vec::new(),
        // 交錯：一個 buffer 裝了全部聲道
        1 => {
            let ch = declared_channels.max(1);
            if ch == 1 {
                return planes.into_iter().next().unwrap_or_default();
            }
            planes[0]
                .chunks(ch)
                .map(|f| f.iter().sum::<f32>() / f.len() as f32)
                .collect()
        }
        // 分平面：逐幀取平均。長度不齊時以最短的為準，寧可短一點也不要
        // 拿越界的資料湊數
        n => {
            let frames = planes.iter().map(Vec::len).min().unwrap_or(0);
            (0..frames)
                .map(|i| planes.iter().map(|p| p[i]).sum::<f32>() / n as f32)
                .collect()
        }
    }
}

fn system_audio(
    tx: SyncSender<Chunk>,
    stop: Arc<AtomicBool>,
    ready: &Sender<TrackReady>,
) -> Result<(), String> {
    let sink = SystemAudioSink {
        inner: Arc::new(Mutex::new(SystemAudioState {
            pipe: None,
            batcher: Some(Batcher::new(Track::System, tx)),
            complained: false,
        })),
    };

    let stream = match open_system_stream(&sink) {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };
    crate::stt::live::log("已開啟 system 軌（ScreenCaptureKit）");

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
    }
    stream
        .stop_capture()
        .map_err(|e| format!("停止系統音訊擷取失敗：{e}"))?;
    // 停止之後回呼不會再進來，這時才輪得到尾巴
    if let Ok(mut state) = sink.inner.lock() {
        state.flush();
    }
    Ok(())
}

impl SystemAudioState {
    fn flush(&mut self) {
        if let Some(pipe) = self.pipe.as_mut() {
            pipe.flush();
        } else if let Some(b) = self.batcher.as_mut() {
            b.flush();
        }
    }
}

fn open_system_stream(sink: &SystemAudioSink) -> Result<SCStream, String> {
    let content = SCShareableContent::get()
        .map_err(|e| format!("系統音訊：取得可擷取內容失敗（多半是沒有螢幕錄製權限）：{e}"))?;
    let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or("系統音訊：找不到任何顯示器")?;

    // 擷取整個顯示器的音訊。畫面內容不使用，但 SCK 的音訊是掛在內容篩選器上的，
    // 沒有篩選器就沒有串流。
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build();

    // 要 16 kHz 單聲道是為了省掉一次重取樣，但交付的格式仍以 sample buffer
    // 自己說的為準 —— 這裡只是請求。
    let config = SCStreamConfiguration::new()
        .with_captures_audio(true)
        .with_sample_rate(SAMPLE_RATE as i32)
        .with_channel_count(1);

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(sink.clone(), SCStreamOutputType::Audio);
    stream
        .start_capture()
        .map_err(|e| format!("系統音訊：啟動擷取失敗（多半是沒有螢幕錄製權限）：{e}"))?;
    Ok(stream)
}

/* ── 麥克風：CoreAudio（cpal） ─────────────────────────────────── */

fn microphone(
    tx: SyncSender<Chunk>,
    stop: Arc<AtomicBool>,
    ready: &Sender<TrackReady>,
) -> Result<(), String> {
    let alive = Arc::new(AtomicBool::new(true));
    let pipe = Arc::new(Mutex::new(None::<Downmix>));

    let stream = match open_microphone(tx, &alive, &pipe) {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };

    while !stop.load(Ordering::Relaxed) && alive.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
    }
    // Stream 在 macOS 上不是 Send，因此它從建立到 drop 都留在這條執行緒。
    // 先 drop 再 flush：回呼停了才輪得到尾巴。
    drop(stream);
    if let Ok(mut p) = pipe.lock() {
        if let Some(p) = p.as_mut() {
            p.flush();
        }
    }
    Ok(())
}

fn open_microphone(
    tx: SyncSender<Chunk>,
    alive: &Arc<AtomicBool>,
    pipe: &Arc<Mutex<Option<Downmix>>>,
) -> Result<cpal::Stream, String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("麥克風：找不到預設裝置")?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("麥克風：讀取格式失敗（多半是沒有麥克風權限）：{e}"))?;
    let sample_format = supported.sample_format();
    if sample_format != cpal::SampleFormat::F32 {
        // 只接受 f32 而不是自己轉：cpal 的預設選擇順序把 F32 排在最前，
        // 走到這裡代表裝置真的不支援，那時該讓使用者知道而不是靜默降級。
        return Err(format!("麥克風：格式不是 f32 而是 {sample_format:?}"));
    }
    let config: cpal::StreamConfig = supported.into();
    let in_rate = config.sample_rate as usize;
    let channels = config.channels as usize;

    *pipe.lock().map_err(|_| "麥克風：狀態鎖損毀")? =
        Some(Downmix::new(in_rate, Batcher::new(Track::Mic, tx))?);

    let closed = alive.clone();
    let shared = pipe.clone();
    let stream = device
        .build_input_stream(
            config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let Ok(mut guard) = shared.lock() else {
                    return;
                };
                if let Some(p) = guard.as_mut() {
                    if !p.feed(data, channels) {
                        closed.store(false, Ordering::Relaxed);
                    }
                }
            },
            |e| crate::stt::live::log(&format!("麥克風串流錯誤：{e}")),
            None,
        )
        .map_err(|e| format!("麥克風：建立串流失敗（多半是沒有麥克風權限）：{e}"))?;
    // 0.18 起所有後端都是暫停狀態返回，少了這一行會錄到一片空白
    stream
        .play()
        .map_err(|e| format!("麥克風：啟動失敗：{e}"))?;
    crate::stt::live::log(&format!(
        "已開啟 mic 軌（CoreAudio {in_rate} Hz {channels} 聲道）"
    ));
    Ok(stream)
}

/// 降混成單聲道、降取樣到 16 kHz、再分批。兩軌共用。
struct Downmix {
    /// 來源已經是 16 kHz 時就沒有重取樣器，直接送
    resampler: Option<Fft<f32>>,
    in_rate: usize,
    /// 重取樣器一次要固定長度的輸入，湊不滿的留到下一次回呼
    carry: Vec<f32>,
    frames_in: usize,
    /// 重取樣輸出的暫存。音訊回呼裡不配置記憶體，配置的耗時不可預測，
    /// 撞上一次就是一段掉幀
    out: Vec<f32>,
    batcher: Batcher,
}

impl Downmix {
    fn new(in_rate: usize, batcher: Batcher) -> Result<Self, String> {
        if in_rate == SAMPLE_RATE as usize {
            return Ok(Self {
                resampler: None,
                in_rate,
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
            in_rate,
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

        if self.resampler.is_none() {
            return self.batcher.push(&mono);
        }

        self.carry.extend_from_slice(&mono);
        while self.carry.len() >= self.frames_in {
            let rest = self.carry.split_off(self.frames_in);
            let block = std::mem::replace(&mut self.carry, rest);
            if !self.resample_block(&block, block.len()) {
                return false;
            }
        }
        true
    }

    /// 收工時把還沒湊滿一個重取樣區塊的樣本吐出來。
    ///
    /// 重取樣器一次只吃固定長度，所以補零湊滿再算，然後只取與真實樣本等比
    /// 的輸出。不補的話 48 kHz 的麥克風每次停止都會少掉最多 21 ms，
    /// 加上分批本身的 100 ms，最後一個字的韻尾常常就在裡面。
    fn flush(&mut self) {
        if !self.carry.is_empty() && self.resampler.is_some() {
            let real = self.carry.len();
            let mut block = std::mem::take(&mut self.carry);
            block.resize(self.frames_in, 0.0);
            // 補零那一段不是音訊，不能讓它推進擷取時間軸
            let keep = real * SAMPLE_RATE as usize / self.in_rate;
            self.resample_block(&block, keep);
        }
        self.batcher.flush();
    }

    /// `keep` 是最多要保留幾個輸出幀，用來擋掉 flush 補進去的靜音。
    fn resample_block(&mut self, block: &[f32], keep: usize) -> bool {
        let Some(resampler) = self.resampler.as_mut() else {
            return true;
        };
        // 兩個 `expect` 都是恆等式：frames 就是各自緩衝區自己的長度，
        // 而 SizeError 只在 buf.len() < channels * frames 時發生
        let out_frames = self.out.len();
        let input =
            SequentialSlice::new(block, 1, block.len()).expect("frames 取自 block 自身的長度");
        let mut output = SequentialSlice::new_mut(&mut self.out, 1, out_frames)
            .expect("out 的長度就是 output_frames_max()");
        match resampler.process_into_buffer(&input, &mut output, None) {
            // 寫進去的幀數每次可能不同，只送實際寫到的那一段
            Ok((_, written)) => {
                let n = written.min(keep);
                if !self.batcher.push(&self.out[..n]) {
                    return false;
                }
            }
            Err(e) => {
                crate::stt::live::log(&format!("重取樣失敗：{e}"));
                return false;
            }
        }
        self.frames_in = resampler.input_frames_next();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batcher(track: Track) -> (Batcher, Receiver<Chunk>) {
        let (tx, rx) = sync_channel(CAPTURE_BACKLOG_CHUNKS);
        (Batcher::new(track, tx), rx)
    }

    /// 分批的時間軸是「送出去的樣本數」，不是系統時鐘。
    ///
    /// 拿時鐘當時間軸的話，暫停與丟幀都會讓引用定位到錯的地方 —— 那是
    /// Windows 那邊踩過的坑（BLUEPRINT.md §17.2 的第一列）。
    #[test]
    fn test_each_batch_carries_the_offset_of_the_audio_it_contains() {
        let (mut b, rx) = batcher(Track::Mic);
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
        let (mut b, rx) = batcher(Track::System);
        assert!(b.push(&vec![0.2; CHUNK_FRAMES - 1]));
        assert!(rx.try_recv().is_err(), "不足一批就送出去了");
        assert!(b.push(&[0.2]));
        assert_eq!(
            rx.try_recv().expect("湊滿了該送").samples.len(),
            CHUNK_FRAMES
        );
    }

    /// 停止時尾巴要吐出來，而且只有它可以是短的。
    #[test]
    fn test_the_tail_left_over_at_stop_is_sent_instead_of_dropped() {
        let (mut b, rx) = batcher(Track::Mic);
        assert!(b.push(&vec![0.3; CHUNK_FRAMES + 400]));
        let full = rx.try_recv().expect("滿的那一批該送");
        assert_eq!(full.samples.len(), CHUNK_FRAMES);
        assert!(rx.try_recv().is_err(), "尾巴不該在 flush 之前送出");

        b.flush();
        let tail = rx.try_recv().expect("停止時尾巴要送出來");
        assert_eq!(tail.samples.len(), 400);
        assert_eq!(tail.captured_start_ms, 100, "尾巴的位置接不上前一批");
        // flush 過的 Batcher 不會再吐出空批次
        b.flush();
        drop(b);
        assert!(rx.try_recv().is_err(), "flush 送出了空批次");
    }

    #[test]
    fn test_a_closed_receiver_is_reported_as_done_not_as_an_error() {
        let (mut b, rx) = batcher(Track::Mic);
        drop(rx);
        assert!(
            !b.push(&vec![0.0; CHUNK_FRAMES]),
            "接收端關閉時沒有回報收工"
        );
    }

    /// 下游停住時要丟批次，不是無限囤積，也不是在音訊回呼裡等。
    #[test]
    fn test_a_backed_up_queue_drops_batches_instead_of_growing_without_bound() {
        let (tx, rx) = sync_channel(2);
        let mut b = Batcher::new(Track::System, tx);
        // 沒有人讀，佇列只有兩格
        assert!(b.push(&vec![0.4; CHUNK_FRAMES * 20]), "丟批次不該當成收工");
        assert_eq!(b.dropped.load(Ordering::Relaxed), 18);
        assert_eq!(rx.try_recv().unwrap().samples.len(), CHUNK_FRAMES);
    }

    #[test]
    fn test_multi_channel_input_is_mixed_down_not_truncated() {
        // 外接麥克風常把訊號放在第二軌。只取第一軌會錄到一片安靜，
        // 而且不會有任何錯誤 —— 那種失敗要到聽錄音才發現
        let (b, rx) = batcher(Track::Mic);
        let mut p = Downmix::new(SAMPLE_RATE as usize, b).expect("16 kHz 不需要重取樣器");
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
        let (b, _rx) = batcher(Track::Mic);
        let p = Downmix::new(SAMPLE_RATE as usize, b).unwrap();
        assert!(p.resampler.is_none());

        let (b, _rx) = batcher(Track::Mic);
        let p = Downmix::new(48_000, b).unwrap();
        assert!(p.resampler.is_some(), "48 kHz 沒有接上重取樣器");
    }

    /// 48 kHz 進來，出去的時間軸必須是 48 kHz 的三分之一。
    ///
    /// 把 48 kHz 當成 16 kHz 直接送，是這個模組最貴的一種錯：時間軸縮成
    /// 三分之一，每一筆引用都定位到錯的地方，而聲音本身聽起來只是快了一點。
    #[test]
    fn test_a_48k_source_produces_a_timeline_at_16k_not_at_48k() {
        let (b, rx) = batcher(Track::System);
        let mut p = Downmix::new(48_000, b).unwrap();
        // 三秒的 48 kHz 音訊
        assert!(p.feed(&vec![0.5; 48_000 * 3], 1));
        p.flush();
        // Downmix 握著 Batcher，Batcher 握著 tx。不先放掉它，into_iter()
        // 就是在等一個永遠不會關閉的通道 —— 測試不會失敗，它會一直跑。
        drop(p);
        let frames: usize = rx.into_iter().map(|c| c.samples.len()).sum();
        let ms = frames * 1000 / SAMPLE_RATE as usize;
        assert!(
            (2900..=3100).contains(&ms),
            "三秒的音訊重取樣後變成 {ms} ms"
        );
    }
}
