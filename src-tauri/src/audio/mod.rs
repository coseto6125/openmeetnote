//! 音訊擷取（BLUEPRINT.md §5.2）。
//!
//! 兩軌分開擷取而不是混音：`track` 是「本機 vs 遠端」的先驗，混掉之後就再也
//! 分不出來，而那是語者策略唯一不需要模型就能拿到的資訊。
//!
//! 這一層只負責「開來源、統一格式、送出 PCM」。斷句、轉錄與時間基準都在外面：
//! 音訊執行緒不做網路、不碰 SQLite、不呼叫 UI（§12）。

use std::sync::mpsc::Receiver;

use crate::model::Track;

pub mod writer;

#[cfg(target_os = "windows")]
pub mod wasapi;

#[cfg(target_os = "macos")]
pub mod macos;

/// 一批 PCM 樣本。單聲道 32-bit float，取樣率固定 [`SAMPLE_RATE`]。
///
/// `captured_start_ms` 是這批樣本在該軌擷取音訊中的位置，不是會議時間軸：
/// 暫停期間沒有音訊，兩者會分岔，混用會讓引用定位到錯的地方。
#[derive(Debug, Clone)]
pub struct Chunk {
    pub track: Track,
    pub captured_start_ms: u64,
    /// 共享而不是各自持有：每個 chunk 會分給三個消費者（即時稿、定稿、
    /// 寫檔），`Vec` 的話每多一個消費者就多一次 6.4 KB 的配置與複製，
    /// 而那全發生在必須跟上兩個音訊裝置的分流執行緒上。三者都只讀。
    pub samples: std::sync::Arc<[f32]>,
}

/// 轉錄引擎要的取樣率。在擷取層就轉好，避免事後再重取樣一次。
pub const SAMPLE_RATE: u32 = 16_000;

/// 擷取佇列最多積幾批（每批 100 ms）。
///
/// 有界而不是無界：下游轉錄慢於即時速度時，無界佇列會一路吃記憶體直到會議
/// 結束，長會議就是一次 OOM。積到十秒還沒被讀走代表分流執行緒已經出事，
/// 那時繼續囤積沒有任何人會受益。
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub const CAPTURE_BACKLOG_CHUNKS: usize = 100;

/// 等每一軌回報裝置開啟結果的時限。
///
/// 首次錄音會跳出系統權限對話框，而 macOS 的麥克風授權會擋住 API 呼叫直到
/// 使用者回答。給得寬鬆是為了那個對話框，但不是無限：等不到答案時要說
/// 「等待授權逾時」，而不是讓會議停在一個沒有人知道在等什麼的狀態。
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
// 每個變體都只有一部分平台會建構：`Unsupported` 只在 Windows 與 macOS
// 以外，`NoDevice` 只在有裝置列舉的平台。契約要對所有平台一致，所以三個
// 變體一直都在，而不是各自 cfg 出去 —— 那會讓錯誤處理的 match 隨平台改變。
#[allow(dead_code)]
pub enum AudioError {
    #[error("找不到音訊裝置：{0}")]
    NoDevice(String),
    #[error("擷取失敗：{0}")]
    Capture(String),
    #[error("{0}")]
    Unsupported(String),
}

/// 音訊來源的接縫。Windows、macOS 與測試 Fixture 實作同一個 trait。
pub trait AudioCapture: Send {
    /// 開始擷取。回傳的 Receiver 會持續收到兩軌的 PCM，直到 [`stop`] 被呼叫。
    ///
    /// **回傳 `Ok` 代表兩軌的裝置都已經開起來了。** 只把執行緒 spawn 出去就
    /// 回報成功是不夠的：權限被拒、沒有預設裝置、格式不受支援全都發生在
    /// 執行緒裡面，於是會議進入 Recording、健康狀態顯示正常，而一軌或兩軌
    /// 一個樣本都沒有 —— 錯誤要等到停止 join 之後才寫進 log。
    ///
    /// 任何一軌開不起來就是錯誤，不是「只錄得到的那一軌」。系統音訊權限
    /// 被拒卻悄悄只錄麥克風，等於把使用者以為錄到的內容換成別的東西。
    ///
    /// [`stop`]: AudioCapture::stop
    fn start(&mut self) -> Result<Receiver<Chunk>, AudioError>;

    /// 停止擷取並等擷取執行緒收尾。
    fn stop(&mut self);
}

/// 一軌回報自己的裝置開起來了沒。
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub type TrackReady = std::result::Result<(), String>;

/// 等 `expected` 軌各自回報開啟結果。
///
/// 任一軌失敗就整體失敗：訊息帶上是哪一軌與原因，使用者才知道要去開哪個
/// 權限。逾時視為失敗，理由同上 —— 沒有回答也是一種回答。
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub fn await_tracks(
    ready: &Receiver<TrackReady>,
    expected: usize,
) -> std::result::Result<(), AudioError> {
    let mut failures = Vec::new();
    for _ in 0..expected {
        match ready.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => failures.push(e),
            Err(_) => {
                failures.push("等待音訊裝置回應逾時（可能是權限對話框沒有被回答）".to_owned())
            }
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    Err(AudioError::Capture(failures.join("；")))
}

/// 建立這個平台的擷取器。
///
/// 平台不支援時明確回報而不是給一個永遠不產生音訊的空實作：後者會讓
/// 「錄了一小時發現沒有聲音」變成正常流程的一部分。
///
/// 錯誤訊息帶上這個平台缺什麼與可以怎麼辦。「不支援」四個字對使用者
/// 沒有任何幫助 —— 他需要知道是等更新、還是自己有什麼可以做。
pub fn platform_capture() -> Result<Box<dyn AudioCapture>, AudioError> {
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(wasapi::WasapiCapture::default()))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacCapture::default()))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Err(AudioError::Unsupported(
            "這個平台沒有音訊擷取實作，目前只有 Windows 與 macOS。".into(),
        ))
    }
}
