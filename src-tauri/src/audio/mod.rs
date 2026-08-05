//! 音訊擷取（BLUEPRINT.md §5.2）。
//!
//! 兩軌分開擷取而不是混音：`track` 是「本機 vs 遠端」的先驗，混掉之後就再也
//! 分不出來，而那是語者策略唯一不需要模型就能拿到的資訊。
//!
//! 這一層只負責「開來源、統一格式、送出 PCM」。斷句、轉錄與時間基準都在外面：
//! 音訊執行緒不做網路、不碰 SQLite、不呼叫 UI（§12）。

use std::sync::mpsc::Receiver;

use crate::model::Track;

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
    pub samples: Vec<f32>,
}

/// 轉錄引擎要的取樣率。在擷取層就轉好，避免事後再重取樣一次。
pub const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, thiserror::Error)]
// NoDevice 與 Capture 只有平台實作會建構。Linux build 沒有 WASAPI，
// 因此它們在這個 target 下確實用不到，但契約要對所有平台一致。
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
    /// [`stop`]: AudioCapture::stop
    fn start(&mut self) -> Result<Receiver<Chunk>, AudioError>;

    /// 停止擷取並等擷取執行緒收尾。
    fn stop(&mut self);
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
