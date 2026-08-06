//! Windows WASAPI 雙軌擷取。
//!
//! 系統音訊走 loopback（遠端與會者），麥克風走一般擷取（本機）。WASAPI 表達
//! loopback 的方式是「取 Render 裝置，但以 Capture 方向初始化」—— 那個組合不是
//! 筆誤，是 API 的既定用法。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;

use wasapi::{initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

use super::{
    await_tracks, AudioCapture, AudioError, Chunk, TrackReady, CAPTURE_BACKLOG_CHUNKS, SAMPLE_RATE,
};
use crate::model::Track;

/// 每批送出的樣本數。100 ms 一批：夠小讓 UI 感覺即時，夠大讓下游不必為了
/// 每個 frame 醒來一次。
const CHUNK_FRAMES: usize = SAMPLE_RATE as usize / 10;

#[derive(Default)]
pub struct WasapiCapture {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Result<(), String>>>,
}

impl AudioCapture for WasapiCapture {
    fn start(&mut self) -> Result<Receiver<Chunk>, AudioError> {
        let (tx, rx) = sync_channel(CAPTURE_BACKLOG_CHUNKS);
        self.stop = Arc::new(AtomicBool::new(false));

        // 兩軌都開起來才算開始。裝置被別的程式獨佔或不存在時，只 spawn 執行緒
        // 就回報成功，會讓會議進到 Recording 而其中一軌一個樣本都沒有。
        let (ready_tx, ready_rx) = channel();
        for (track, dir) in [
            (Track::Mic, Direction::Capture),
            (Track::System, Direction::Render),
        ] {
            let (tx, stop, ready) = (tx.clone(), self.stop.clone(), ready_tx.clone());
            self.threads
                .push(std::thread::spawn(move || capture(track, dir, tx, stop, &ready)));
        }

        match await_tracks(&ready_rx, 2) {
            Ok(()) => Ok(rx),
            Err(e) => {
                self.stop();
                Err(e)
            }
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.threads.drain(..) {
            // 擷取執行緒的錯誤在這裡浮出來。裝置被佔用或權限不足時，
            // 靜默送出一片空白比直接失敗更難查。
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => crate::stt::live::log(&format!("音訊擷取結束於錯誤：{e}")),
                Err(_) => crate::stt::live::log("音訊擷取執行緒異常結束"),
            }
        }
    }
}

fn capture(
    track: Track,
    device_dir: Direction,
    tx: SyncSender<Chunk>,
    stop: Arc<AtomicBool>,
    ready: &Sender<TrackReady>,
) -> Result<(), String> {
    capture_inner(track, device_dir, tx, stop, ready).map_err(|e| e.to_string())
}

/// 累積到一批才送，並記下丟掉幾批。
///
/// 佇列有界：下游轉錄慢於即時速度時，無界佇列會一路吃記憶體到會議結束。
/// 滿了就丟這一批而不是等 —— 擷取停下來的話掉的不只這一批。
struct Batcher {
    track: Track,
    tx: SyncSender<Chunk>,
    pending: Vec<f32>,
    sent_frames: u64,
    dropped: AtomicU64,
}

impl Batcher {
    fn new(track: Track, tx: SyncSender<Chunk>) -> Self {
        Self {
            track,
            tx,
            pending: Vec::with_capacity(CHUNK_FRAMES * 2),
            sent_frames: 0,
            dropped: AtomicU64::new(0),
        }
    }

    /// 回傳 false 代表接收端已關閉，呼叫端該收工。
    fn push(&mut self, samples: &[f32]) -> bool {
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= CHUNK_FRAMES {
            let rest = self.pending.split_off(CHUNK_FRAMES);
            let batch = std::mem::replace(&mut self.pending, rest);
            if !self.send(batch) {
                return false;
            }
        }
        true
    }

    /// 停止時把不滿一批的尾巴送出去。少了這一步，每軌最後最多 100 ms
    /// 會被丟掉，而那是使用者按下停止之前說的最後一個字。
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.pending);
        self.send(batch);
    }

    fn send(&mut self, samples: Vec<f32>) -> bool {
        let frames = samples.len() as u64;
        let chunk = Chunk {
            track: self.track,
            captured_start_ms: self.sent_frames * 1000 / SAMPLE_RATE as u64,
            samples,
        };
        self.sent_frames += frames;
        match self.tx.try_send(chunk) {
            Ok(()) => true,
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

fn capture_inner(
    track: Track,
    device_dir: Direction,
    tx: SyncSender<Chunk>,
    stop: Arc<AtomicBool>,
    ready: &Sender<TrackReady>,
) -> Result<(), Box<dyn std::error::Error>> {
    let opened = open_device(track, device_dir);
    let (client, h_event, capture_client, blockalign) = match opened {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(format!("{} 軌：{e}", track.as_str())));
            return Err(e.into());
        }
    };

    let mut raw = vec![0u8; blockalign * client.get_buffer_size()? as usize * 4];
    let mut batcher = Batcher::new(track, tx);

    while !stop.load(Ordering::Relaxed) {
        // 逾時而不是無限等待，否則停止旗標要等到下一個音訊事件才會被看到；
        // 全程靜音的會議尾段會因此卡住不結束。
        if h_event.wait_for_event(200).is_err() {
            continue;
        }
        let (frames, _flags) = capture_client.read_from_device(&mut raw)?;
        let samples: Vec<f32> = (0..frames as usize)
            .map(|f| {
                let b = &raw[f * blockalign..f * blockalign + 4];
                f32::from_le_bytes([b[0], b[1], b[2], b[3]])
            })
            .collect();
        // 接收端已關閉代表會議結束，這不是錯誤
        if !batcher.push(&samples) {
            client.stop_stream()?;
            return Ok(());
        }
    }

    client.stop_stream()?;
    batcher.flush();
    Ok(())
}

type OpenedDevice = (
    wasapi::AudioClient,
    wasapi::Handle,
    wasapi::AudioCaptureClient,
    usize,
);

fn open_device(track: Track, device_dir: Direction) -> Result<OpenedDevice, String> {
    let open = || -> Result<OpenedDevice, Box<dyn std::error::Error>> {
        initialize_mta().ok()?;
        let device = DeviceEnumerator::new()?.get_default_device(&device_dir)?;
        let mut client = device.get_iaudioclient()?;

        // 單聲道 32-bit float。autoconvert 讓音訊引擎處理裝置原生格式（多半是
        // 48 kHz 立體聲）到這個格式的轉換，省掉自己寫重取樣。
        let format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 1, None);
        let (_def, min_period) = client.get_device_period()?;
        client.initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: min_period,
            },
        )?;

        let h_event = client.set_get_eventhandle()?;
        let capture_client = client.get_audiocaptureclient()?;
        let blockalign = format.get_blockalign() as usize;
        client.start_stream()?;
        Ok((client, h_event, capture_client, blockalign))
    };
    let opened = open().map_err(|e| e.to_string())?;
    crate::stt::live::log(&format!("已開啟 {} 軌裝置", track.as_str()));
    Ok(opened)
}
