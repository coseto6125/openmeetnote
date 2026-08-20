//! 跨模組共用的值型別。
//!
//! 只放「Session、Store、Document 都要講同一種話」的東西。
//! 模組內部才用得到的狀態留在該模組，放進來只會讓耦合看起來像共用。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MeetingState {
    Idle,
    Recording,
    Paused,
    Stopping,
    Finalizing,
    Completed,
    /// 沒有正常走完的會議：崩潰、被強制關閉，或啟動時發現狀態還停在錄音。
    ///
    /// 與 `Completed` 分開，因為它的逐字稿可能斷在半句話。混進正常結束的
    /// 會議裡，使用者就沒有機會知道哪一場的內容是不完整的（§13）。
    Failed,
}

impl MeetingState {
    /// 會議正在進行，允許筆記、語者確認與修訂。
    pub fn accepts_document_work(self) -> bool {
        matches!(self, MeetingState::Recording | MeetingState::Paused)
    }

    /// 可以建立摘要快照。
    ///
    /// 比 [`accepts_document_work`] 多一個 `Completed`：使用者最常見的流程
    /// 就是開完會才要摘要，那時內容才完整。藍圖 §6 規定快照是
    /// `Recording → Recording`，約束的是「不得為了生成而停止錄音」，
    /// 不是「只能在錄音時生成」。
    ///
    /// [`accepts_document_work`]: MeetingState::accepts_document_work
    pub fn accepts_snapshot(self) -> bool {
        self.accepts_document_work() || matches!(self, MeetingState::Completed)
    }

    /// 可以為語者命名。
    ///
    /// 比 [`accepts_document_work`] 多了結算與結束：誰是誰通常要等散會、
    /// 回頭讀逐字稿才確定得下來，錄音當下正忙著開會的人不會停下來命名。
    /// 語者名稱本來就設計成可由使用者更正（§8），把更正的窗口關在錄音期間，
    /// 等於把功能關在使用者需要它之前。
    ///
    /// `Failed` 也放行：斷在半句話的會議一樣看得出誰在講。
    ///
    /// [`accepts_document_work`]: MeetingState::accepts_document_work
    pub fn accepts_speaker_naming(self) -> bool {
        !matches!(self, MeetingState::Idle | MeetingState::Stopping)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MeetingState::Idle => "idle",
            MeetingState::Recording => "recording",
            MeetingState::Paused => "paused",
            MeetingState::Stopping => "stopping",
            MeetingState::Finalizing => "finalizing",
            MeetingState::Completed => "completed",
            MeetingState::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(MeetingState::Idle),
            "recording" => Some(MeetingState::Recording),
            "paused" => Some(MeetingState::Paused),
            "stopping" => Some(MeetingState::Stopping),
            "finalizing" => Some(MeetingState::Finalizing),
            "completed" => Some(MeetingState::Completed),
            "failed" => Some(MeetingState::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Health {
    Ok,
    Degraded,
    Lost,
}

/// 片段版本的來源。使用者修訂的優先權高於 Provider，
/// 因此 Provider 的後到結果不得覆蓋它（§5.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Origin {
    Provider,
    User,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Provider => "provider",
            Origin::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "provider" => Some(Origin::Provider),
            "user" => Some(Origin::User),
            _ => None,
        }
    }
}

/// 音訊軌道。§8.1 的軌道先驗只到「本機 vs 遠端」這個粒度，
/// 不能再往「遠端只有一人」推。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Track {
    Mic,
    System,
}

/// 聲紋分不出是誰時，遠端發言掛在這個識別碼下。
///
/// 不能沿用 `s1`：那是聲紋登記的第一位（見 `stt::speakers`），兩者同名的話
/// 「分不出來」會被靜默併進一個真實的人，逐字稿於是把話安到他頭上，而讀的
/// 人看不出來。
///
/// 它不是一個人，是「遠端，但不確定是誰」。三個後果，畫面、匯出、摘要必須
/// 一致（§8.1）：顯示成「遠端」而不是「語者 N」、不佔用「語者 N」的編號、
/// 不接受命名。命名它等於把好幾位不同的未知發言者併成一位真人。
pub const UNKNOWN_REMOTE: &str = "remote";

impl Track {
    pub fn as_str(self) -> &'static str {
        match self {
            Track::Mic => "mic",
            Track::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mic" => Some(Track::Mic),
            "system" => Some(Track::System),
            _ => None,
        }
    }
}

/// 事件發生當下的兩條時間軸（§11）。
///
/// 兩者一起傳而不是各傳各的：只帶其中一條的呼叫端遲早會用錯基準，
/// 而暫停期間兩者的差距正是這個型別存在的理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timeline {
    /// 含暫停與靜音的會議時間軸，是筆記與所有引用的定位基準。
    pub meeting_time_ms: u64,
    /// 只計有錄音資料的位置，用於音訊檔內定位。
    pub captured_audio_ms: u64,
}

impl Timeline {
    pub fn new(meeting_time_ms: u64, captured_audio_ms: u64) -> Self {
        Self {
            meeting_time_ms,
            captured_audio_ms,
        }
    }
}

/// §3.4 的內容分類，與區塊的結構種類正交。
///
/// 沒有 `Default` 是刻意的。缺這個分類是 schema 驗證失敗，
/// 而不是可以猜的東西；真要退也只能退 `Fact`（fail-closed，逼出引用義務）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClaimKind {
    Fact,
    Inference,
    Suggestion,
    Gap,
}

impl ClaimKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimKind::Fact => "fact",
            ClaimKind::Inference => "inference",
            ClaimKind::Suggestion => "suggestion",
            ClaimKind::Gap => "gap",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fact" => Some(ClaimKind::Fact),
            "inference" => Some(ClaimKind::Inference),
            "suggestion" => Some(ClaimKind::Suggestion),
            "gap" => Some(ClaimKind::Gap),
            _ => None,
        }
    }

    /// 是否必須附帶可驗證的引用。
    ///
    /// 只有 `Fact`（§9.6）。它宣稱會議實際發生過某件事，沒有引用就不能宣稱。
    ///
    /// `Inference` 刻意不在此列：它的義務是「被清楚標示為推論」（§3.4、§10），
    /// 不是提供逐字出處。要求推論逐字引用，會逼模型把推論改寫成它引得到的
    /// 片段，也就是把推論偽裝成事實，那正好是這套機制要防的事。
    /// `Suggestion` 與 `Gap` 依定義不是會議事實。
    pub fn requires_citation(self) -> bool {
        self == ClaimKind::Fact
    }
}

/* ── Provider 設定（§5.6） ──────────────────────────────────────── */

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Stt,
    Llm,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Stt => "stt",
            ProviderKind::Llm => "llm",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stt" => Some(ProviderKind::Stt),
            "llm" => Some(ProviderKind::Llm),
            _ => None,
        }
    }

    /// 環境變數前綴，例如 `OPENMEETNOTE_STT_PROVIDER`。
    pub fn env_prefix(self) -> &'static str {
        match self {
            ProviderKind::Stt => "OPENMEETNOTE_STT",
            ProviderKind::Llm => "OPENMEETNOTE_LLM",
        }
    }
}

/// GUI 存下來的非敏感設定，對應 `provider_settings` 資料表。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredProvider {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    #[serde(default)]
    pub options: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_enum_round_trips_through_its_database_string() {
        for s in [
            MeetingState::Idle,
            MeetingState::Recording,
            MeetingState::Paused,
            MeetingState::Stopping,
            MeetingState::Finalizing,
            MeetingState::Completed,
        ] {
            assert_eq!(MeetingState::parse(s.as_str()), Some(s));
        }
        for o in [Origin::Provider, Origin::User] {
            assert_eq!(Origin::parse(o.as_str()), Some(o));
        }
        for t in [Track::Mic, Track::System] {
            assert_eq!(Track::parse(t.as_str()), Some(t));
        }
        for c in [
            ClaimKind::Fact,
            ClaimKind::Inference,
            ClaimKind::Suggestion,
            ClaimKind::Gap,
        ] {
            assert_eq!(ClaimKind::parse(c.as_str()), Some(c));
        }
    }

    #[test]
    fn only_fact_carries_a_citation_duty() {
        assert!(ClaimKind::Fact.requires_citation());
        // 推論的義務是被標示，不是逐字引用。要求它引用只會把推論
        // 改寫成引得到的片段，也就是偽裝成事實。
        assert!(!ClaimKind::Inference.requires_citation());
        assert!(!ClaimKind::Suggestion.requires_citation());
        assert!(!ClaimKind::Gap.requires_citation());
    }
}
