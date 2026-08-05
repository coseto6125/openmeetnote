//! Windows WASAPI 雙軌擷取。
//!
//! 系統音訊走 loopback（遠端與會者），麥克風走一般擷取（本機）。WASAPI 表達
//! loopback 的方式是「取 Render 裝置，但以 Capture 方向初始化」—— 那個組合不是
//! 筆誤，是 API 的既定用法。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use wasapi::{initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

use super::{AudioCapture, AudioError, Chunk, SAMPLE_RATE};
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
        let (tx, rx) = channel();
        self.stop = Arc::new(AtomicBool::new(false));

        for (track, dir) in [
            (Track::Mic, Direction::Capture),
            (Track::System, Direction::Render),
        ] {
            let (tx, stop) = (tx.clone(), self.stop.clone());
            self.threads
                .push(std::thread::spawn(move || capture(track, dir, tx, stop)));
        }
        Ok(rx)
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
    tx: Sender<Chunk>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    capture_inner(track, device_dir, tx, stop).map_err(|e| e.to_string())
}

fn capture_inner(
    track: Track,
    device_dir: Direction,
    tx: Sender<Chunk>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    initialize_mta().ok()?;

    let device = DeviceEnumerator::new()?.get_default_device(&device_dir)?;
    crate::stt::live::log(&format!("已開啟 {} 軌裝置", track.as_str()));
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

    let mut raw = vec![0u8; blockalign * client.get_buffer_size()? as usize * 4];
    let mut pending: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES * 2);
    let mut captured_frames: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        // 逾時而不是無限等待，否則停止旗標要等到下一個音訊事件才會被看到；
        // 全程靜音的會議尾段會因此卡住不結束。
        if h_event.wait_for_event(200).is_err() {
            continue;
        }
        let (frames, _flags) = capture_client.read_from_device(&mut raw)?;
        for f in 0..frames as usize {
            let b = &raw[f * blockalign..f * blockalign + 4];
            pending.push(f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        }

        while pending.len() >= CHUNK_FRAMES {
            let rest = pending.split_off(CHUNK_FRAMES);
            let samples = std::mem::replace(&mut pending, rest);
            let chunk = Chunk {
                track,
                captured_start_ms: captured_frames * 1000 / SAMPLE_RATE as u64,
                samples,
            };
            captured_frames += CHUNK_FRAMES as u64;
            // 接收端已關閉代表會議結束，這不是錯誤
            if tx.send(chunk).is_err() {
                client.stop_stream()?;
                return Ok(());
            }
        }
    }

    client.stop_stream()?;
    Ok(())
}
