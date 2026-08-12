//! 即時轉錄來源：把音訊擷取與兩個引擎接成 `TranscriptSource`。
//!
//! 兩個引擎跑在各自的執行緒上，因為它們的速度差二十倍：Paraformer 每秒就能
//! 回一次結果，whisper 一段十幾秒的音訊要跑好幾秒。放同一條執行緒會讓即時稿
//! 被定稿拖住，畫面就不動了。
//!
//! 對應到 session 既有的修訂語意：Paraformer 出 `Partial`，whisper 出 `Final`。
//! 定稿覆蓋即時稿走的是 `segment_id` + revision 那條既有路徑，這裡不需要新規則。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use std::io::Write;

use sherpa_rs::punctuate::{Punctuation, PunctuationConfig};
use sherpa_rs::silero_vad::{SileroVad, SileroVadConfig};

use super::diff::Corrections;
use super::paraformer::Paraformer;
use super::speakers::{speaker_at, SpeakerBook, SpeakerSpan};
use super::whisper::Whisper;
use super::{Result, SttError};
use crate::audio::{platform_capture, AudioCapture, AudioError, Chunk, SAMPLE_RATE};
use crate::model::Track;
use crate::session::{TranscriptInput, TranscriptSource};

/// 即時稿的更新間隔。太短會讓畫面抖動且浪費算力，太長會像沒在動。
const PARTIAL_EVERY_MS: u64 = 800;

/// 即時稿一次看多長的音訊。
///
/// 這是上限不是目標：Paraformer 每次都要看完整段，若讓 buffer 一直長下去，
/// 錄十分鐘就變成每 800 ms 算十分鐘的音訊，計算量隨錄音時間線性上升，
/// 到後來畫面就停住了。超過這個長度就丟掉最舊的部分。
const PARTIAL_WINDOW_MS: u64 = 15_000;

/// 使用者眼中「這個程式在哪裡」的那個目錄。
///
/// Windows 與 Linux 上是執行檔所在目錄。macOS 上執行檔在
/// `OpenMeetNote.app/Contents/MacOS/`，而使用者看到的是 `OpenMeetNote.app`
/// 本身 —— 他會把 `models/` 放在 `.app` 旁邊，因為 README 說「放在程式旁邊」，
/// 而在 Finder 裡 `.app` 就是程式。直接用 `current_exe().parent()` 的話，
/// 找的是 bundle 內部，那裡永遠不會有東西。
pub fn app_dir() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or(&exe);
    // …/Foo.app/Contents/MacOS/foo → …
    if dir.file_name().is_some_and(|n| n == "MacOS") {
        if let Some(bundle) = dir.parent().and_then(|p| p.parent()) {
            if bundle.extension().is_some_and(|e| e == "app") {
                if let Some(outside) = bundle.parent() {
                    return outside.to_path_buf();
                }
            }
        }
    }
    dir.to_path_buf()
}

/// 一定寫得進去的目錄，放日誌與其他執行期產物。
///
/// 不能用 [`app_dir`]：安裝到 `/Applications` 或 `C:\Program Files` 之後那裡
/// 是唯讀的，而日誌存在的唯一理由就是診斷那種「什麼都沒發生」的失敗。寫不
/// 進去的日誌等於沒有日誌。
pub fn data_dir() -> std::path::PathBuf {
    let dir = dirs::data_dir()
        .map(|d| d.join("OpenMeetNote"))
        .unwrap_or_else(app_dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 找資源（模型、詞表）的順序：程式旁邊優先，其次是使用者資料目錄。
///
/// 程式旁邊優先是因為模型有一 GB 以上，使用者需要知道那一 GB 在哪裡
/// （§5.3.1）；藏進 app data 只會讓「為什麼要一 GB」變成無解的問題。
/// 但安裝版的程式目錄不可寫，所以資料目錄要是一條真的走得通的退路。
fn resource_dirs() -> [std::path::PathBuf; 2] {
    [app_dir(), data_dir()]
}

/// 在資源目錄裡找一個相對路徑，回傳第一個存在的。都不存在時回第一個候選，
/// 讓錯誤訊息指向使用者最可能想放的位置。
fn find_resource(rel: &str) -> std::path::PathBuf {
    let dirs = resource_dirs();
    dirs.iter()
        .map(|d| d.join(rel))
        .find(|p| p.exists())
        .unwrap_or_else(|| dirs[0].join(rel))
}

/// 把訊息寫到使用者資料目錄的 stt.log。
///
/// Windows 的 GUI 子系統不接 stderr，`eprintln!` 在打包後的 app 裡等於什麼都
/// 沒做 —— 引擎載入失敗會變成「錄了半小時發現一個字都沒有」而且無從查起。
pub fn log(msg: &str) {
    let line = format!("[{}] {msg}\n", crate::clock::now_utc());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir().join("stt.log"))
    {
        let _ = f.write_all(line.as_bytes());
    }
    eprint!("{line}");
}

/// 記一批被丟掉的音訊，並在累計數是二的冪時說一次。
///
/// 每次都記的話，一旦佇列開始滿就是每 100 ms 一行；只記第一次又看不出
/// 塞了多久。二的冪讓行數隨時間成對數成長，而且每一行都比前一行多一倍，
/// 「偶爾一批」與「持續塞住」在 log 上長得不一樣。
fn note_drop(dropped: &AtomicU64, queue: &str) {
    let n = dropped.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        log(&format!("{queue}佇列已滿，累計丟棄 {n} 批音訊"));
    }
}

/// 每軌已經定稿幾段，由定稿執行緒推進、即時稿執行緒讀取。
#[derive(Default, Clone)]
struct Progress {
    mic: Arc<AtomicU64>,
    system: Arc<AtomicU64>,
}

impl Progress {
    fn of(&self, track: Track) -> &AtomicU64 {
        match track {
            Track::Mic => &self.mic,
            Track::System => &self.system,
        }
    }
}

/// 兩軌的 segment_id 不能互相碰撞。mic 從 1 起算，system 從這個值起算。
const SYSTEM_SEGMENT_BASE: u64 = 1 << 32;

/// 即時稿佇列的長度上限（每批 100 ms）。
///
/// 即時稿每 800 ms 重算整個 15 秒視窗，落後超過視窗長度的音訊對它已經
/// 沒有意義了，囤著只是佔記憶體。
const FAST_BACKLOG_CHUNKS: usize = 150;

/// 定稿佇列的長度上限（每批 100 ms），約十分鐘。
///
/// 定稿的內容會進逐字稿，丟掉就是真的少一段，所以給得比即時稿寬得多。
/// 但仍然有界：whisper 卡住時無界佇列會一路長到會議結束。
const SLOW_BACKLOG_CHUNKS: usize = 6_000;

/// 寫檔佇列的長度上限（每批 100 ms），約十分鐘。
///
/// 跟定稿一樣寬：丟掉一批就是原音真的缺一段，而原音的用途正是事後驗證。
/// 但仍然有界 —— 防毒掃描或磁碟卡住時，無界佇列會一路長到會議結束。
const WRITE_BACKLOG_CHUNKS: usize = 6_000;

#[derive(Debug, Clone)]
pub struct ModelPaths {
    pub whisper: String,
    pub paraformer_dir: String,
    pub vad: String,
    /// 標點模型。缺席時逐字稿照樣產生，只是沒有標點。
    pub punct: Option<String>,
    /// 語者分離模型：(segmentation, embedding)。缺席時遠端一律歸為同一位語者。
    pub speaker: Option<(String, String)>,
}

impl ModelPaths {
    /// 找模型檔。環境變數優先，其次是 [`resource_dirs`] 裡的 `models/`。
    ///
    /// 模型有數百 MB，不放進安裝包也不自動下載：使用者需要知道這些檔案在哪、
    /// 佔多少空間，藏起來只會讓「為什麼要一 GB」變成無解的問題。
    pub fn discover() -> Result<Self> {
        let model = |name: &str| find_resource(&format!("models/{name}"));
        let from_env = |var: &str, name: &str| {
            std::env::var(var).unwrap_or_else(|_| model(name).to_string_lossy().into_owned())
        };

        let whisper = from_env("OMN_WHISPER_MODEL", "ggml-large-v3-turbo-q5_0.bin");
        let paraformer_dir = from_env("OMN_PARAFORMER_DIR", "sherpa-onnx-paraformer-zh-2023-09-14");

        if !std::path::Path::new(&whisper).exists() {
            return Err(SttError::Load(format!("找不到定稿模型：{whisper}")));
        }
        if !std::path::Path::new(&paraformer_dir).is_dir() {
            return Err(SttError::Load(format!(
                "找不到即時稿模型：{paraformer_dir}"
            )));
        }
        let vad = from_env("OMN_VAD_MODEL", "silero_vad.onnx");
        if !std::path::Path::new(&vad).exists() {
            return Err(SttError::Load(format!("找不到語音偵測模型：{vad}")));
        }
        let punct = std::env::var("OMN_PUNCT_MODEL")
            .ok()
            .or_else(|| {
                let p = model("sherpa-onnx-punct-ct-transformer/model.onnx");
                p.exists().then(|| p.to_string_lossy().into_owned())
            })
            .filter(|p| std::path::Path::new(p).exists());

        // 兩個模型缺一不可：只有聲紋沒有 segmentation 就找不出語者切換點，
        // 一段裡混了好幾個人的聲紋比對出來只會是亂數。
        let seg_model = model("speaker-segmentation.onnx");
        let emb_model = model("speaker-embedding.onnx");
        let speaker = (seg_model.exists() && emb_model.exists()).then(|| {
            (
                seg_model.to_string_lossy().into_owned(),
                emb_model.to_string_lossy().into_owned(),
            )
        });

        Ok(Self {
            whisper,
            paraformer_dir,
            vad,
            punct,
            speaker,
        })
    }
}

/// 兩軌最近的音量，給畫面上的音量條用。
///
/// 音訊執行緒寫、事件迴圈讀，兩邊都不能互相阻塞，所以用原子值而不是鎖。
/// f32 以 bits 存放：這裡只搬運不做運算。
#[derive(Default)]
struct Levels {
    mic: AtomicU32,
    system: AtomicU32,
}

impl Levels {
    /// 峰值取大、其餘衰減。
    ///
    /// 直接顯示每個 chunk 的 RMS 會讓音量條在字與字的間隙掉到底，看起來像
    /// 收音斷斷續續。取大讓尖峰立刻反映，衰減讓它平順地掉回去。
    fn observe(&self, track: Track, rms: f32) {
        const DECAY: f32 = 0.75;
        // RMS 0.3 當滿格：真人對著麥克風講話實測在 0.05 到 0.3 之間
        let now = (rms / 0.3).min(1.0);
        let cell = match track {
            Track::Mic => &self.mic,
            Track::System => &self.system,
        };
        let prev = f32::from_bits(cell.load(Ordering::Relaxed));
        let next = if now > prev { now } else { prev * DECAY };
        cell.store(next.to_bits(), Ordering::Relaxed);
    }

    fn read(&self) -> (f32, f32) {
        (
            f32::from_bits(self.mic.load(Ordering::Relaxed)),
            f32::from_bits(self.system.load(Ordering::Relaxed)),
        )
    }
}

pub struct LocalSttSource {
    rx: Receiver<TranscriptInput>,
    capture: Box<dyn AudioCapture>,
    workers: Vec<JoinHandle<()>>,
    levels: Arc<Levels>,
    paused: Arc<AtomicBool>,
    /// 擷取開始的時刻。裝置比會議時鐘早跑，差多少由 session 問。
    capture_started: Instant,
}

impl LocalSttSource {
    /// 開始擷取並轉錄。
    ///
    /// 模型載入失敗就直接失敗，不退回 fixture：靜默換成假逐字稿會讓使用者
    /// 以為會議被記錄下來了。
    /// `audio_dir` 是這場會議的音訊要寫去哪。`None` 代表不保留原音 —— 那時
    /// 逐字稿就是唯一紀錄，事後無法驗證也無法換模型重跑。
    pub fn start(models: ModelPaths, audio_dir: Option<std::path::PathBuf>) -> Result<Self> {
        // 只有「這個平台沒有實作」才映成 NoCapture。裝置不見了、擷取起不來
        // 都是真的失敗，不該讓上層退回 fixture。
        let mut capture = platform_capture().map_err(|e| match e {
            AudioError::Unsupported(m) => SttError::NoCapture(m),
            other => SttError::Audio(other.to_string()),
        })?;
        let audio_rx = capture
            .start()
            .map_err(|e| SttError::Audio(e.to_string()))?;
        // 裝置從這一刻起就在收音，而會議時鐘要等模型載入完、`start` 回傳
        // 之後才開始跑。兩者都以實時前進，差的就是這中間的長度。
        let capture_started = Instant::now();
        log("音訊擷取已啟動");

        let (result_tx, rx) = channel();
        // 兩條引擎佇列都有界。定稿慢於即時速度時，無界佇列會一路長到會議
        // 結束；長會議因此是一次 OOM，而且停止之後還要等整個積壓跑完，畫面
        // 停在「收尾中」十幾分鐘。滿了就丟並記錄，見 audio::Chunk 那一層。
        let (fast_tx, fast_rx) = sync_channel(FAST_BACKLOG_CHUNKS);
        let (slow_tx, slow_rx) = sync_channel(SLOW_BACKLOG_CHUNKS);
        // 保存原音的那一條。跟兩個引擎一樣自己一條 channel 加一條執行緒：
        // 磁碟 I/O 會阻塞，壓在分流的熱路徑上會拖慢即時稿。
        let audio_writer = audio_dir.map(|dir| {
            let (tx, chunk_rx) = sync_channel(WRITE_BACKLOG_CHUNKS);
            let (seg_tx, seg_rx) = channel();
            let worker =
                std::thread::spawn(move || crate::audio::writer::run(chunk_rx, dir, seg_tx));
            (tx, seg_rx, worker)
        });
        let (write_tx, written_rx, write_worker) = match audio_writer {
            Some((tx, rx, w)) => (Some(tx), Some(rx), Some(w)),
            None => {
                log("沒有指定音訊目錄，這場會議不保留原音");
                (None, None, None)
            }
        };
        // 只有真的在寫音訊時才需要回報用的 Sender：多留一個沒人用的
        // Sender 會讓 session 端的 channel 永遠不關閉。
        let audio_tx = write_tx.as_ref().map(|_| result_tx.clone());
        // 引擎在這裡載入完才算開始。載入失敗原本是工作執行緒裡的一行 log
        // 加上 `return`：Session 仍然停在 Recording、健康狀態顯示 ok，而
        // 定稿執行緒死掉還會讓分流停止讀音訊 —— 使用者錄完整場才發現一個字
        // 都沒有。
        let (ready_tx, ready_rx) = channel::<std::result::Result<(), String>>();

        // 兩條執行緒共用定稿進度。即時稿要掛在「下一個還沒定稿的片段」上，
        // 而那個編號只有定稿那邊知道。各自記一份的話即時稿會永遠停在第一個
        // 編號，一路覆蓋掉已經定好的句子。
        let progress = Progress::default();

        // 分流而不是讓兩個引擎搶同一個 Receiver：兩者的消費速度差二十倍，
        // 共用會讓快的那個把音訊吃光，慢的那個永遠拿不到完整片段。
        let levels = Arc::new(Levels::default());
        let paused = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let write_dropped = Arc::new(AtomicU64::new(0));
        let dispatcher = {
            let (levels, paused, dropped) = (levels.clone(), paused.clone(), dropped.clone());
            let write_dropped = write_dropped.clone();
            std::thread::spawn(move || {
                for chunk in audio_rx {
                    // 暫停期間直接丟掉，連引擎都不進。擷取執行緒繼續跑是
                    // 刻意的：重開音訊裝置要一到兩秒，恢復錄音時使用者會
                    // 掉頭幾個字。
                    if paused.load(Ordering::Relaxed) {
                        continue;
                    }
                    // 音量在這裡取：每個 chunk 都會經過，而且還沒被任何
                    // 閘門篩掉，畫面上的音量條反映的是真的收到什麼
                    levels.observe(chunk.track, rms(&chunk.samples));
                    // 原音先送去寫檔。它不受任何閘門影響：閘門判斷的是
                    // 「要不要送去轉錄」，而事後要驗證的恰好包含被擋掉的那些。
                    if let Some(w) = &write_tx {
                        match w.try_send(chunk.clone()) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => note_drop(&write_dropped, "音訊寫入"),
                            // 寫檔那一條收掉了原音就沒得留，但逐字稿還在跑，
                            // 不能因此停止分流
                            Err(TrySendError::Disconnected(_)) => {}
                        }
                    }
                    // 即時稿是拋棄式的，滿了就丟那一批：它每 800 ms 重算
                    // 整個視窗，少一批不會留下痕跡
                    let _ = fast_tx.try_send(chunk.clone());
                    match slow_tx.try_send(chunk) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => note_drop(&dropped, "定稿"),
                        // 定稿執行緒沒了就沒有逐字稿，繼續分流沒有意義
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
            })
        };

        let fast = {
            let (dir, vad, tx, progress, ready) = (
                models.paraformer_dir.clone(),
                models.vad.clone(),
                result_tx.clone(),
                progress.clone(),
                ready_tx.clone(),
            );
            std::thread::spawn(move || partial_loop(fast_rx, tx, &dir, &vad, progress, &ready))
        };
        let slow = {
            let (models, tx) = (models.clone(), result_tx);
            std::thread::spawn(move || final_loop(slow_rx, tx, &models, progress, &ready_tx))
        };

        // 落地的音訊段走逐字稿同一條 channel 回 session。音訊執行緒不碰
        // SQLite（§12），由 session 配發 seq 並寫入事件序列。
        let mut workers = vec![dispatcher, fast, slow];
        if let (Some(tx), Some(written_rx), Some(w)) = (audio_tx, written_rx, write_worker) {
            workers.push(std::thread::spawn(move || {
                for seg in written_rx {
                    log(&format!(
                        "{} 軌音訊落地：{} ms 到 {} ms → {}",
                        seg.track.as_str(),
                        seg.captured_start_ms,
                        seg.captured_end_ms,
                        seg.path
                    ));
                    if tx
                        .send(TranscriptInput::AudioSegment {
                            track: seg.track,
                            path: seg.path,
                            captured_span: (seg.captured_start_ms, seg.captured_end_ms),
                            checksum: seg.checksum,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
            workers.push(w);
        }

        let source = Self {
            rx,
            capture,
            workers,
            levels,
            paused,
            capture_started,
        };
        // 兩個引擎都回報載入結果才回傳。失敗時 `source` 在這裡被 drop，
        // 它的 Drop 會停掉擷取並 join 三條執行緒 —— 開了一半的狀態不留下。
        if let Err(e) = await_engines(&ready_rx) {
            drop(source);
            return Err(SttError::Load(e));
        }
        log("音訊擷取與轉錄引擎都已就緒");
        Ok(source)
    }
}

/// 等兩個引擎回報載入結果。
///
/// 沒有時限：載入是純本機的檔案讀取與記憶體配置，慢是因為模型有一 GB，
/// 不是因為在等誰回答。設一個時限只會在慢一點的磁碟上把成功變成失敗。
fn await_engines(
    ready: &Receiver<std::result::Result<(), String>>,
) -> std::result::Result<(), String> {
    let mut failures = Vec::new();
    for _ in 0..2 {
        match ready.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => failures.push(e),
            // 執行緒還沒回報就消失了，那本身就是失敗
            Err(_) => failures.push("轉錄執行緒沒有回報就結束了".to_owned()),
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    Err(failures.join("；"))
}

impl Drop for LocalSttSource {
    fn drop(&mut self) {
        self.capture.stop();
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

impl TranscriptSource for LocalSttSource {
    /// 只取已經算好的結果，不在這裡做任何轉錄。
    ///
    /// 這個方法在 session 的事件迴圈上被呼叫，阻塞它等於卡住整個會議狀態機。
    fn poll(&mut self, _window_ms: u64) -> Vec<TranscriptInput> {
        self.rx.try_iter().collect()
    }

    /// 模型載入期間裝置已經在收音了，這段長度是兩個時鐘的固定落差。
    ///
    /// whisper 那顆模型有五百多 MB，載入要好幾秒到數十秒。不扣掉它，每一段
    /// 音訊的會議時間都會晚上這麼多，而且每場會議都會發生，不只暫停時。
    fn captured_before_attach_ms(&self) -> u64 {
        self.capture_started.elapsed().as_millis() as u64
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    /// 停掉擷取，讓三條工作執行緒把手上的音訊處理完。
    ///
    /// 不 join：這個方法跑在 session 的事件迴圈上，等在這裡會讓整個 UI
    /// 凍住十幾秒。改由 `drained` 輪詢，結果照常從 `poll` 流出來。
    fn begin_flush(&mut self) {
        self.capture.stop();
    }

    fn drained(&self) -> bool {
        self.workers.iter().all(|h| h.is_finished())
    }

    fn levels(&self) -> Option<(f32, f32)> {
        Some(self.levels.read())
    }
}

fn speaker_of(track: Track) -> &'static str {
    match track {
        // 麥克風軌一定是使用者本人，這是不需要模型就能知道的事
        Track::Mic => "me",
        // 遠端先當成單一語者，diarization 是 M3 的工作
        Track::System => "s1",
    }
}

fn segment_id(track: Track, n: u64) -> u64 {
    match track {
        Track::Mic => n + 1,
        Track::System => SYSTEM_SEGMENT_BASE + n + 1,
    }
}

/// 每軌累積中的音訊與它已經定稿幾段。
#[derive(Default)]
struct TrackBuffer {
    samples: Vec<f32>,
    /// 連續幾批沒有人聲。每批 100 ms。
    silent_batches: u32,
}

/// 即時稿迴圈：每 [`PARTIAL_EVERY_MS`] 對目前累積的音訊重跑一次。
///
/// 重跑整段而不是接續解碼，是因為 Paraformer 是離線模型：它每次都要看完整段。
/// 代價是同一段音訊會被算很多次，但 RTF 0.033 讓這個浪費完全可以接受。
fn partial_loop(
    rx: Receiver<Chunk>,
    tx: Sender<TranscriptInput>,
    model_dir: &str,
    vad_model: &str,
    progress: Progress,
    ready: &Sender<std::result::Result<(), String>>,
) {
    let mut engine = match Paraformer::load(model_dir, 2) {
        Ok(e) => {
            log(&format!("即時稿引擎已載入：{model_dir}"));
            let _ = ready.send(Ok(()));
            e
        }
        Err(e) => {
            let msg = format!("即時稿引擎載入失敗（{model_dir}）：{e}");
            log(&msg);
            let _ = ready.send(Err(msg));
            return;
        }
    };
    // 即時稿也要過濾雜音：沒有這一層，鍵盤聲與呼吸聲會被轉成「嗯」之類的字
    // 掛在畫面上。VAD 載入失敗時照樣繼續，只是會多出那些雜訊字。
    let mut vads: HashMap<Track, SileroVad> = HashMap::new();
    let mut vad_ok = true;
    let mut buffers: HashMap<Track, TrackBuffer> = HashMap::new();
    let mut last_run: HashMap<Track, u64> = HashMap::new();

    // 一次把積壓的音訊全部收進來再跑一次引擎，不是一批跑一次。
    // 每批只有 100 ms 但重跑整個視窗要數百毫秒，逐批處理會讓落後持續累積，
    // 畫面看起來就停在幾十秒前不動了。
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        for chunk in batch {
            let track = chunk.track;

            // 只留有人聲的部分。順帶讓視窗裝得下更多實際內容：
            // 一段安靜的會議不會把視窗塞滿靜音。
            let vad = if vad_ok {
                match vads.entry(track) {
                    std::collections::hash_map::Entry::Occupied(e) => Some(e.into_mut()),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        match SileroVad::new(vad_config(vad_model), 30.0) {
                            Ok(v) => Some(e.insert(v)),
                            Err(err) => {
                                log(&format!("即時稿的語音偵測載入失敗：{err}"));
                                vad_ok = false;
                                None
                            }
                        }
                    }
                }
            } else {
                None
            };
            // 判斷這批有沒有人聲，但存進 buffer 的一律是原始音訊：
            // 剪掉靜音會讓 Paraformer 看到跳接的輸入。
            //
            // 這裡要的是串流判斷，不是 voiced_ms。進來的是 100 ms 的 chunk，而
            // VAD 認定一段語音至少要 250 ms，每個 chunk 都重置狀態的話永遠湊不
            // 滿（實測連續講話十秒、一百個 chunk，voiced_ms 判有人聲 0 次，改用
            // is_speech 是 94 次）。跨 chunk 累積正是串流偵測器該做的事。
            let has_voice = match vad {
                Some(v) => {
                    // 一次一個 window：sherpa 的 VAD 整段丟進去只處理第一個
                    for w in chunk.samples.chunks(VAD_WINDOW) {
                        v.accept_waveform(w.to_vec());
                    }
                    v.is_speech()
                }
                None => true,
            };

            let buf = buffers.entry(track).or_default();
            buf.samples.extend_from_slice(&chunk.samples);
            if !has_voice && buf.silent_batches < u32::MAX {
                buf.silent_batches += 1;
            }
            if has_voice {
                buf.silent_batches = 0;
            }
            // 連續三秒沒有人聲就清空視窗：這句話已經講完，讓下一句從頭開始，
            // 而不是把三十秒前的殘句一直掛在畫面上重算
            if buf.silent_batches * 100 >= 3_000 {
                buf.samples.clear();
                buf.silent_batches = 0;
                continue;
            }
            if !has_voice {
                continue;
            }

            // 滑動視窗：超過上限就丟掉最舊的，讓每輪的計算量維持固定
            let max_samples = (PARTIAL_WINDOW_MS * SAMPLE_RATE as u64 / 1000) as usize;
            if buf.samples.len() > max_samples {
                buf.samples.drain(..buf.samples.len() - max_samples);
            }

            // 節流看的是擷取時間軸，不是 buffer 長度。後者被上面的滑動視窗剪在
            // PARTIAL_WINDOW_MS，一旦講滿一次視窗就永遠停在上限，`now - last_run`
            // 從此小於間隔，該軌的即時稿整場不再更新（連續講十五秒就會發生，
            // 也就是每場會議開頭一分鐘內）。
            let now = chunk.captured_start_ms;
            if now.saturating_sub(*last_run.get(&track).unwrap_or(&0)) < PARTIAL_EVERY_MS {
                continue;
            }
            last_run.insert(track, now);

            let text: String = engine
                .tokens(&buf.samples)
                .iter()
                .map(|t| t.text.as_str())
                .collect();
            if text.trim().is_empty() {
                continue;
            }
            // 掛在下一個還沒定稿的編號上。定稿那邊推進之後，這裡下一輪就會跟上，
            // 已經定稿的句子不會再被即時稿蓋回去。
            let id = segment_id(track, progress.of(track).load(Ordering::Relaxed));
            if tx
                .send(TranscriptInput::Partial {
                    segment_id: id,
                    speaker_id: speaker_of(track).into(),
                    text: super::diff::to_traditional(&text, &Default::default()),
                    track,
                })
                .is_err()
            {
                return;
            }
        }
    }
}

/// 定稿迴圈：由 VAD 決定切點，一個語音段送一次 whisper。
///
/// 不用固定長度切窗：機械地每十二秒切一刀，切點會落在句子中間，whisper 只能
/// 在殘句內部加標點，結果就是斷句與標點都不成形。VAD 知道哪裡是語音結束
/// （靜音超過 `min_silence_duration`），在那裡切每段就是完整的一句或幾句話。
///
/// 附帶好處是講完一句就能定稿，不必等滿一個固定視窗。
fn final_loop(
    rx: Receiver<Chunk>,
    tx: Sender<TranscriptInput>,
    models: &ModelPaths,
    progress: Progress,
    ready: &Sender<std::result::Result<(), String>>,
) {
    let (model, vad_model) = (models.whisper.as_str(), models.vad.as_str());
    let punct_model = models.punct.as_deref();
    let speaker_model = models
        .speaker
        .as_ref()
        .map(|(a, b)| (a.as_str(), b.as_str()));
    let engine = match Whisper::load(model, 4) {
        Ok(e) => {
            log(&format!("定稿引擎已載入：{model}"));
            let _ = ready.send(Ok(()));
            e
        }
        Err(e) => {
            let msg = format!("定稿引擎載入失敗（{model}）：{e}");
            log(&msg);
            let _ = ready.send(Err(msg));
            return;
        }
    };

    // 標點是可選的：沒有它逐字稿照樣產生，只是讀起來像一長串。
    // whisper 自己會加一些，但它只看得到單一個切片，句子被切斷的地方就加不出來。
    let mut punct = punct_model.and_then(|m| {
        match Punctuation::new(PunctuationConfig {
            model: m.to_owned(),
            num_threads: Some(1),
            provider: None,
            debug: false,
        }) {
            Ok(p) => {
                log(&format!("標點模型已載入：{m}"));
                Some(p)
            }
            Err(e) => {
                log(&format!("標點模型載入失敗，逐字稿將沒有標點：{e}"));
                None
            }
        }
    });

    // 詞表跟模型走同一組搜尋路徑，使用者自己維護。找不到就只有內建的簡繁詞彙。
    let vocab = {
        let p = find_resource("vocabulary.txt");
        if p.exists() {
            log(&format!("詞表已載入：{}", p.display()));
        }
        Corrections::from_file(&p)
    };

    // 只有系統音訊軌需要辨識語者：麥克風軌一定是使用者本人，那是不需要
    // 模型就成立的先驗，再去比對聲紋只會製造把自己認成別人的機會。
    let mut speakers =
        speaker_model.and_then(|(_seg, emb)| match SpeakerBook::load(vad_model, emb) {
            Ok(b) => {
                log(&format!("語者辨識已載入：{emb}"));
                Some(b)
            }
            Err(e) => {
                log(&format!("語者分離載入失敗，遠端將歸為同一位語者：{e}"));
                None
            }
        });

    // 每軌一個 VAD：兩軌的語音活動彼此獨立，共用會把遠端的說話算成本機也在說話。
    let mut vads: HashMap<Track, SileroVad> = HashMap::new();
    // 閘門用的是另一組實例。切點判斷靠的是跨 chunk 累積的串流狀態，而閘門
    // 每批都得從乾淨狀態重算，兩者共用一個實例會互相破壞。
    let mut gates: HashMap<Track, SileroVad> = HashMap::new();
    // 每軌各自估底噪：麥克風收的是房間噪音，系統音訊收的是會議軟體的
    // 靜音底，兩者差一個數量級，共用一個估計會讓其中一軌的門檻完全失準。
    let mut floors: HashMap<Track, NoiseFloor> = HashMap::new();
    let mut buffers: HashMap<Track, FinalBuffer> = HashMap::new();

    for chunk in rx {
        let track = chunk.track;
        let vad = match vads.entry(track) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                match SileroVad::new(vad_config(vad_model), 40.0) {
                    Ok(v) => e.insert(v),
                    Err(err) => {
                        log(&format!("語音偵測載入失敗，定稿停止：{err}"));
                        return;
                    }
                }
            }
        };
        let buf = buffers.entry(track).or_default();

        // 累積的是原始連續音訊，靜音一起留著。把 VAD 切出的語音段剪接起來
        // 送轉錄是行不通的：接縫會產生原本不存在的音訊變化，whisper 只認得出
        // 殘句（實測轉出「那今天感謝。」這種東西）。VAD 在這裡只回答
        // 「現在有沒有人在講話」，用來決定切點。
        if buf.samples.is_empty() {
            buf.start_ms = chunk.captured_start_ms;
        }
        buf.samples.extend_from_slice(&chunk.samples);

        // 一次只餵一個 window。sherpa-onnx 的 VAD 是串流偵測器，整段丟進去
        // 它只會處理第一個 window，其餘直接丟掉（實測不論音訊多長都固定
        // 回報 314 ms）。這裡的分批不是效能考量，是這個 API 唯一正確的用法。
        for w in chunk.samples.chunks(VAD_WINDOW) {
            vad.accept_waveform(w.to_vec());
            while !vad.is_empty() {
                buf.voiced_samples += vad.front().samples.len();
                vad.pop();
            }
        }
        let speaking = vad.is_speech();

        let buffered_ms = buf.samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE);

        // 對方還在講就別切，會把句子攔腰截斷。除非已經長到 whisper 的
        // context（30 秒）快裝不下，那時寧可切壞一句也不能讓整段被默默截掉。
        let must_cut = buffered_ms >= MAX_SEGMENT_MS;
        if !must_cut && (speaking || buffered_ms < MIN_BATCH_MS) {
            continue;
        }

        let batch = std::mem::take(&mut buf.samples);
        buf.voiced_samples = 0;
        let batch_start_ms = buf.start_ms;

        // 用能量而不是 VAD 的累計長度來判斷這批值不值得轉錄。
        //
        // VAD 是串流狀態機：一段語音只在結束時才吐出來，跨越批次邊界的語音
        // 會讓前一批算到 0、後一批算到超過批次長度的數字（實測 8400 ms 的
        // 批次回報 22934 ms 人聲）。拿那個數字當閘門會把真實內容整段丟掉。
        // RMS 是無狀態的，同一段音訊算幾次都一樣。
        let level = rms(&batch);
        let floor = floors.entry(track).or_default();
        floor.observe(level);

        // 數位靜音直接跳過，純粹是為了不浪費 VAD 的計算；判斷本身不靠它。
        // 這條線壓得比任何真實訊號都低，不會有東西因為「太小聲」被砍掉。
        if level < SILENT_RMS {
            log(&format!(
                "{} 軌略過 {buffered_ms} ms（起點 {batch_start_ms} ms，RMS {level:.4}，近乎靜音）",
                track.as_str()
            ));
            continue;
        }

        // 值不值得轉錄由 VAD 決定，不由音量決定。
        //
        // 這兩件事在能量上分不開：真機量到的環境噪音 RMS 0.0058 到 0.0094，
        // 跟小聲說話重疊，whisper 拿到純噪音會編出「好」這種字（兩小時錄音
        // 的麥克風軌 83 段幾乎都是這樣來的）。而 VAD 判的是內容：實測純噪音
        // 從 RMS 0.0061 一路到 0.4325（七十倍）全部判 0 ms 人聲，把真實語音
        // 縮到 RMS 0.0107 仍判 6406 ms，語音混進 RMS 0.1284 的噪音裡照樣
        // 判得出 6906 ms。
        //
        // 所以能量門檻不排在它前面。排前面的話一旦誤擋，VAD 沒有機會救，
        // 而它只省得下 RTF 0.0022 的計算，不值得拿真實發言去換。
        let voiced = match gates.entry(track) {
            std::collections::hash_map::Entry::Occupied(e) => voiced_ms(e.into_mut(), &batch),
            std::collections::hash_map::Entry::Vacant(e) => {
                match SileroVad::new(gate_config(vad_model), 40.0) {
                    Ok(v) => voiced_ms(e.insert(v), &batch),
                    // VAD 載不起來時退回能量判斷。它是這裡唯一還剩的訊號，所以
                    // 用跟著環境跑的門檻而不是固定值 —— 固定值在安靜房間會吃掉
                    // 小聲說的話，在吵雜場地又形同虛設。
                    Err(err) => {
                        let t = floor.threshold();
                        log(&format!(
                            "閘門用的語音偵測載入失敗，改為只看能量（門檻 {t:.4}）：{err}"
                        ));
                        if level < t {
                            0
                        } else {
                            buffered_ms
                        }
                    }
                }
            }
        };
        let voiced_pct = voiced as f64 * 100.0 / buffered_ms.max(1) as f64;
        // 能量擋的是「微弱到不可能是有意義發言、但 VAD 仍判得出結構」的音訊：
        // 外放的遠端回音、隔壁房間的說話聲。whisper 在那上面會編出整段不存在
        // 的內容（實測 RMS 0.0042 產出七句，包括「我想要吃飯」這種跟會議毫無
        // 關係的句子）。
        //
        // 兩個來源取大：SPEECH_FLOOR 是真實發言的絕對下限（實測幻覺最高
        // 0.0050、小聲語音 0.0107、真實批次最低 0.0628），底噪估計則讓
        // 吵雜場地自動提高標準。
        let speech_floor = SPEECH_FLOOR.max(floors.entry(track).or_default().threshold());
        match gate(voiced, buffered_ms, level, speech_floor) {
            Gate::Send => {}
            Gate::TooQuiet => {
                log(&format!(
                    "{} 軌略過 {buffered_ms} ms（起點 {batch_start_ms} ms，RMS {level:.4} < 發言下限 {speech_floor:.4}）",
                    track.as_str()
                ));
                continue;
            }
            Gate::TooSparse => {
                log(&format!(
                    "{} 軌略過 {buffered_ms} ms（起點 {batch_start_ms} ms，RMS {level:.4}，人聲僅 {voiced_pct:.0}%、{voiced} ms）",
                    track.as_str()
                ));
                continue;
            }
        }

        match engine.transcribe(&batch) {
            Ok(segments) => {
                // 能量閘門擋不住的那一種：環境噪音的 RMS 可以剛好高過門檻，
                // whisper 在上面編出字幕組署名。兩小時實測漏過兩筆，都在
                // 沒人說話的麥克風軌上。
                let texts: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
                if crate::stt::is_hallucination(&texts, level) {
                    log(&format!(
                        "{} 軌丟棄幻覺（起點 {batch_start_ms} ms，RMS {level:.4}）：{texts:?}",
                        track.as_str()
                    ));
                    continue;
                }
                log(&format!(
                    "{} 軌定稿：起點 {batch_start_ms} ms，{buffered_ms} ms 音訊（RMS {level:.4}，人聲 {voiced_pct:.0}%）→ {} 句 {:?}",
                    track.as_str(),
                    segments.len(),
                    segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>()
                ));
                let counter = progress.of(track);
                let n = counter.load(Ordering::Relaxed);
                // 只有系統音訊軌需要分離語者：麥克風軌一定是使用者本人，
                // 那是不需要模型就成立的先驗，再去比對只會製造認錯的機會。
                let spans = match (track, speakers.as_mut()) {
                    (Track::System, Some(book)) => book.split(&batch),
                    _ => Vec::new(),
                };
                if !spans.is_empty() {
                    let names: std::collections::BTreeSet<&str> =
                        spans.iter().map(|s| s.speaker.as_str()).collect();
                    log(&format!("語者分離：{} 段、{:?}", spans.len(), names));
                }
                let mut ctx = FinalContext {
                    track,
                    finalized: n,
                    batch_start_ms,
                    punct: punct.as_mut(),
                    spans: &spans,
                    vocab: &vocab,
                };
                match emit_final(&tx, &mut ctx, &segments) {
                    Some(sent) => counter.store(n + sent, Ordering::Relaxed),
                    None => return,
                }
            }
            Err(e) => log(&format!("定稿失敗，這段維持即時稿：{e}")),
        }
    }

    // 音訊結束了，但每軌的 buffer 裡還積著不足一個批次的音訊。不處理的話
    // 使用者按下結束會議之前講的最後八到十五秒會直接消失 —— 而那通常是
    // 結語或結論，正是最不能掉的部分。
    for (track, buf) in buffers.iter_mut() {
        if buf.samples.is_empty() {
            continue;
        }
        let batch = std::mem::take(&mut buf.samples);
        let buffered_ms = batch.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
        let level = rms(&batch);
        if level < SILENT_RMS {
            continue;
        }
        let voiced = match gates.get_mut(track) {
            Some(v) => voiced_ms(v, &batch),
            None => buffered_ms,
        };
        if voiced == 0 {
            continue;
        }
        let Ok(segments) = engine.transcribe(&batch) else {
            continue;
        };
        let texts: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
        if crate::stt::is_hallucination(&texts, level) {
            continue;
        }
        log(&format!(
            "{} 軌收尾：起點 {} ms，{buffered_ms} ms 音訊 → {} 句",
            track.as_str(),
            buf.start_ms,
            segments.len()
        ));
        let spans = match (*track, speakers.as_mut()) {
            (Track::System, Some(book)) => book.split(&batch),
            _ => Vec::new(),
        };
        let counter = progress.of(*track);
        let n = counter.load(Ordering::Relaxed);
        let mut ctx = FinalContext {
            track: *track,
            finalized: n,
            batch_start_ms: buf.start_ms,
            punct: punct.as_mut(),
            spans: &spans,
            vocab: &vocab,
        };
        if let Some(sent) = emit_final(&tx, &mut ctx, &segments) {
            counter.store(n + sent, Ordering::Relaxed);
        }
    }
}

/// 定稿用的累積區：原始連續音訊，外加其中有多少樣本被判定為人聲。
#[derive(Default)]
struct FinalBuffer {
    samples: Vec<f32>,
    voiced_samples: usize,
    /// 這批音訊在該軌擷取音訊中的起點。whisper 給的時間是相對於送進去的
    /// 片段，要加上這個偏移才是會議中的絕對位置。
    start_ms: u64,
}

/// VAD 的設定。
///
/// threshold 偏低配上 min_speech_duration 的長度條件：單發的鍵盤聲、咳嗽、
/// 關門聲會被長度擋掉，而真人講話不可能短於四分之一秒。反過來把 threshold
/// 調高則會漏掉遠場的小聲發言，實測有真人講話的會議音訊只有 -46 dB。
/// 閘門用的設定：比切點更容易認定「這裡有人在說話」。
///
/// 切點要的是穩定的句子邊界，寧可晚切也不要把一句話剖成兩半，250 ms 是
/// 合理的。閘門的代價完全不同 —— 判錯的後果是整批音訊被丟掉，使用者
/// 真的說過的話就此消失。
///
/// 實測一個 8 秒批次裡只有 180 ms 真實語音時：250 ms 判 0（整批被丟），
/// 150 ms 判 322 ms（留得住），而純噪音在兩者之下都是 0。短的單音節回應
/// （「好」「嗯」「對」）與被強制切批切到下一批的詞尾，都落在這個區間。
fn gate_config(model: &str) -> SileroVadConfig {
    SileroVadConfig {
        min_speech_duration: 0.15,
        ..vad_config(model)
    }
}

fn vad_config(model: &str) -> SileroVadConfig {
    SileroVadConfig {
        model: model.to_owned(),
        threshold: 0.4,
        min_speech_duration: 0.25,
        min_silence_duration: 0.5,
        max_speech_duration: 20.0,
        sample_rate: SAMPLE_RATE,
        window_size: 512,
        num_threads: Some(1),
        provider: None,
        debug: false,
    }
}

/// 建一個乾淨的偵測器判斷這段音訊。測試與效能量測用。
pub(crate) fn gate_voiced_ms(vad_model: &str, samples: &[f32]) -> super::Result<u64> {
    let mut v = SileroVad::new(vad_config(vad_model), 40.0)
        .map_err(|e| super::SttError::Load(e.to_string()))?;
    Ok(voiced_ms(&mut v, samples))
}

/// 這段音訊裡有多少毫秒是人聲。
///
/// 只回報長度，不動音訊本身。把靜音剪掉再把語音拼起來送轉錄是行不通的：
/// 接縫處會產生原本不存在的音訊變化，whisper 對跳接的輸入會直接開始編
/// （實測轉出「中文字幕志願者」這種訓練資料殘留）。VAD 在這裡只當閘門。
fn voiced_ms(vad: &mut SileroVad, samples: &[f32]) -> u64 {
    // 一次只餵一個 window。sherpa-onnx 的 VAD 是串流偵測器，整段丟進去
    // 它只會處理第一個 window，其餘直接丟掉（實測不論音訊多長都固定回報
    // 314 ms）。這裡的分批不是效能考量，是這個 API 唯一正確的用法。
    let mut frames = 0usize;
    for w in samples.chunks(VAD_WINDOW) {
        vad.accept_waveform(w.to_vec());
        while !vad.is_empty() {
            frames += vad.front().samples.len();
            vad.pop();
        }
    }
    vad.flush();
    while !vad.is_empty() {
        frames += vad.front().samples.len();
        vad.pop();
    }
    // 每次判斷都從乾淨狀態開始。VAD 是有狀態的串流偵測器，跨輪沿用會讓
    // 上一段的尾巴影響這一段的判定，實測結果是每段都被當成靜音而跳過。
    //
    // clear 只清佇列，模型自己的遞迴狀態要 reset 才會歸零。少了它，一批
    // 真實語音之後的噪音會被算成有人聲（實測 1100 ms，單獨判是 0 ms），
    // 而那正是閘門要擋的東西。
    vad.clear();
    vad.reset();
    frames as u64 * 1000 / SAMPLE_RATE as u64
}

/// VAD 每次處理的樣本數，必須與 `SileroVadConfig::window_size` 一致。
const VAD_WINDOW: usize = 512;

/// 送去定稿前至少要累積這麼長的語音。
///
/// VAD 在每個停頓處都會切一段，直接一段一送的話 whisper 只拿得到一兩秒的
/// 片段，轉出來是「那今天感謝。」這種殘句。累積到接近一句完整的話再送，
/// 模型才有足夠上下文，斷句與標點也才成形。
const MIN_BATCH_MS: u64 = 8_000;

/// 單段定稿的長度上限。
///
/// whisper 的 context 只有 30 秒，超過會被默默截掉。而且一段講太久，
/// 使用者要等到整段結束才看得到定稿。VAD 的 `max_speech_duration` 實測
/// 擋不住連續發言，所以在這裡自己切。
const MAX_SEGMENT_MS: u64 = 15_000;

/// 有意義的發言至少要有這個能量。
///
/// 只在 VAD 已經判定有語音結構之後才套用，所以不會誤殺小聲說的話。
/// 實測的三個錨點：漏網的幻覺最高 0.0050、把真實語音縮到聽得出來的下限
/// 是 0.0107、真實會議批次最低 0.0628。0.008 落在前兩者中間。
const SPEECH_FLOOR: f32 = 0.008;

/// 一個批次至少要有這個比例是人聲。
///
/// VAD 判出「有人聲」還不夠：微弱的環境噪音會讓它判出零星幾百毫秒，而
/// whisper 在那種音訊上會編出正常詞（實測「好」，佔比 4% 與 14%）。
///
/// 20% 取自兩邊的空隙：真實會議的批次實測最低 31%、中位 88%，噪音幻覺
/// 最高 14%。收尾批次不套用這條 —— 那是使用者按下結束前的最後一段，
/// 寧可留著也不要賭。
const MIN_VOICED_PCT: f64 = 20.0;

/// 佔比不足時，湊得出一句話的最低人聲總長。
///
/// 佔比擋得住噪音幻覺，卻連短回應一起擋掉：使用者大部分時間在聽，「對」
/// 「OK」「了解」落在 8 秒批次裡天然只有 12-18%，跟實測幻覺的 4-14% 同一
/// 個區間 —— 一場 295 秒的會議，麥克風軌 27 個批次被擋掉 21 個。
///
/// 分開兩者的訊號是能量而不是佔比（見 [`SPEECH_FLOOR`]：幻覺實測 0.0050
/// 以下，被擋掉的短回應 0.0083 以上），所以這個值只負責擋「零星幾百毫秒」
/// 那一種：500 ms 在實測最零碎的幻覺總長 320 ms 之上，又容得下一個「了解」。
const MIN_VOICED_MS: u64 = 500;

/// 這批音訊要不要送去定稿。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Send,
    /// VAD 判得出語音結構，但能量不到有意義發言的下限
    TooQuiet,
    /// 人聲太零碎，湊不出一句話
    TooSparse,
}

/// 能量先判、佔比後判，兩者都在 VAD 之後。
///
/// 能量放在 VAD 之後是安全考量而非順序偏好：VAD 已經確認裡面有語音結構，
/// 這時再看能量不會誤殺小聲說的話，放在前面才會。
fn gate(voiced_ms: u64, buffered_ms: u64, level: f32, speech_floor: f32) -> Gate {
    if level < speech_floor {
        return Gate::TooQuiet;
    }
    let pct = voiced_ms as f64 * 100.0 / buffered_ms.max(1) as f64;
    if pct >= MIN_VOICED_PCT || voiced_ms >= MIN_VOICED_MS {
        Gate::Send
    } else {
        Gate::TooSparse
    }
}

/// 低於這個能量就當數位靜音，連 VAD 都不必跑。
///
/// 麥克風接著沒人說話的房間，實測 RMS 在 0.0017 以上；這條線壓在它的
/// 三分之一，只會攔到真正沒有訊號的批次。
const SILENT_RMS: f32 = 0.0005;

/// 能量後備門檻的絕對下界與上界（RMS）。
///
/// 只在 VAD 載不起來、能量變成唯一訊號時才會用到。
///
/// 門檻本身是跟著環境跑的（見 [`NoiseFloor`]），這兩個值只負責兜住兩端：
/// 再安靜的房間也不放行 0.003 以下的東西，再吵的場地也不把門檻推到
/// 0.02 以上 —— 那已經高過遠場會議收音的實測值，會開始吃掉真實發言。
const RMS_FLOOR: f32 = 0.003;
const RMS_CEIL: f32 = 0.02;

/// 門檻是底噪的幾倍。
///
/// 1.5 倍約 +3.5 dB。設得比一般噪音閘門低，因為這裡不是唯一的防線：
/// 過了能量這關還有 VAD 判內容，所以寧可讓邊緣的音訊進來給 VAD 判，
/// 也不要在這一層就把小聲說的話砍掉。
const RMS_MARGIN: f32 = 1.5;

/// 這一軌最近的底噪估計。
///
/// 固定門檻是拿一個環境的數字硬套所有環境：安靜辦公室底噪 0.001，
/// 0.005 的門檻會吃掉小聲說的話；吵雜場地底噪 0.01，同一個門檻形同虛設。
///
/// 取第 20 百分位而不是最小值，單一個特別安靜的批次不該把整條線拉歪。
/// 觀察的是所有批次（含被擋掉的），只統計通過的會讓估計逐漸偏高。
struct NoiseFloor {
    recent: std::collections::VecDeque<f32>,
}

impl Default for NoiseFloor {
    fn default() -> Self {
        Self {
            recent: std::collections::VecDeque::with_capacity(Self::WINDOW),
        }
    }
}

impl NoiseFloor {
    /// 約五分鐘的批次量。夠長才不會被一段連續發言帶著跑，夠短才追得上
    /// 換場地、開冷氣這類真的改變了底噪的事。
    const WINDOW: usize = 40;

    fn observe(&mut self, rms: f32) {
        if self.recent.len() == Self::WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(rms);
    }

    /// 這一軌現在該用的能量門檻。
    ///
    /// 樣本還不夠時回下界：寧可放行讓 VAD 判，也不要用兩三批就算出來的
    /// 底噪去擋掉會議剛開始的發言。
    fn threshold(&self) -> f32 {
        if self.recent.len() < 8 {
            return RMS_FLOOR;
        }
        let mut v: Vec<f32> = self.recent.iter().copied().collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let floor = v[v.len() / 5];
        (floor * RMS_MARGIN).clamp(RMS_FLOOR, RMS_CEIL)
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// 送出定稿需要的東西。
///
/// 打包成一個結構而不是攤成八個參數：這些值全都描述「同一批音訊的同一次
/// 定稿」，散開之後呼叫端很容易把 `finalized` 和 `batch_start_ms` 傳反
/// （兩個都是 u64，編譯器不會攔）。
struct FinalContext<'a> {
    track: Track,
    /// 這一軌已經定稿幾句，用來配發 segment_id
    finalized: u64,
    /// 這批音訊在該軌擷取音訊中的起點
    batch_start_ms: u64,
    punct: Option<&'a mut Punctuation>,
    spans: &'a [SpeakerSpan],
    vocab: &'a Corrections,
}

/// 逐句送出定稿，回傳送出的句數；`None` 代表接收端已關閉，呼叫端該收工。
///
/// 一句一段而不是整塊送出：whisper 的 segment 邊界是模型判斷的語意邊界，
/// 合併之後畫面上就會出現一大段沒有換行的文字，而且尾巴常常斷在句中
/// （切片邊界落在句子中間時）。逐句輸出讓每一行都是完整的一句話。
fn emit_final(
    tx: &Sender<TranscriptInput>,
    ctx: &mut FinalContext<'_>,
    segments: &[super::Segment],
) -> Option<u64> {
    let FinalContext {
        track,
        finalized,
        batch_start_ms,
        punct,
        spans,
        vocab,
    } = ctx;
    let (track, finalized, batch_start_ms) = (*track, *finalized, *batch_start_ms);
    let mut sent = 0u64;
    for seg in segments {
        let text = seg.text.trim();
        if text.is_empty() {
            // 空白不佔用 segment 編號，否則畫面上會出現一排空片段
            continue;
        }
        // 標點在轉繁之前套用：標點模型吃的是簡體語料，繁體輸入它認得的詞會變少
        let punctuated = match punct.as_deref_mut() {
            Some(p) => p.add_punctuation(text),
            None => text.to_owned(),
        };
        // 每一句都帶自己的音訊區間。少了它，同一批的每一句會拿到相同的
        // 時間戳，被跨串流去重當成同一段的不同版本而一句蓋掉一句。
        let ok = tx
            .send(TranscriptInput::Final {
                segment_id: segment_id(track, finalized + sent),
                speaker_id: speaker_at(spans, seg.start_ms, seg.end_ms)
                    .unwrap_or(speaker_of(track))
                    .to_owned(),
                text: super::diff::to_traditional(&punctuated, vocab),
                track,
                audio_span: Some((batch_start_ms + seg.start_ms, batch_start_ms + seg.end_ms)),
            })
            .is_ok();
        if !ok {
            return None;
        }
        sent += 1;
    }
    Some(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_two_tracks_never_share_a_segment_id() {
        // 兩軌各自從 0 開始編號，沒有偏移的話第一句就會互相覆蓋
        for n in 0..1000 {
            assert_ne!(segment_id(Track::Mic, n), segment_id(Track::System, n));
        }
    }

    #[test]
    fn test_the_microphone_track_is_always_the_user() {
        // 這是不需要模型就成立的先驗，混音之後就拿不到了
        assert_eq!(speaker_of(Track::Mic), "me");
        assert_ne!(speaker_of(Track::System), "me");
    }

    #[test]
    fn test_the_partial_window_is_bounded() {
        // 沒有上限的話，Paraformer 每輪都要重看整場會議：錄十分鐘就變成
        // 每 800 ms 算十分鐘的音訊，畫面最後會停住
        let max_samples = (PARTIAL_WINDOW_MS * SAMPLE_RATE as u64 / 1000) as usize;
        let mut buf = TrackBuffer::default();
        for _ in 0..100 {
            buf.samples
                .extend(std::iter::repeat_n(0.0f32, SAMPLE_RATE as usize));
            if buf.samples.len() > max_samples {
                buf.samples.drain(..buf.samples.len() - max_samples);
            }
            assert!(buf.samples.len() <= max_samples);
        }
        assert_eq!(buf.samples.len(), max_samples);
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    fn vad() -> Option<SileroVad> {
        let m = std::env::var("OMN_VAD_MODEL")
            .unwrap_or_else(|_| "/home/enor/whisper-bench/silero_vad.onnx".into());
        std::path::Path::new(&m)
            .exists()
            .then(|| SileroVad::new(vad_config(&m), 40.0).expect("載入 VAD"))
    }

    fn noise(amp: f32, secs: usize) -> Vec<f32> {
        let mut seed = 99u64;
        (0..SAMPLE_RATE as usize * secs)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((seed >> 33) as f32 / 4_294_967_296.0 - 0.5) * amp
            })
            .collect()
    }

    /// 這是整個閘門存在的理由：能量分不開的兩種音訊，VAD 分得開。
    #[test]
    fn test_quiet_speech_passes_the_gate_while_louder_noise_does_not() {
        let Some(mut v) = vad() else {
            eprintln!("略過：找不到 VAD 模型");
            return;
        };
        let wav = "/home/enor/whisper-bench/near.wav";
        if !std::path::Path::new(wav).exists() {
            eprintln!("略過：找不到音訊");
            return;
        }
        let sp = crate::stt::load_wav_16k_mono(wav).expect("讀音訊");
        // 把真實語音縮到比噪音還小聲，證明判斷不是靠音量
        let quiet: Vec<f32> = sp[..SAMPLE_RATE as usize * 8]
            .iter()
            .map(|x| x * 0.03)
            .collect();
        let loud_noise = noise(0.045, 8);

        let (q_rms, n_rms) = (rms(&quiet), rms(&loud_noise));
        let q_voiced = voiced_ms(&mut v, &quiet);
        let n_voiced = voiced_ms(&mut v, &loud_noise);
        println!("小聲語音 RMS {q_rms:.4} → {q_voiced} ms；噪音 RMS {n_rms:.4} → {n_voiced} ms");

        assert!(n_rms > q_rms, "測試前提不成立：噪音應該比這段語音大聲");
        assert!(q_voiced > 0, "小聲說的話被閘門擋掉了");
        assert_eq!(n_voiced, 0, "噪音通過了閘門，whisper 會在上面編字");
    }

    #[test]
    fn test_silence_never_passes_the_gate() {
        let Some(mut v) = vad() else { return };
        assert_eq!(voiced_ms(&mut v, &vec![0.0; SAMPLE_RATE as usize * 8]), 0);
    }

    /// 真機量到的環境噪音能量，兩小時錄音裡讓 whisper 反覆轉出「好」
    /// 靜音預篩壓得比任何真實訊號都低，不會有東西因為太小聲被砍掉
    #[test]
    fn test_the_silence_prefilter_sits_below_every_real_signal() {
        // 真機量到的安靜房間麥克風底噪
        const { assert!(SILENT_RMS < 0.0017) };
        // 實測最小聲的真實語音
        const { assert!(SILENT_RMS < 0.0107) };
    }

    #[test]
    fn test_room_tone_at_the_level_that_produced_phantom_words_is_rejected() {
        let Some(mut v) = vad() else { return };
        let room = noise(0.021, 8);
        assert!(
            rms(&room) > RMS_FLOOR,
            "測試前提不成立：這段噪音應該高過能量下界"
        );
        assert_eq!(voiced_ms(&mut v, &room), 0);
    }
}

#[cfg(test)]
mod voiced_ratio_tests {
    use super::*;

    /// 真實會議的每一批都遠高於任何合理的佔比門檻。
    ///
    /// 這是給閘門後續收緊時當基準的：實測最低 40%、中位 88%，代表在這之下
    /// 還有很大的空間可以擋掉偽陽性，而不會碰到真實發言。
    #[test]
    fn test_real_meeting_batches_are_overwhelmingly_voiced() {
        let m = std::env::var("OMN_VAD_MODEL")
            .unwrap_or_else(|_| "/home/enor/whisper-bench/silero_vad.onnx".into());
        let wav = "/home/enor/whisper-bench/multi.wav";
        if !std::path::Path::new(&m).exists() || !std::path::Path::new(wav).exists() {
            eprintln!("略過：找不到素材");
            return;
        }
        let sp = crate::stt::load_wav_16k_mono(wav).expect("讀音訊");
        let mut v = SileroVad::new(vad_config(&m), 40.0).expect("VAD");
        let batch = SAMPLE_RATE as usize * 15;
        let mut lowest = f64::MAX;
        for c in sp.chunks(batch) {
            if c.len() < SAMPLE_RATE as usize * 2 {
                continue;
            }
            let ms = c.len() as u64 * 1000 / SAMPLE_RATE as u64;
            lowest = lowest.min(voiced_ms(&mut v, c) as f64 / ms as f64);
        }
        println!("真實會議最低人聲佔比 {:.1}%", lowest * 100.0);
        assert!(
            lowest > 0.2,
            "真實會議出現低佔比批次 {lowest:.3}，門檻策略要重新評估"
        );
    }
}

#[cfg(test)]
/// 這組門檻只在 VAD 載不起來時才會用到，那時能量是唯一剩下的訊號。
mod noise_floor_tests {
    use super::*;

    fn feed(f: &mut NoiseFloor, level: f32, n: usize) {
        for _ in 0..n {
            f.observe(level);
        }
    }

    #[test]
    fn test_a_cold_start_uses_the_floor_instead_of_a_half_formed_estimate() {
        let mut f = NoiseFloor::default();
        feed(&mut f, 0.5, 3);
        // 三批連續發言不該把門檻推到 0.75，會議開頭的話會整段消失
        assert_eq!(f.threshold(), RMS_FLOOR);
    }

    #[test]
    fn test_a_quiet_room_lowers_the_gate_below_the_old_fixed_value() {
        let mut f = NoiseFloor::default();
        feed(&mut f, 0.001, 20);
        // 舊的固定門檻是 0.005，在這種環境會吃掉小聲說的話
        assert!(f.threshold() < 0.005, "安靜環境的門檻沒有降下來");
        assert_eq!(f.threshold(), RMS_FLOOR);
    }

    #[test]
    fn test_a_noisy_room_raises_the_gate_above_the_room_tone() {
        let mut f = NoiseFloor::default();
        // 真機量到的麥克風底噪
        feed(&mut f, 0.006, 20);
        let t = f.threshold();
        assert!(t > 0.006, "門檻沒有高過底噪，噪音會直接通過");
        // 35 分鐘實測漏掉的那一筆
        assert!(0.0058 < t, "漏網的那批噪音仍然會通過");
    }

    #[test]
    fn test_the_gate_never_climbs_high_enough_to_swallow_real_speech() {
        let mut f = NoiseFloor::default();
        feed(&mut f, 1.0, 20);
        assert_eq!(f.threshold(), RMS_CEIL);
        // 遠場會議收音實測在 0.05 以上，上界必須遠低於它。
        // 這條在編譯期就該成立，改動常數時直接編不過而不是等測試跑。
        const { assert!(RMS_CEIL < 0.05) };
    }

    #[test]
    fn test_a_burst_of_speech_does_not_drag_the_estimate_up() {
        let mut f = NoiseFloor::default();
        feed(&mut f, 0.004, 24);
        feed(&mut f, 0.3, 15);
        // 第 20 百分位仍落在底噪那一段，連續發言不該讓後續的話被擋
        assert!(
            f.threshold() <= 0.006,
            "一段連續發言把門檻帶上去了：{}",
            f.threshold()
        );
    }

    #[test]
    fn test_the_estimate_follows_the_room_when_it_actually_changes() {
        let mut f = NoiseFloor::default();
        feed(&mut f, 0.001, 40);
        let quiet = f.threshold();
        // 換到吵雜場地，視窗滿了之後估計要跟上
        feed(&mut f, 0.01, 40);
        assert!(f.threshold() > quiet, "換環境之後門檻沒有跟上");
    }
}

#[cfg(test)]
mod gate_order_probe {
    use super::*;

    fn noise(amp: f32, n: usize, seed0: u64) -> Vec<f32> {
        let mut seed = seed0;
        (0..n)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((seed >> 33) as f32 / 4_294_967_296.0 - 0.5) * amp
            })
            .collect()
    }

    #[test]
    fn probe_vad_reliability_in_loud_rooms() {
        let m = "/home/enor/whisper-bench/silero_vad.onnx";
        let wav = "/home/enor/whisper-bench/near.wav";
        if !std::path::Path::new(m).exists() || !std::path::Path::new(wav).exists() {
            eprintln!("略過");
            return;
        }
        let mut v = SileroVad::new(vad_config(m), 40.0).expect("VAD");
        let n = SAMPLE_RATE as usize * 8;
        let sp = crate::stt::load_wav_16k_mono(wav).expect("讀音訊");

        println!("== 純噪音，能量從安靜到極吵 ==");
        for amp in [0.021f32, 0.07, 0.2, 0.6, 1.5] {
            let x = noise(amp, n, 7);
            println!("  RMS {:.4} → 人聲 {} ms", rms(&x), voiced_ms(&mut v, &x));
        }

        println!("== 語音混進噪音（語音固定，噪音越來越大）==");
        for amp in [0.0f32, 0.05, 0.15, 0.4] {
            let bg = noise(amp, n, 11);
            let mix: Vec<f32> = sp[..n].iter().zip(&bg).map(|(a, b)| a * 0.15 + b).collect();
            println!(
                "  噪音 amp {amp} → RMS {:.4}、人聲 {} ms",
                rms(&mix),
                voiced_ms(&mut v, &mix)
            );
        }
    }
}

#[cfg(test)]
mod streaming_vs_batch_tests {
    use super::*;

    fn vad(buf_s: f32) -> Option<SileroVad> {
        let m = std::env::var("OMN_VAD_MODEL")
            .unwrap_or_else(|_| "/home/enor/whisper-bench/silero_vad.onnx".into());
        std::path::Path::new(&m)
            .exists()
            .then(|| SileroVad::new(vad_config(&m), buf_s).expect("VAD"))
    }

    fn speech() -> Option<Vec<f32>> {
        let w = "/home/enor/whisper-bench/near.wav";
        std::path::Path::new(w)
            .exists()
            .then(|| crate::stt::load_wav_16k_mono(w).expect("讀音訊"))
    }

    /// 即時稿收到的是 100 ms 的 chunk，比 VAD 認定的最短語音（250 ms）還短。
    /// 每個 chunk 都重置狀態就永遠湊不滿，畫面上不會有字。
    #[test]
    fn test_streaming_chunks_detect_speech_only_when_state_carries_across_them() {
        let (Some(mut v), Some(sp)) = (vad(30.0), speech()) else {
            eprintln!("略過：找不到素材");
            return;
        };
        let chunk = SAMPLE_RATE as usize / 10;
        let hits = (0..100)
            .filter(|i| {
                for w in sp[i * chunk..(i + 1) * chunk].chunks(VAD_WINDOW) {
                    v.accept_waveform(w.to_vec());
                }
                v.is_speech()
            })
            .count();
        println!("串流判斷：100 個 chunk 命中 {hits} 次");
        assert!(
            hits > 50,
            "連續講話十秒卻幾乎判不到人聲（{hits}/100），即時稿會是空的"
        );
    }

    /// 同一段音訊走每批重置的 helper 會判不出來，這正是它不該用在即時稿的原因
    #[test]
    fn test_the_batch_helper_is_wrong_for_chunks_shorter_than_min_speech() {
        let (Some(mut v), Some(sp)) = (vad(30.0), speech()) else {
            return;
        };
        let chunk = SAMPLE_RATE as usize / 10;
        let hits = (0..100)
            .filter(|i| voiced_ms(&mut v, &sp[i * chunk..(i + 1) * chunk]) > 0)
            .count();
        assert_eq!(hits, 0, "這個 helper 對 100 ms 輸入應該一律判不到");
    }

    /// 定稿批次夠長，重置反而是必要的：上一批的語音不該影響這一批
    #[test]
    fn test_the_batch_helper_is_right_for_full_length_batches() {
        let (Some(mut v), Some(sp)) = (vad(40.0), speech()) else {
            return;
        };
        let batch = SAMPLE_RATE as usize * 8;
        assert!(
            voiced_ms(&mut v, &sp[..batch]) > 0,
            "整批語音應該判得出人聲"
        );
    }
}

#[cfg(test)]
mod forced_cut_tests {
    use super::*;

    fn tail_batch(sp: &[f32], tail_ms: usize) -> Vec<f32> {
        let n8 = SAMPLE_RATE as usize * 8;
        let len = SAMPLE_RATE as usize * tail_ms / 1000;
        let mut b = vec![0.0f32; n8];
        let from = SAMPLE_RATE as usize * 3;
        b[..len].copy_from_slice(&sp[from..from + len]);
        b
    }

    fn setup() -> Option<(SileroVad, Vec<f32>)> {
        let m = std::env::var("OMN_VAD_MODEL")
            .unwrap_or_else(|_| "/home/enor/whisper-bench/silero_vad.onnx".into());
        let w = "/home/enor/whisper-bench/near.wav";
        if !std::path::Path::new(&m).exists() || !std::path::Path::new(w).exists() {
            return None;
        }
        Some((
            SileroVad::new(gate_config(&m), 40.0).expect("VAD"),
            crate::stt::load_wav_16k_mono(w).expect("讀音訊"),
        ))
    }

    /// 一句話跨過 15 秒強制切批時，詞尾落在下一批的開頭。
    /// 閘門要留得住它，否則那個字永遠消失。
    #[test]
    fn test_a_word_tail_carried_into_the_next_batch_survives_the_gate() {
        let Some((mut v, sp)) = setup() else {
            eprintln!("略過：找不到素材");
            return;
        };
        for tail_ms in [180, 250, 400] {
            let got = voiced_ms(&mut v, &tail_batch(&sp, tail_ms));
            assert!(got > 0, "{tail_ms} ms 的詞尾被閘門丟掉了");
        }
    }

    /// 已知損失：短於約 150 ms 的尾音留不住。
    ///
    /// 要救它得追蹤「上一批是被強制切的」並對續批放寬判準，換來的複雜度
    /// 與幻覺回歸的風險，超過一個字尾音的價值。這條測試釘住現況，日後
    /// 若真的要救，它會提醒改動的邊界在哪。
    #[test]
    fn test_a_very_short_tail_is_a_known_loss() {
        let Some((mut v, sp)) = setup() else { return };
        assert_eq!(voiced_ms(&mut v, &tail_batch(&sp, 120)), 0);
    }
}

#[cfg(test)]
mod partial_throttle_tests {
    use super::*;

    /// 節流的時間來源必須單調遞增。
    ///
    /// 曾經拿 buffer 長度當時間軸，而它被滑動視窗剪在 PARTIAL_WINDOW_MS，
    /// 連續講滿一次視窗之後就永遠停在上限，該軌的即時稿整場不再更新。
    /// 這裡模擬同一條判斷式，證明擷取時間軸不會卡住。
    #[test]
    fn test_the_throttle_keeps_firing_after_the_window_fills_up() {
        let mut last_run = 0u64;
        let mut fired = 0;
        // 一小時的 100 ms chunk，遠超過 15 秒的視窗上限
        for i in 0..36_000u64 {
            let now = i * 100;
            if now.saturating_sub(last_run) >= PARTIAL_EVERY_MS {
                last_run = now;
                fired += 1;
            }
        }
        // 每 800 ms 一次，一小時該有四千多次
        assert!(fired > 4_000, "節流只觸發了 {fired} 次，即時稿會停住");
    }

    /// 對照：用會封頂的長度當時間軸就會卡死
    #[test]
    fn test_a_capped_clock_would_stall_the_throttle() {
        let mut last_run = 0u64;
        let mut fired = 0;
        for i in 0..36_000u64 {
            let now = (i * 100).min(PARTIAL_WINDOW_MS); // buffer 長度的行為
            if now.saturating_sub(last_run) >= PARTIAL_EVERY_MS {
                last_run = now;
                fired += 1;
            }
        }
        assert!(
            fired < 30,
            "封頂的時間軸竟然沒有卡住，這個測試的前提要重新檢查"
        );
    }
}

#[cfg(test)]
mod level_tests {
    use super::*;

    #[test]
    fn test_a_loud_chunk_shows_up_immediately() {
        let l = Levels::default();
        l.observe(Track::Mic, 0.3);
        assert!(l.read().0 >= 0.99, "滿格音量沒有立刻反映");
    }

    #[test]
    fn test_the_bar_decays_instead_of_dropping_to_zero_between_words() {
        let l = Levels::default();
        l.observe(Track::Mic, 0.3);
        l.observe(Track::Mic, 0.0);
        let after = l.read().0;
        assert!(
            after > 0.0 && after < 1.0,
            "字與字之間掉到 {after}，音量條會閃爍"
        );
    }

    #[test]
    fn test_silence_eventually_reaches_the_bottom() {
        let l = Levels::default();
        l.observe(Track::Mic, 0.3);
        for _ in 0..40 {
            l.observe(Track::Mic, 0.0);
        }
        assert!(l.read().0 < 0.01, "靜音久了音量條還停在半空");
    }

    #[test]
    fn test_the_two_tracks_do_not_bleed_into_each_other() {
        let l = Levels::default();
        l.observe(Track::System, 0.3);
        assert_eq!(l.read().0, 0.0, "遠端的聲音被算到麥克風上了");
    }
}

#[cfg(test)]
mod gate_decision_tests {
    use super::*;

    /// 實測幻覺的能量上限與被誤殺短回應的能量下限，門檻落在兩者之間
    const PHANTOM_RMS: f32 = 0.0050;
    const SHORT_REPLY_RMS: f32 = 0.0083;

    /// 真機漏過的兩批噪音幻覺，內容都是「好」，黑名單與結構判準都治不了
    #[test]
    fn test_the_two_phantom_batches_measured_in_the_field_are_still_blocked() {
        // 4% 那批：時長與能量都不到
        assert_eq!(gate(320, 8_000, PHANTOM_RMS, SPEECH_FLOOR), Gate::TooQuiet);
        // 14% 那批：人聲總長 1120 ms 比真實短回應還長，只有能量攔得住它。
        // 這就是判準不能只看時長的理由。
        assert_eq!(
            gate(1_120, 8_000, PHANTOM_RMS, SPEECH_FLOOR),
            Gate::TooQuiet
        );
    }

    /// 實測真實會議批次的分布：最低 31%、五百分位 74%、中位 88%
    #[test]
    fn test_every_measured_real_batch_clears_the_bar() {
        for pct in [31.0, 68.0, 74.0, 79.0, 88.0, 96.0, 99.0] {
            let voiced = (8_000.0 * pct / 100.0) as u64;
            assert_eq!(
                gate(voiced, 8_000, 0.0628, SPEECH_FLOOR),
                Gate::Send,
                "真實會議的 {pct}% 批次會被擋掉"
            );
        }
    }

    /// meeting 57 被門檻吃掉的麥克風批次，換判準之後必須放行
    #[test]
    fn test_the_short_replies_the_ratio_bar_ate_now_pass() {
        // 實測值：人聲 3% 到 18%、RMS 0.0083 到 0.0142，27 批被擋掉 21 批
        for (pct, rms) in [
            (12.0, 0.0090_f32),
            (13.0, 0.0087),
            (14.0, 0.0094),
            (15.0, 0.0142),
            (16.0, 0.0101),
            (17.0, 0.0096),
            (18.0, 0.0102),
        ] {
            let voiced = (8_000.0 * pct / 100.0) as u64;
            assert_eq!(
                gate(voiced, 8_000, rms, SPEECH_FLOOR),
                Gate::Send,
                "{pct}%（{voiced} ms、RMS {rms:.4}）的短回應又被擋掉了"
            );
        }
    }

    /// 能量夠但人聲只有零星碎片，仍然不送
    #[test]
    fn test_a_few_scattered_milliseconds_never_reach_the_model() {
        assert_eq!(
            gate(320, 8_000, SHORT_REPLY_RMS, SPEECH_FLOOR),
            Gate::TooSparse
        );
    }

    /// 兩條門檻各自要與實測資料保持距離
    #[test]
    fn test_both_bars_sit_in_the_gap_not_against_either_side() {
        // 能量是分開幻覺與短回應的那一條，必須落在兩者中間
        const { assert!(SPEECH_FLOOR > PHANTOM_RMS) };
        const { assert!(SPEECH_FLOOR < SHORT_REPLY_RMS) };
        // 時長只負責擋零星碎片：低於實測最短的真實短回應（3% = 240 ms 之上），
        // 又要高於實測最零碎的幻覺（320 ms）
        const { assert!(MIN_VOICED_MS > 320) };
        const { assert!(MIN_VOICED_MS < 960) };
    }
}

#[cfg(test)]
mod speech_floor_tests {
    use super::*;

    /// 三個實測錨點必須排在正確的位置，門檻落在中間的空隙
    #[test]
    fn test_the_speech_floor_sits_between_phantoms_and_the_quietest_real_speech() {
        // 真機漏網的幻覺，RMS 0.0026 到 0.0050
        const HIGHEST_PHANTOM: f32 = 0.0050;
        // 把真實語音縮到還聽得出內容的下限
        const QUIETEST_REAL: f32 = 0.0107;
        // 真實會議批次的最低值
        const LOWEST_MEETING_BATCH: f32 = 0.0628;

        const { assert!(SPEECH_FLOOR > HIGHEST_PHANTOM * 1.5) };
        const { assert!(SPEECH_FLOOR < QUIETEST_REAL / 1.3) };
        const { assert!(SPEECH_FLOOR < LOWEST_MEETING_BATCH / 5.0) };
    }

    /// 吵雜場地要自動提高標準，不能停在絕對下限
    #[test]
    fn test_a_noisy_room_raises_the_speech_floor_above_the_constant() {
        let mut f = NoiseFloor::default();
        for _ in 0..20 {
            f.observe(0.012);
        }
        let floor = SPEECH_FLOOR.max(f.threshold());
        assert!(floor > SPEECH_FLOOR, "吵雜環境沒有提高發言下限");
    }

    /// 安靜環境用絕對下限，不會被幾批靜音拉到擋不住幻覺
    #[test]
    fn test_a_quiet_room_still_blocks_the_measured_phantoms() {
        let mut f = NoiseFloor::default();
        for _ in 0..20 {
            f.observe(0.0008);
        }
        let floor = SPEECH_FLOOR.max(f.threshold());
        assert!(floor > 0.0050, "安靜環境下擋不住實測的幻覺能量");
        assert!(floor < 0.0107, "門檻高到會吃掉小聲說的話");
    }
}
