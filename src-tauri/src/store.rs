//! 事件日誌與投影。
//!
//! BLUEPRINT.md §5.4.1：`meeting_events` 是唯一真實來源，其餘資料表都是它的
//! 投影，必須能在任何時候重建。這個模組是那條規則唯一的執行點。
//!
//! 三個結構決定：
//!
//! 1. **`append` 與 `rebuild_projections` 共用同一個 `project()`。** 兩條路徑
//!    各寫一份投影邏輯，就等於保證它們有一天會不一致，而且不一致只在災難
//!    復原時才會被發現，也就是最不想除錯的時候。
//! 2. **事件與投影在同一個 SQLite 交易內提交。** 不存在「事件已寫、投影未更新」
//!    的中間狀態，因此不需要開機修補程序。
//! 3. **seq 由 `meetings.high_seq` 配發，不用 `MAX(seq)`。** 兩者平時相同，但
//!    投影重建或事件裁切之後 `MAX` 會倒退，而 seq 一旦倒退，已匯出文件的
//!    `through_event_seq` 就會指到別的內容。
//!
//! 投影只認得決定性事件。partial 逐字稿不進日誌（§5.4.1）：一場兩小時會議的
//! partial 是 final 的數十倍，讓它進來會使 `through_event_seq` 失去意義。

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::clock;
use crate::model::{ClaimKind, MeetingState, Origin, Timeline, Track};

pub type MeetingId = i64;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("會議 {0} 不存在")]
    NoSuchMeeting(MeetingId),
    /// 日誌內容無法投影。這不是可以略過的髒資料，略過就等於默默改寫歷史。
    #[error("事件 seq {seq} 無法投影：{reason}")]
    Corrupt { seq: u64, reason: String },
}

pub type Result<T> = std::result::Result<T, StoreError>;

// ── 事件 ────────────────────────────────────────────────────────────────

/// 逐字稿片段的一個不可變版本。
///
/// 欄位比想像中多，因為 §11 要求 Provider 的三個識別碼與去重所需的音訊區間
/// 都必須落在欄位上。識別碼只供診斷，判重用的是正規化後的音訊區間。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentRevision {
    pub segment_id: u64,
    pub revision: u32,
    pub text: String,
    pub speaker_id: Option<String>,
    pub track: Track,
    pub meeting_start_ms: u64,
    pub meeting_end_ms: u64,
    pub captured_start_ms: u64,
    pub captured_end_ms: u64,
    #[serde(default)]
    pub echo_likelihood: Option<f64>,
    #[serde(default)]
    pub overlap_group_id: Option<String>,
    #[serde(default)]
    pub provider_stream_id: Option<String>,
    #[serde(default)]
    pub provider_result_id: Option<String>,
    #[serde(default)]
    pub rollover_generation: u32,
    pub origin: Origin,
    /// 詞或語句層級的語者指派。空 = 用片段層級的 `speaker_id`（§18）。
    #[serde(default)]
    pub speaker_spans: Vec<SpeakerSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerSpan {
    pub speaker_id: String,
    pub meeting_start_ms: u64,
    pub meeting_end_ms: u64,
    pub char_start: u32,
    pub char_end: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSegment {
    pub id: i64,
    pub track: Track,
    /// 裝置重新開啟就換一個 epoch。跨 epoch 的樣本計數不可直接相加。
    pub source_epoch: u32,
    pub path: String,
    pub captured_start_ms: u64,
    pub captured_end_ms: u64,
    pub meeting_start_ms: u64,
    pub meeting_end_ms: u64,
    /// 壓縮靜音自成的段：captured 長度為零、meeting 長度不為零（§5.2.3）。
    #[serde(default)]
    pub is_silence_fill: bool,
    pub checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentChunk {
    pub page_no: Option<u32>,
    pub start_offset: u32,
    pub end_offset: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentBlock {
    pub position: u32,
    /// 結構種類：heading / paragraph / list / table / mermaid / quote / code
    pub kind: String,
    /// §3.4 的內容分類，與 `kind` 正交。沒有預設值。
    pub claim_kind: ClaimKind,
    pub content: String,
    #[serde(default)]
    pub source_refs: Vec<SourceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRef {
    pub source_kind: String,
    pub source_id: String,
    /// 固定版本。單有版本不足以重現引用，所以引文與雜湊一起存（§11）。
    pub source_revision: u32,
    pub locator: String,
    pub quoted_text: String,
    /// 引文的雜湊。Planner 不必提供：要模型算 SHA256 只會拿到瞎編的值，
    /// 系統收到區塊後自己算才有意義（§9.6）。
    #[serde(default)]
    pub quoted_text_sha256: String,
    /// 驗證結果。同樣由系統填，模型提供的值一律被覆蓋 —— 讓被驗證者
    /// 自己宣告驗證通過，那個驗證就不存在。
    #[serde(default)]
    pub validation_status: String,
}

/// 決定性事件。這份清單就是 §5.4.1 的清單，不多不少。
///
/// 新增種類時同步更新 `project()`，否則投影會安靜地漏掉它。
/// `kind()` 與 `project()` 都是窮舉 match，少一支編譯不會過。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
pub enum DomainEvent {
    MeetingStateChanged {
        state: MeetingState,
    },
    /// 會議標題。建立時發一次，之後每次改名再發一次。
    ///
    /// 標題原本是就地 UPDATE，於是它是唯一一個重建不回來的欄位 ——
    /// 「所有投影都能從日誌重建」有一個沒有人注意到的例外。
    MeetingRenamed {
        title: String,
    },
    TranscriptSegmentFinalized {
        segment: SegmentRevision,
    },
    /// Provider 對已 final 片段的後續修訂
    TranscriptSegmentRevised {
        segment: SegmentRevision,
    },
    /// 使用者修訂。優先權高於 Provider，因此另立事件而非共用 Revised。
    TranscriptSegmentEdited {
        segment: SegmentRevision,
    },
    SpeakerReassigned {
        segment_id: u64,
        revision: u32,
        speaker_id: Option<String>,
    },
    NoteAdded {
        note_id: u64,
        text: String,
    },
    NoteEdited {
        note_id: u64,
        text: String,
    },
    NoteRemoved {
        note_id: u64,
    },
    SpeakerProposed {
        speaker_id: String,
        ordinal: u32,
        proposed_name: Option<String>,
        #[serde(default)]
        provider_labels: Vec<String>,
    },
    SpeakerConfirmed {
        speaker_id: String,
        name: String,
    },
    SpeakerRenamed {
        speaker_id: String,
        name: String,
    },
    SpeakerMerged {
        from_speaker_id: String,
        into_speaker_id: String,
    },
    SpeakerSplit {
        from_speaker_id: String,
        new_speaker_id: String,
        ordinal: u32,
    },
    AttachmentAdded {
        attachment_id: i64,
        path: String,
        mime: String,
        sha256: String,
    },
    AttachmentExtracted {
        attachment_id: i64,
        extraction_revision: u32,
        chunks: Vec<AttachmentChunk>,
    },
    AttachmentRemoved {
        attachment_id: i64,
    },
    AudioSegmentFinalized {
        segment: AudioSegment,
    },
    /// 建立摘要快照。文件本身也在這裡落地，因為 `documents` 同樣是投影，
    /// 沒有事件承載它就無法從日誌重建。
    SnapshotCreated {
        document_id: i64,
        run_id: i64,
        parent_run_id: Option<i64>,
        version_no: u32,
        purpose: String,
        title: String,
        through_event_seq: u64,
        prompt: String,
    },
    /// 帶著整份成果。區塊與引用都是投影，事件不承載它們就重建不出來。
    GenerationCompleted {
        run_id: i64,
        blocks: Vec<DocumentBlock>,
        usage: serde_json::Value,
    },
    GenerationFailed {
        run_id: i64,
        reason: String,
    },
}

impl DomainEvent {
    /// 供 `meeting_events.kind` 索引用的字串。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MeetingStateChanged { .. } => "MeetingStateChanged",
            Self::MeetingRenamed { .. } => "MeetingRenamed",
            Self::TranscriptSegmentFinalized { .. } => "TranscriptSegmentFinalized",
            Self::TranscriptSegmentRevised { .. } => "TranscriptSegmentRevised",
            Self::TranscriptSegmentEdited { .. } => "TranscriptSegmentEdited",
            Self::SpeakerReassigned { .. } => "SpeakerReassigned",
            Self::NoteAdded { .. } => "NoteAdded",
            Self::NoteEdited { .. } => "NoteEdited",
            Self::NoteRemoved { .. } => "NoteRemoved",
            Self::SpeakerProposed { .. } => "SpeakerProposed",
            Self::SpeakerConfirmed { .. } => "SpeakerConfirmed",
            Self::SpeakerRenamed { .. } => "SpeakerRenamed",
            Self::SpeakerMerged { .. } => "SpeakerMerged",
            Self::SpeakerSplit { .. } => "SpeakerSplit",
            Self::AttachmentAdded { .. } => "AttachmentAdded",
            Self::AttachmentExtracted { .. } => "AttachmentExtracted",
            Self::AttachmentRemoved { .. } => "AttachmentRemoved",
            Self::AudioSegmentFinalized { .. } => "AudioSegmentFinalized",
            Self::SnapshotCreated { .. } => "SnapshotCreated",
            Self::GenerationCompleted { .. } => "GenerationCompleted",
            Self::GenerationFailed { .. } => "GenerationFailed",
        }
    }

    /// `entity_id` / `entity_revision` 只供人工追查與索引，不參與投影。
    fn entity(&self) -> (Option<String>, Option<u32>) {
        match self {
            Self::TranscriptSegmentFinalized { segment }
            | Self::TranscriptSegmentRevised { segment }
            | Self::TranscriptSegmentEdited { segment } => {
                (Some(segment.segment_id.to_string()), Some(segment.revision))
            }
            Self::SpeakerReassigned {
                segment_id,
                revision,
                ..
            } => (Some(segment_id.to_string()), Some(*revision)),
            Self::NoteAdded { note_id, .. }
            | Self::NoteEdited { note_id, .. }
            | Self::NoteRemoved { note_id } => (Some(note_id.to_string()), None),
            Self::SpeakerProposed { speaker_id, .. }
            | Self::SpeakerConfirmed { speaker_id, .. }
            | Self::SpeakerRenamed { speaker_id, .. } => (Some(speaker_id.clone()), None),
            Self::SpeakerMerged {
                from_speaker_id, ..
            }
            | Self::SpeakerSplit {
                from_speaker_id, ..
            } => (Some(from_speaker_id.clone()), None),
            Self::AttachmentAdded { attachment_id, .. }
            | Self::AttachmentRemoved { attachment_id } => (Some(attachment_id.to_string()), None),
            Self::AttachmentExtracted {
                attachment_id,
                extraction_revision,
                ..
            } => (Some(attachment_id.to_string()), Some(*extraction_revision)),
            Self::AudioSegmentFinalized { segment } => (Some(segment.id.to_string()), None),
            Self::SnapshotCreated {
                run_id, version_no, ..
            } => (Some(run_id.to_string()), Some(*version_no)),
            Self::GenerationCompleted { run_id, .. } | Self::GenerationFailed { run_id, .. } => {
                (Some(run_id.to_string()), None)
            }
            Self::MeetingStateChanged { .. } | Self::MeetingRenamed { .. } => (None, None),
        }
    }
}

/// 一筆已持久化的事件。
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub seq: u64,
    pub timeline: Timeline,
    /// 事件寫入當下的掛鐘時間。重播必須沿用這個值，
    /// 用當下的時間會讓投影裡的 created_at 隨每次重建而變，
    /// 那就不是重建投影而是改寫歷史。
    pub created_at: String,
    pub event: DomainEvent,
}

// ── 查詢結果 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingSummary {
    pub id: MeetingId,
    pub title: String,
    pub state: MeetingState,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub meeting_time_ms: u64,
    pub captured_audio_ms: u64,
    pub high_seq: u64,
    pub segment_count: u64,
    pub note_count: u64,
    pub document_count: u64,
}

/// 一場命中搜尋的會議，連同它為什麼命中。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingHit {
    pub summary: MeetingSummary,
    /// 命中的上下文，最多 `max_excerpts` 筆
    pub excerpts: Vec<SearchExcerpt>,
    /// 這場會議一共命中幾處。摘錄被截斷時使用者仍要知道還有多少
    pub total_hits: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchExcerpt {
    /// `transcript` 或 `note`
    pub kind: String,
    pub segment_id: Option<u64>,
    pub meeting_time_ms: u64,
    pub text: String,
}

/// 重開會議所需的全部投影內容。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingDetail {
    pub summary: MeetingSummary,
    pub segments: Vec<StoredSegment>,
    pub notes: Vec<StoredNote>,
    pub speakers: Vec<StoredSpeaker>,
    pub runs: Vec<StoredRun>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSegment {
    pub segment_id: u64,
    pub revision: u32,
    pub origin: Origin,
    pub speaker_id: Option<String>,
    pub text: String,
    pub track: Track,
    pub meeting_start_ms: u64,
    pub meeting_end_ms: u64,
    pub user_edited: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredNote {
    pub note_id: u64,
    pub text: String,
    pub meeting_time_ms: u64,
    pub captured_audio_ms: u64,
    /// 建立這筆筆記的事件序號。
    ///
    /// 引用驗證要求 `source_revision` 等於它（筆記沒有 revision 的概念，
    /// event_seq 就是它的版本）。不把這個值送給模型，模型就永遠組不出一筆
    /// 通得過驗證的筆記引用 —— 而 §17 完成定義第 5 點要求筆記可被引用。
    pub event_seq: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSpeaker {
    pub speaker_id: String,
    pub ordinal: u32,
    pub proposed_name: Option<String>,
    pub confirmed_name: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredRun {
    pub run_id: i64,
    pub document_id: i64,
    pub version_no: u32,
    pub through_event_seq: u64,
    pub status: String,
    pub title: String,
    pub purpose: String,
    pub prompt: String,
    pub failure_reason: Option<String>,
    pub created_at: String,
}

// ── Store ───────────────────────────────────────────────────────────────

pub struct Store {
    conn: Connection,
}

/// Store 的共用把手。
///
/// 與 `SessionHandle` 分成兩把鎖：寫 SQLite 不該卡住命令處理，命令持鎖時
/// 也不該等磁碟。代價是兩者之間有個短暫的未落地窗口，因此每個命令結束都
/// flush 一次，未落地的內容最多只有 STT 的一個節流窗。
///
/// 住在 store 而不是 session：設定與歷史都要用它，掛在會議模組底下會逼
/// 那些模組為了一個儲存把手而相依於會議。
pub struct StoreHandle {
    inner: std::sync::Mutex<Store>,
    path: std::path::PathBuf,
    /// 只有 `temp()` 建的把手會設。正式環境的資料庫絕不自動刪除。
    #[cfg(test)]
    ephemeral: bool,
}

#[cfg(test)]
impl Drop for StoreHandle {
    fn drop(&mut self) {
        if !self.ephemeral {
            return;
        }
        // 測試用的檔案自己收乾淨。一輪測試上百個 sqlite3 加 -wal 留在
        // /tmp 裡，下次跑的時候誰也分不出哪些還活著。
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

/// 鎖損毀或連線開不起來。呼叫端一律 fail closed。
#[derive(Debug)]
pub struct StoreUnavailable;

impl std::fmt::Display for StoreUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("資料庫連線狀態已損毀")
    }
}

impl StoreHandle {
    pub fn open(path: std::path::PathBuf) -> crate::db::Result<Self> {
        let conn = crate::db::open(&path)?;
        Ok(Self {
            inner: std::sync::Mutex::new(Store::new(conn)),
            path,
            #[cfg(test)]
            ephemeral: false,
        })
    }

    /// 獨佔連線。讀寫都走這裡，持鎖時間必須短。
    ///
    /// 名字講的是鎖的性質，不是操作的方向：`get_settings` 與 `export_document`
    /// 只讀也走這條，因為 `Store` 只有這一個入口。要並行讀取的是 `reader()`。
    pub fn exclusive(
        &self,
    ) -> std::result::Result<std::sync::MutexGuard<'_, Store>, StoreUnavailable> {
        self.inner.lock().map_err(|_| StoreUnavailable)
    }

    /// 以暫存檔為底的把手，供測試走與正式環境完全相同的路徑。
    ///
    /// 不用記憶體資料庫：`reader()` 需要真的另開一條連線，而記憶體資料庫
    /// 每個連線都是各自獨立的。測試若走不同的路徑，就抓不到那條路徑上的錯。
    #[cfg(test)]
    pub fn temp() -> crate::db::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "openmeetnote-test-{}-{}.sqlite3",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        // -wal 與 -shm 也要清。留著舊的 -wal 對上新建的資料庫，
        // SQLite 會把它當成需要復原的日誌套進去。pid 會被回收，
        // 所以「上次那個檔案不存在」不是可以假設的事。
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let mut handle = Self::open(path)?;
        handle.ephemeral = true;
        Ok(handle)
    }

    /// 有時限的寫入嘗試,只給測試用。
    ///
    /// `write()` 會一直等。測試若用它來證明「某某期間鎖是放開的」,規則被破壞時
    /// 得到的是卡死而不是失敗,CI 上會燒掉整個 job 的時限才收場。
    #[cfg(test)]
    pub fn try_exclusive_for(
        &self,
        limit: std::time::Duration,
    ) -> std::result::Result<std::sync::MutexGuard<'_, Store>, StoreUnavailable> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            match self.inner.try_lock() {
                Ok(g) => return Ok(g),
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(StoreUnavailable),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(StoreUnavailable);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }

    /// 另開一條唯讀連線。
    ///
    /// 摘要生成要在整個模型往返期間讀證據。用主連線的話就得抱著寫入鎖，
    /// 錄音的落地會被卡住整趟 LLM 呼叫 —— 那正是 §5.4.2 要避免的事。
    /// WAL 模式允許一個寫入者與多個讀取者同時存在，所以這裡直接開新連線。
    pub fn reader(&self) -> crate::db::Result<Store> {
        Ok(Store::new(crate::db::open_reader(&self.path)?))
    }
}

impl Store {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub fn create_meeting(&mut self, title: &str) -> Result<MeetingId> {
        let now = clock::now_utc();
        self.conn.execute(
            "INSERT INTO meetings (title, state, created_at) VALUES (?1, 'idle', ?2)",
            params![title, now],
        )?;
        // 起始標題不發事件。這一列的存在本身就不是事件的投影 —— 事件要靠
        // 外鍵指向它，列得先在那裡。建立時給的名字屬於那一步，之後的每一次
        // 改名才是決定性事件。
        Ok(self.conn.last_insert_rowid())
    }

    /// 追加一批事件並更新投影，全部在同一個交易內。
    ///
    /// 整批一交易而不是逐筆：一次錄音節流窗內的事件在語意上是一起發生的，
    /// 拆成多個交易只會讓崩潰時多出「半批」這種需要另外處理的狀態。
    pub fn append(
        &mut self,
        meeting: MeetingId,
        events: &[(DomainEvent, Timeline)],
    ) -> Result<Vec<u64>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self.conn.transaction()?;
        let mut high: u64 = tx
            .query_row(
                "SELECT high_seq FROM meetings WHERE id = ?1",
                params![meeting],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .ok_or(StoreError::NoSuchMeeting(meeting))? as u64;

        let now = clock::now_utc();
        let mut seqs = Vec::with_capacity(events.len());
        for (event, tl) in events {
            high += 1;
            let (entity_id, entity_revision) = event.entity();
            tx.execute(
                "INSERT INTO meeting_events
                    (meeting_id, seq, kind, entity_id, entity_revision, payload,
                     meeting_time_ms, captured_audio_ms, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    meeting,
                    high as i64,
                    event.kind(),
                    entity_id,
                    entity_revision,
                    serde_json::to_string(event)?,
                    tl.meeting_time_ms as i64,
                    tl.captured_audio_ms as i64,
                    now,
                ],
            )?;
            project(&tx, meeting, high, *tl, &now, event)?;
            seqs.push(high);
        }

        // high_seq 與兩條時間軸一起推進，列表頁不必重播事件就能顯示長度
        let last = events[events.len() - 1].1;
        tx.execute(
            "UPDATE meetings SET high_seq = ?2, meeting_time_ms = ?3, captured_audio_ms = ?4
             WHERE id = ?1",
            params![
                meeting,
                high as i64,
                last.meeting_time_ms as i64,
                last.captured_audio_ms as i64
            ],
        )?;
        tx.commit()?;
        Ok(seqs)
    }

    /// 原始事件日誌。目前只有測試與重建路徑會走，但它是 §5.4.1 那條
    /// 「日誌是唯一真實來源」的讀取入口，不隨投影演化而改變。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn events(&self, meeting: MeetingId) -> Result<Vec<StoredEvent>> {
        load_events(&self.conn, meeting)
    }

    /// 清空並從日誌重建這場會議的所有投影。
    ///
    /// 走的是 `append` 用的同一個 `project()`，因此重建結果依定義等同
    /// 逐筆寫入的結果。這個性質有測試守著。
    pub fn rebuild_projections(&mut self, meeting: MeetingId) -> Result<()> {
        let events = load_events(&self.conn, meeting)?;
        let tx = self.conn.transaction()?;
        clear_projections(&tx, meeting)?;
        // meetings 那一列上由事件決定的欄位也要重設，否則壞掉的狀態會直接
        // 活過重建 —— 而重建正是為了修好它才跑的。標題不動：它由建立那一步
        // 給定，之後的改名會在重播時把新的值寫回來。
        tx.execute(
            "UPDATE meetings SET state = 'idle', started_at = NULL, ended_at = NULL WHERE id = ?1",
            params![meeting],
        )?;
        for e in &events {
            project(&tx, meeting, e.seq, e.timeline, &e.created_at, &e.event)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 把上次沒有正常結束的會議收尾（BLUEPRINT.md §13）。
    ///
    /// app 被強制關閉或崩潰時，會議的狀態會停在 `recording` 或 `paused`。
    /// 不處理的話歷史頁會永遠顯示一場「進行中」的會議，而使用者按不到任何
    /// 可以結束它的按鈕 —— 那是「畫面說有、磁碟說沒有」的另一種形狀。
    ///
    /// 標成 `failed` 而不是 `completed`：那場會議確實沒有正常走完，逐字稿
    /// 可能停在半句話。混進正常結束的會議裡，使用者就沒有機會知道哪一場
    /// 的內容是不完整的。已經寫進去的片段、筆記與摘要全部保留。
    ///
    /// 回傳被收尾的會議數，供啟動日誌記錄。
    pub fn close_abandoned_meetings(&mut self) -> Result<usize> {
        let stranded: Vec<MeetingId> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM meetings
                  WHERE state IN ('recording', 'paused', 'stopping', 'finalizing')",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        // 走事件而不是就地 UPDATE：`failed` 是這場會議歷史的一部分，
        // 重建投影時要回得來。日誌裡沒有它的話，重建之後那場會議會變回
        // 「錄音中」，而它其實早就結束了。
        for meeting in &stranded {
            self.append(
                *meeting,
                &[(
                    DomainEvent::MeetingStateChanged {
                        state: MeetingState::Failed,
                    },
                    Timeline::new(0, 0),
                )],
            )?;
        }
        Ok(stranded.len())
    }

    /// 這場會議的標題。
    ///
    /// 匯出檔的 `<title>` 與 `<h1>` 用它，而不是 `documents.title`：後者一律是
    /// 「會議摘要」，於是每一份匯出檔看起來都一樣，瀏覽器分頁上也分不出誰是誰。
    /// 會議標題是使用者改得動的那一個，這裡就該用它。
    pub fn meeting_title(&self, meeting: MeetingId) -> Result<String> {
        self.conn
            .query_row(
                "SELECT title FROM meetings WHERE id = ?1",
                params![meeting],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::NoSuchMeeting(meeting))
    }

    /// 快照游標之後又寫入了幾筆**內容**事件。
    ///
    /// 這是 §9.2 第五步要回答的問題之一：成果之外還有沒有東西，而那是系統
    /// 知道、模型不知道的事實。
    ///
    /// 只數逐字稿與筆記，不數 `high_seq` 的差。快照本身會記一筆
    /// `SnapshotCreated`、生成完成再記一筆 `GenerationCompleted`，兩者的 seq
    /// 都在游標之後，拿事件總數來比的話每一份成果都會被標上一個不存在的缺口。
    pub fn content_events_after(&self, meeting: MeetingId, seq: u64) -> Result<u64> {
        const CONTENT: [&str; 4] = [
            "TranscriptSegmentFinalized",
            "TranscriptSegmentRevised",
            "TranscriptSegmentEdited",
            "NoteAdded",
        ];
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM meeting_events
              WHERE meeting_id = ?1 AND seq > ?2
                AND kind IN (?3, ?4, ?5, ?6)",
            params![meeting, seq as i64, CONTENT[0], CONTENT[1], CONTENT[2], CONTENT[3]],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    pub fn list_meetings(&self) -> Result<Vec<MeetingSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.title, m.state, m.started_at, m.ended_at,
                    m.meeting_time_ms, m.captured_audio_ms, m.high_seq,
                    (SELECT COUNT(*) FROM transcript_segments s WHERE s.meeting_id = m.id),
                    (SELECT COUNT(*) FROM notes n WHERE n.meeting_id = m.id AND n.removed = 0),
                    (SELECT COUNT(*) FROM documents d WHERE d.meeting_id = m.id)
             FROM meetings m
             ORDER BY COALESCE(m.started_at, m.created_at) DESC, m.id DESC",
        )?;
        let rows = stmt.query_map([], row_to_summary)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 跨會議搜尋標題、逐字稿與人工筆記（§2.1）。
    ///
    /// 走 `LIKE` 掃描而不是 FTS5：全文索引要靠觸發器或事件重播維護，而
    /// `meeting_events` 是唯一真實來源，多一份得同步的衍生狀態就多一種它
    /// 與事實不一致的方式。
    ///
    /// 規模也不支持那個成本。實測（debug build，13 萬列，相當於 50 場兩小時
    /// 會議）：一般查詢 40 ms，每一列都命中的最壞情況 174 ms。前端有 200 ms
    /// 去抖，使用者感覺得到的是輸入停下之後的那一次。`probe_how_long_a_like_scan_takes_at_realistic_scale`
    /// 守著這個判斷，超過 500 ms 就紅燈，那時才該換索引。
    ///
    /// 空字串回空結果而不是全部：呼叫端在沒有查詢字串時該顯示完整清單，
    /// 那是另一條路徑，不該由搜尋函式假裝自己是它。
    pub fn search_meetings(&self, query: &str, max_excerpts: usize) -> Result<Vec<MeetingHit>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        // LIKE 的萬用字元要跳脫，否則使用者搜「100%」會match到任何東西
        let escaped = q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");

        let mut hits: std::collections::HashMap<MeetingId, (Vec<SearchExcerpt>, u32)> =
            std::collections::HashMap::new();
        let mut add = |meeting: MeetingId, excerpt: SearchExcerpt| {
            let e = hits.entry(meeting).or_default();
            e.1 += 1;
            if e.0.len() < max_excerpts {
                e.0.push(excerpt);
            }
        };

        let mut stmt = self.conn.prepare(
            "SELECT r.meeting_id, r.segment_id, r.meeting_start_ms, r.text
               FROM transcript_segments s
               JOIN transcript_segment_revisions r
                 ON r.meeting_id = s.meeting_id
                AND r.segment_id = s.id
                AND r.revision   = s.current_revision
              WHERE r.text LIKE ?1 ESCAPE '\\'
              ORDER BY r.meeting_id, s.meeting_start_ms",
        )?;
        let rows = stmt.query_map(params![pattern], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (meeting, segment_id, at, text) = row?;
            add(
                meeting,
                SearchExcerpt {
                    kind: "transcript".into(),
                    segment_id: Some(segment_id as u64),
                    meeting_time_ms: at,
                    text,
                },
            );
        }

        let mut stmt = self.conn.prepare(
            "SELECT meeting_id, meeting_time_ms, text
               FROM notes
              WHERE removed = 0 AND text LIKE ?1 ESCAPE '\\'
              ORDER BY meeting_id, meeting_time_ms",
        )?;
        let rows = stmt.query_map(params![pattern], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (meeting, at, text) = row?;
            add(
                meeting,
                SearchExcerpt {
                    kind: "note".into(),
                    segment_id: None,
                    meeting_time_ms: at,
                    text,
                },
            );
        }

        // 標題命中不附摘錄：標題本來就顯示在結果列上，再抄一次是雜訊。
        // 但它必須讓那場會議進入結果，否則「用會議名稱找會議」會找不到。
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM meetings WHERE title LIKE ?1 ESCAPE '\\'")?;
        let rows = stmt.query_map(params![pattern], |r| r.get::<_, i64>(0))?;
        for row in rows {
            hits.entry(row?).or_default();
        }

        // 排序與清單一致：最近的在前。使用者對「上週那場」的記憶是時間，
        // 不是相關性分數。
        let out: Vec<MeetingHit> = self
            .list_meetings()?
            .into_iter()
            .filter_map(|summary| {
                hits.remove(&summary.id)
                    .map(|(excerpts, total_hits)| MeetingHit {
                        summary,
                        excerpts,
                        total_hits,
                    })
            })
            .collect();
        Ok(out)
    }

    pub fn meeting(&self, meeting: MeetingId) -> Result<MeetingDetail> {
        let summary = self
            .conn
            .query_row(
                "SELECT m.id, m.title, m.state, m.started_at, m.ended_at,
                        m.meeting_time_ms, m.captured_audio_ms, m.high_seq,
                        (SELECT COUNT(*) FROM transcript_segments s WHERE s.meeting_id = m.id),
                        (SELECT COUNT(*) FROM notes n WHERE n.meeting_id = m.id AND n.removed = 0),
                        (SELECT COUNT(*) FROM documents d WHERE d.meeting_id = m.id)
                 FROM meetings m WHERE m.id = ?1",
                params![meeting],
                row_to_summary,
            )
            .optional()?
            .ok_or(StoreError::NoSuchMeeting(meeting))?;

        // 只讀目前指標指到的那個 revision。歷史版本另外查 revisions 表，
        // 不在這裡一併載入，否則長會議的重開成本會隨修訂次數成長。
        let mut stmt = self.conn.prepare(
            "SELECT r.segment_id, r.revision, r.origin, r.speaker_id, r.text, r.track,
                    r.meeting_start_ms, r.meeting_end_ms, s.user_edited
             FROM transcript_segments s
             JOIN transcript_segment_revisions r
               ON r.meeting_id = s.meeting_id
              AND r.segment_id = s.id
              AND r.revision   = s.current_revision
             WHERE s.meeting_id = ?1
             ORDER BY s.meeting_start_ms, s.id",
        )?;
        let segments = stmt
            .query_map(params![meeting], |r| {
                Ok(StoredSegment {
                    segment_id: r.get::<_, i64>(0)? as u64,
                    revision: r.get::<_, i64>(1)? as u32,
                    origin: Origin::parse(&r.get::<_, String>(2)?).unwrap_or(Origin::Provider),
                    speaker_id: r.get(3)?,
                    text: r.get(4)?,
                    track: Track::parse(&r.get::<_, String>(5)?).unwrap_or(Track::System),
                    meeting_start_ms: r.get::<_, i64>(6)? as u64,
                    meeting_end_ms: r.get::<_, i64>(7)? as u64,
                    user_edited: r.get::<_, i64>(8)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT id, text, meeting_time_ms, captured_audio_ms, event_seq
             FROM notes WHERE meeting_id = ?1 AND removed = 0 ORDER BY meeting_time_ms, id",
        )?;
        let notes = stmt
            .query_map(params![meeting], |r| {
                Ok(StoredNote {
                    note_id: r.get::<_, i64>(0)? as u64,
                    text: r.get(1)?,
                    meeting_time_ms: r.get::<_, i64>(2)? as u64,
                    captured_audio_ms: r.get::<_, i64>(3)? as u64,
                    event_seq: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT id, ordinal, proposed_name, confirmed_name, status
             FROM speakers WHERE meeting_id = ?1 AND status <> 'merged' ORDER BY ordinal",
        )?;
        let speakers = stmt
            .query_map(params![meeting], |r| {
                Ok(StoredSpeaker {
                    speaker_id: r.get(0)?,
                    ordinal: r.get::<_, i64>(1)? as u32,
                    proposed_name: r.get(2)?,
                    confirmed_name: r.get(3)?,
                    status: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(MeetingDetail {
            summary,
            segments,
            notes,
            speakers,
            runs: self.runs(meeting)?,
        })
    }

    pub fn runs(&self, meeting: MeetingId) -> Result<Vec<StoredRun>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.document_id, g.version_no, g.through_event_seq, g.status,
                    d.title, d.purpose, g.prompt, g.failure_reason, g.created_at
             FROM generation_runs g
             JOIN documents d ON d.id = g.document_id
             WHERE d.meeting_id = ?1
             ORDER BY g.version_no",
        )?;
        let rows = stmt.query_map(params![meeting], |r| {
            Ok(StoredRun {
                run_id: r.get(0)?,
                document_id: r.get(1)?,
                version_no: r.get::<_, i64>(2)? as u32,
                through_event_seq: r.get::<_, i64>(3)? as u64,
                status: r.get(4)?,
                title: r.get(5)?,
                purpose: r.get(6)?,
                prompt: r.get(7)?,
                failure_reason: r.get(8)?,
                created_at: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 一輪生成的成果區塊與引用，依位置排序。
    pub fn run_blocks(&self, run_id: i64) -> Result<Vec<DocumentBlock>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, position, kind, claim_kind, content
             FROM document_blocks WHERE run_id = ?1 ORDER BY position",
        )?;
        let raw = stmt
            .query_map(params![run_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)? as u32,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut refs_stmt = self.conn.prepare(
            "SELECT source_kind, source_id, source_revision, locator,
                    quoted_text, quoted_text_sha256, validation_status
             FROM source_refs WHERE block_id = ?1 ORDER BY id",
        )?;
        let mut out = Vec::with_capacity(raw.len());
        for (id, position, kind, claim_kind, content) in raw {
            let source_refs = refs_stmt
                .query_map(params![id], |r| {
                    Ok(SourceRef {
                        source_kind: r.get(0)?,
                        source_id: r.get(1)?,
                        source_revision: r.get::<_, i64>(2)? as u32,
                        locator: r.get(3)?,
                        quoted_text: r.get(4)?,
                        quoted_text_sha256: r.get(5)?,
                        validation_status: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.push(DocumentBlock {
                position,
                kind,
                claim_kind: ClaimKind::parse(&claim_kind).ok_or(StoreError::Corrupt {
                    seq: 0,
                    reason: format!("未知的 claim_kind：{claim_kind}"),
                })?,
                content,
                source_refs,
            });
        }
        Ok(out)
    }

    /// 快照涵蓋範圍內、已 final 的片段（§5.4.2）。
    ///
    /// 讀的是「當時的 revision」而不是目前指標：已匯出文件永遠讀當初的版本，
    /// 不被後續修訂靜默改寫（§11）。
    pub fn segments_through(
        &self,
        meeting: MeetingId,
        through_event_seq: u64,
    ) -> Result<Vec<StoredSegment>> {
        // 選版本的規則必須與 Session、投影、前端 reducer 完全一致：
        // 使用者修訂勝過 Provider，版本號其次。用純 MAX(revision) 的話，
        // 使用者改過之後 Provider 又送一版，畫面顯示使用者的內容，
        // 匯出的文件卻引用 Provider 的 —— 更正在離開 app 的那份成果裡消失。
        let mut stmt = self.conn.prepare(
            "SELECT segment_id, revision, origin, speaker_id, text, track,
                    meeting_start_ms, meeting_end_ms
             FROM (
                 SELECT r.*, ROW_NUMBER() OVER (
                            PARTITION BY r.segment_id
                            ORDER BY (r.origin = 'user') DESC, r.revision DESC
                        ) AS rn
                 FROM transcript_segment_revisions r
                 WHERE r.meeting_id = ?1 AND r.created_event_seq <= ?2
             )
             WHERE rn = 1
             ORDER BY meeting_start_ms, segment_id",
        )?;
        let rows = stmt.query_map(params![meeting, through_event_seq as i64], |r| {
            Ok(StoredSegment {
                segment_id: r.get::<_, i64>(0)? as u64,
                revision: r.get::<_, i64>(1)? as u32,
                origin: Origin::parse(&r.get::<_, String>(2)?).unwrap_or(Origin::Provider),
                speaker_id: r.get(3)?,
                text: r.get(4)?,
                track: Track::parse(&r.get::<_, String>(5)?).unwrap_or(Track::System),
                meeting_start_ms: r.get::<_, i64>(6)? as u64,
                meeting_end_ms: r.get::<_, i64>(7)? as u64,
                user_edited: Origin::parse(&r.get::<_, String>(2)?) == Some(Origin::User),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 快照涵蓋範圍內已經出現過的語者，以及他們在游標當下的名字（§5.4.2）。
    ///
    /// 從事件流折出來，不查 `speakers` 投影。投影裡的 `confirmed_name` 與
    /// `status` 是可變的，只用 `created_event_seq` 界定的話，游標之後才做的
    /// 改名或合併會出現在一個游標更早的 Prompt 裡 —— 那個游標就不再是凍結的
    /// 範圍，而生成期間使用者正好在確認語者名稱是很常見的事。
    ///
    /// 換欄位擋不住這件事：同一位語者可以被改名兩次，投影只留得下最後一次。
    /// 「seq 10 當下他叫什麼」只有日誌答得出來。
    pub fn speakers_through(
        &self,
        meeting: MeetingId,
        through_event_seq: u64,
    ) -> Result<Vec<StoredSpeaker>> {
        use std::collections::HashMap;

        let mut by_id: HashMap<String, StoredSpeaker> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for e in load_events(&self.conn, meeting)? {
            if e.seq > through_event_seq {
                break;
            }
            match &e.event {
                DomainEvent::SpeakerProposed {
                    speaker_id,
                    ordinal,
                    proposed_name,
                    ..
                } => {
                    let entry = by_id.entry(speaker_id.clone()).or_insert_with(|| {
                        order.push(speaker_id.clone());
                        StoredSpeaker {
                            speaker_id: speaker_id.clone(),
                            ordinal: *ordinal,
                            proposed_name: None,
                            confirmed_name: None,
                            status: "unconfirmed".into(),
                        }
                    });
                    entry.ordinal = *ordinal;
                    entry.proposed_name = proposed_name.clone();
                    if proposed_name.is_some() && entry.confirmed_name.is_none() {
                        entry.status = "proposed".into();
                    }
                }
                DomainEvent::SpeakerConfirmed { speaker_id, name }
                | DomainEvent::SpeakerRenamed { speaker_id, name } => {
                    if let Some(s) = by_id.get_mut(speaker_id) {
                        s.confirmed_name = Some(name.clone());
                        s.status = "confirmed".into();
                    }
                }
                DomainEvent::SpeakerMerged {
                    from_speaker_id, ..
                } => {
                    if let Some(s) = by_id.get_mut(from_speaker_id) {
                        s.status = "merged".into();
                    }
                }
                DomainEvent::SpeakerSplit {
                    new_speaker_id,
                    ordinal,
                    ..
                } => {
                    by_id.entry(new_speaker_id.clone()).or_insert_with(|| {
                        order.push(new_speaker_id.clone());
                        StoredSpeaker {
                            speaker_id: new_speaker_id.clone(),
                            ordinal: *ordinal,
                            proposed_name: None,
                            confirmed_name: None,
                            status: "unconfirmed".into(),
                        }
                    });
                }
                _ => {}
            }
        }

        let mut out: Vec<StoredSpeaker> = order
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .filter(|s| s.status != "merged")
            .collect();
        out.sort_by_key(|s| s.ordinal);
        Ok(out)
    }

    /// 快照涵蓋範圍內的人工筆記。
    ///
    /// 已知缺口:`removed` 與 `text` 沒有以游標界定,因為 `NoteRemoved` 與
    /// `NoteEdited` 目前沒有生產者。接上編輯與刪除的 UI 時,這兩件事要一起做,
    /// 否則游標就管不住筆記:
    ///
    /// - 刪除要記 `removed_event_seq`,游標之後的刪除不影響早先的快照。
    /// - 編輯要換一個版本身分。引用的 `source_revision` 是筆記的 `event_seq`,
    ///   就地改文字而不換版本,等於同一個版本指向不同內容,§9.6 的逐字比對
    ///   會對著改過的文字驗一段沒人說過的引文。
    pub fn notes_through(
        &self,
        meeting: MeetingId,
        through_event_seq: u64,
    ) -> Result<Vec<StoredNote>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, text, meeting_time_ms, captured_audio_ms, event_seq
             FROM notes WHERE meeting_id = ?1 AND removed = 0 AND event_seq <= ?2
             ORDER BY meeting_time_ms, id",
        )?;
        let rows = stmt.query_map(params![meeting, through_event_seq as i64], |r| {
            Ok(StoredNote {
                note_id: r.get::<_, i64>(0)? as u64,
                text: r.get(1)?,
                meeting_time_ms: r.get::<_, i64>(2)? as u64,
                captured_audio_ms: r.get::<_, i64>(3)? as u64,
                event_seq: r.get::<_, i64>(4)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 開一個讀取快照,之後所有 SELECT 看到同一個已提交狀態。
    ///
    /// 生成會跨多輪讀取證據:先建索引,每一輪再驗證引用。這些讀取如果各自
    /// 是獨立的隱含交易,中途提交的寫入會讓前後看到不同的世界。WAL 下的
    /// BEGIN DEFERRED 把讀取釘在一個快照上,而且不阻塞寫入者。
    ///
    /// 不可變的內容(片段版本)本來就不受影響,真正需要這層保護的是可變欄位:
    /// `notes.removed`、`notes.text`、`attachments.status`、`speakers` 的確認名稱。
    pub fn begin_read_snapshot(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN DEFERRED")?;
        Ok(())
    }

    pub fn end_read_snapshot(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// 引用驗證要用的證據原文（§9.6）。
    ///
    /// 一併回傳「這個版本是在哪個 event_seq 產生的」，因為驗證條件之一是
    /// 該版本必須落在本輪快照的涵蓋範圍內。只回文字的話呼叫端無從判斷。
    pub fn evidence_text(
        &self,
        meeting: MeetingId,
        source_kind: &str,
        source_id: &str,
        source_revision: u32,
    ) -> Result<Option<EvidenceText>> {
        let Ok(id) = source_id.parse::<i64>() else {
            return Ok(None);
        };
        let row = match source_kind {
            "transcript_segment" => self
                .conn
                .query_row(
                    "SELECT text, created_event_seq FROM transcript_segment_revisions
                     WHERE meeting_id = ?1 AND segment_id = ?2 AND revision = ?3",
                    params![meeting, id, source_revision as i64],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)),
                )
                .optional()?,
            // 筆記的版本就是它的 event_seq，兩者不符即非同一份內容
            "note" => self
                .conn
                .query_row(
                    "SELECT text, event_seq FROM notes
                     WHERE meeting_id = ?1 AND id = ?2 AND event_seq = ?3 AND removed = 0",
                    params![meeting, id, source_revision as i64],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)),
                )
                .optional()?,
            "attachment_chunk" => self
                .conn
                .query_row(
                    "SELECT c.text, a.event_seq FROM attachment_chunks c
                     JOIN attachments a ON a.id = c.attachment_id
                     WHERE c.id = ?1 AND c.extraction_revision = ?2
                       AND a.meeting_id = ?3 AND a.status <> 'removed'",
                    params![id, source_revision as i64, meeting],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)),
                )
                .optional()?,
            _ => None,
        };
        Ok(row.map(|(text, created_event_seq)| EvidenceText {
            text,
            created_event_seq,
        }))
    }

    /// 改名。走事件，不是就地 UPDATE。
    ///
    /// 使用者命名的東西一樣是決定性事件：它會出現在匯出檔的標題與歷史頁上，
    /// 而重建投影時得有辦法把它放回去。
    pub fn rename_meeting(&mut self, meeting: MeetingId, title: &str) -> Result<()> {
        self.append(
            meeting,
            &[(
                DomainEvent::MeetingRenamed {
                    title: title.to_owned(),
                },
                Timeline::new(0, 0),
            )],
        )?;
        Ok(())
    }

    /// 刪除整場會議，含事件日誌。
    ///
    /// 這是唯一會刪掉事件的路徑，而且是使用者明示的刪除，不是清理。
    /// 外鍵的 CASCADE 負責投影，日誌與 meetings 這裡明寫。
    pub fn delete_meeting(&mut self, meeting: MeetingId) -> Result<()> {
        let tx = self.conn.transaction()?;
        clear_projections(&tx, meeting)?;
        tx.execute(
            "DELETE FROM meeting_events WHERE meeting_id = ?1",
            params![meeting],
        )?;
        let n = tx.execute("DELETE FROM meetings WHERE id = ?1", params![meeting])?;
        tx.commit()?;
        if n == 0 {
            return Err(StoreError::NoSuchMeeting(meeting));
        }
        Ok(())
    }

    /// 讀出 GUI 存下的非敏感 Provider 設定。沒有設定過就回預設值。
    pub fn provider_settings(
        &self,
        kind: crate::model::ProviderKind,
    ) -> Result<crate::model::StoredProvider> {
        Ok(self
            .conn
            .query_row(
                "SELECT provider, model, base_url, options FROM provider_settings WHERE kind = ?1",
                params![kind.as_str()],
                |r| {
                    Ok(crate::model::StoredProvider {
                        provider: r.get(0)?,
                        model: r.get(1)?,
                        base_url: r.get(2)?,
                        options: r.get(3)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    /// 寫入非敏感設定。這張表不放密鑰（§5.6、§14），型別上也沒有那個欄位。
    pub fn set_provider_settings(
        &mut self,
        kind: crate::model::ProviderKind,
        v: &crate::model::StoredProvider,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO provider_settings (kind, provider, model, base_url, options)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT (kind) DO UPDATE SET
                 provider = excluded.provider,
                 model    = excluded.model,
                 base_url = excluded.base_url,
                 options  = excluded.options",
            params![
                kind.as_str(),
                v.provider,
                v.model,
                v.base_url,
                if v.options.is_empty() {
                    "{}"
                } else {
                    &v.options
                }
            ],
        )?;
        Ok(())
    }

    /// 把上次沒跑完的生成收尾。
    ///
    /// app 在生成途中被關掉，那筆 run 會永遠停在 `running`：歷史頁顯示
    /// 「生成中」，沒有重試入口，而它其實早就沒人在等了。Session 那條路徑
    /// 有 `abandon_running_generations` 處理換會議的情況，但它是記憶體裡的
    /// 東西，程式關掉就跟著沒了。
    ///
    /// 寫成事件而不是直接 UPDATE：`rebuild_projections` 會重播事件，
    /// 只改投影的話重建一次就又變回「生成中」。
    ///
    /// 回傳被收尾的筆數，供啟動日誌記錄。
    pub fn close_abandoned_runs(&mut self) -> Result<usize> {
        let mut stmt = self.conn.prepare(
            "SELECT d.meeting_id, g.id
               FROM generation_runs g
               JOIN documents d ON d.id = g.document_id
              WHERE g.status IN ('queued', 'running')",
        )?;
        let orphans: Vec<(MeetingId, i64)> = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        for (meeting, run_id) in &orphans {
            self.append(
                *meeting,
                &[(
                    DomainEvent::GenerationFailed {
                        run_id: *run_id,
                        reason: "上次的生成沒有跑完就結束了，請再試一次。".into(),
                    },
                    crate::model::Timeline::default(),
                )],
            )?;
        }
        Ok(orphans.len())
    }

    /// 這場會議寫到第幾號事件。
    ///
    /// 為已結束的會議建立摘要時，這就是快照游標：不會再有新事件，涵蓋到這裡
    /// 就是涵蓋整場。判斷「游標之後還有沒有內容」不要用它，用
    /// `content_events_after` —— 快照自己的紀錄也會推進這個值。
    pub fn high_seq(&self, meeting: MeetingId) -> Result<u64> {
        Ok(self
            .conn
            .query_row(
                "SELECT high_seq FROM meetings WHERE id = ?1",
                params![meeting],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .ok_or(StoreError::NoSuchMeeting(meeting))? as u64)
    }

    /// 這場會議既有的文件 id。同一場會議只產生一份文件，版本鏈掛在它下面。
    ///
    /// 回 `None` 代表這場會議還沒有任何摘要，呼叫端要配一個新的 document_id。
    pub fn document_of(&self, meeting: MeetingId) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM documents WHERE meeting_id = ?1",
                params![meeting],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// 配發一個文件 id 與一個生成版本 id。
    ///
    /// 事件必須自帶 id（否則重播時 rowid 會隨插入順序改變），所以配發者在
    /// 程式這邊。但配發者只能有一個：曾經是 Session 在會議開始時快取一次
    /// `MAX(id)+1`、歷史頁的摘要每次重讀，於是錄音中對舊會議做摘要就會拿到
    /// 同一個號碼，而撞號的後果是一場會議的成果被另一場覆寫（見 002 migration）。
    ///
    /// 遞增與讀取在同一個 statement 裡，因此就算配發之後隔很久才真的寫入
    /// 事件，號碼也不會被別人拿走 —— 那是 `MAX(id)+1` 做不到的地方。
    ///
    /// 兩個號碼一起發：呼叫端可能只用得到其中一個（同一場會議的第二次生成
    /// 沿用既有文件），沒用到的號碼就空著。空號沒有代價，重複的號碼有。
    pub fn allocate_run_ids(&mut self) -> Result<RunIds> {
        Ok(RunIds {
            document_id: self.next_id("documents")?,
            run_id: self.next_id("generation_runs")?,
        })
    }

    fn next_id(&mut self, name: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "UPDATE id_sequences SET next = next + 1 WHERE name = ?1 RETURNING next - 1",
            params![name],
            |r| r.get(0),
        )?)
    }
}

#[derive(Debug, Clone)]
pub struct EvidenceText {
    pub text: String,
    /// 這個版本是由哪個事件產生的。驗證要確認它落在快照涵蓋範圍內。
    pub created_event_seq: u64,
}

/// 一輪生成要用到的兩個 id，由 [`Store::allocate_run_ids`] 一起發出。
#[derive(Debug, Clone, Copy, Default)]
pub struct RunIds {
    pub document_id: i64,
    pub run_id: i64,
}

fn row_to_summary(r: &rusqlite::Row<'_>) -> rusqlite::Result<MeetingSummary> {
    Ok(MeetingSummary {
        id: r.get(0)?,
        title: r.get(1)?,
        state: MeetingState::parse(&r.get::<_, String>(2)?).unwrap_or(MeetingState::Idle),
        started_at: r.get(3)?,
        ended_at: r.get(4)?,
        meeting_time_ms: r.get::<_, i64>(5)? as u64,
        captured_audio_ms: r.get::<_, i64>(6)? as u64,
        high_seq: r.get::<_, i64>(7)? as u64,
        segment_count: r.get::<_, i64>(8)? as u64,
        note_count: r.get::<_, i64>(9)? as u64,
        document_count: r.get::<_, i64>(10)? as u64,
    })
}

fn load_events(conn: &Connection, meeting: MeetingId) -> Result<Vec<StoredEvent>> {
    let mut stmt = conn.prepare(
        "SELECT seq, meeting_time_ms, captured_audio_ms, created_at, payload
         FROM meeting_events WHERE meeting_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![meeting], |r| {
        Ok((
            r.get::<_, i64>(0)? as u64,
            Timeline::new(r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64),
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, timeline, created_at, payload) = row?;
        let event = serde_json::from_str(&payload).map_err(|e| StoreError::Corrupt {
            seq,
            reason: e.to_string(),
        })?;
        out.push(StoredEvent {
            seq,
            timeline,
            created_at,
            event,
        });
    }
    Ok(out)
}

/// 刪除所有投影，保留 `meeting_events` 與 `meetings`。
///
/// 順序依外鍵相依由葉往根。`ON DELETE CASCADE` 會處理子表，
/// 但明寫出來比較能看出這裡到底清了什麼。
fn clear_projections(tx: &Transaction<'_>, meeting: MeetingId) -> Result<()> {
    tx.execute(
        "DELETE FROM source_refs WHERE block_id IN (
             SELECT b.id FROM document_blocks b
             JOIN generation_runs g ON g.id = b.run_id
             JOIN documents d ON d.id = g.document_id WHERE d.meeting_id = ?1)",
        params![meeting],
    )?;
    tx.execute(
        "DELETE FROM document_blocks WHERE run_id IN (
             SELECT g.id FROM generation_runs g
             JOIN documents d ON d.id = g.document_id WHERE d.meeting_id = ?1)",
        params![meeting],
    )?;
    tx.execute(
        "DELETE FROM generation_runs WHERE document_id IN
             (SELECT id FROM documents WHERE meeting_id = ?1)",
        params![meeting],
    )?;
    tx.execute(
        "DELETE FROM documents WHERE meeting_id = ?1",
        params![meeting],
    )?;
    tx.execute(
        "DELETE FROM attachment_chunks WHERE attachment_id IN
             (SELECT id FROM attachments WHERE meeting_id = ?1)",
        params![meeting],
    )?;
    tx.execute(
        "DELETE FROM attachments WHERE meeting_id = ?1",
        params![meeting],
    )?;
    for table in [
        "transcript_segment_speaker_spans",
        "transcript_segments",
        "transcript_segment_revisions",
        "notes",
        "speakers",
        "audio_segments",
    ] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE meeting_id = ?1"),
            params![meeting],
        )?;
    }
    Ok(())
}

/// 投影更新必須影響到列。
///
/// UPDATE 影響 0 列在 SQLite 眼中完全合法，於是「事件寫進日誌了，投影卻沒動」
/// 這件事會安靜地發生：畫面顯示成功，磁碟上什麼都沒有。實際踩過一次
/// （SpeakerConfirmed 更新一列不存在的 speakers），因此每個 UPDATE 分支
/// 都要說出自己預期影響幾列，不符就當成日誌損壞往上報。
fn expect_touched(seq: u64, what: &str, affected: usize) -> Result<()> {
    if affected == 0 {
        return Err(StoreError::Corrupt {
            seq,
            reason: format!("{what} 沒有對應的投影列，事件無法套用"),
        });
    }
    Ok(())
}

/// 把一筆事件套用到投影。`append` 與 `rebuild_projections` 共用這一份。
fn project(
    tx: &Transaction<'_>,
    meeting: MeetingId,
    seq: u64,
    tl: Timeline,
    // now 是事件自己的掛鐘時間，不是現在。重播必須產生與初次寫入相同的投影。
    now: &str,
    event: &DomainEvent,
) -> Result<()> {
    match event {
        DomainEvent::MeetingStateChanged { state } => {
            // started_at / ended_at 用 COALESCE 只寫一次：重播時不能因為
            // 掛鐘變了就改寫歷史上的開始時間。
            let n = tx.execute(
                "UPDATE meetings SET state = ?2,
                     started_at = CASE WHEN ?3 THEN COALESCE(started_at, ?5) ELSE started_at END,
                     ended_at   = CASE WHEN ?4 THEN COALESCE(ended_at,   ?5) ELSE ended_at   END
                 WHERE id = ?1",
                params![
                    meeting,
                    state.as_str(),
                    *state == MeetingState::Recording,
                    // 收尾失敗的會議也是結束了。少了這一項，被中斷的會議在
                    // 歷史頁上沒有結束時間，看起來像還在跑
                    matches!(state, MeetingState::Completed | MeetingState::Failed),
                    now
                ],
            )?;
            expect_touched(seq, "會議", n)?;
        }

        DomainEvent::MeetingRenamed { title } => {
            let n = tx.execute(
                "UPDATE meetings SET title = ?2 WHERE id = ?1",
                params![meeting, title],
            )?;
            expect_touched(seq, "會議", n)?;
        }

        DomainEvent::TranscriptSegmentFinalized { segment }
        | DomainEvent::TranscriptSegmentRevised { segment }
        | DomainEvent::TranscriptSegmentEdited { segment } => {
            insert_revision(tx, meeting, seq, segment, now)?;
            let user_edited = matches!(event, DomainEvent::TranscriptSegmentEdited { .. })
                || segment.origin == Origin::User;
            // 三條規則，逐字對應前端的 supersedes 與 Session 的判定（§5.3）：
            //
            // 1. Provider 不得覆蓋使用者修訂，版本號再高也不行。
            // 2. 版本號較高才前進，擋掉 Provider 重連後重送的舊版本。
            // 3. 版本號相同時只有「使用者蓋 Provider」成立。相同版本互蓋雖然
            //    改不到文字（revisions 是 DO NOTHING），但會改寫 meeting_start_ms。
            //
            // 第二條原本只寫在 Session 裡，這裡靠 MAX(revision) 近似，結果是
            // 兩層規則不等價：Provider 的 r3 會蓋掉使用者的 r2。不等價的兩層
            // 防禦比一層更糟，因為它讓人以為下層擋得住。
            tx.execute(
                "INSERT INTO transcript_segments
                     (meeting_id, id, current_revision, stability, user_edited, meeting_start_ms)
                 VALUES (?1,?2,?3,'final',?4,?5)
                 ON CONFLICT (meeting_id, id) DO UPDATE SET
                     current_revision = excluded.current_revision,
                     stability        = 'final',
                     user_edited      = MAX(user_edited, excluded.user_edited),
                     meeting_start_ms = excluded.meeting_start_ms
                 WHERE (excluded.user_edited = 1 OR transcript_segments.user_edited = 0)
                   AND (excluded.current_revision > transcript_segments.current_revision
                        OR (excluded.current_revision = transcript_segments.current_revision
                            AND excluded.user_edited = 1
                            AND transcript_segments.user_edited = 0))",
                params![
                    meeting,
                    segment.segment_id as i64,
                    segment.revision as i64,
                    user_edited as i64,
                    segment.meeting_start_ms as i64
                ],
            )?;
        }

        DomainEvent::SpeakerReassigned {
            segment_id,
            revision,
            speaker_id,
        } => {
            // 只改指定版本的歸屬。這是 Provider 重新指派，不是新內容，
            // 因此不建立新 revision。
            //
            // 已知缺口：這違反「revisions 不可變」，舊游標的讀取會拿到現在的
            // 歸屬，重新匯出一份舊文件時語者會變。目前沒有生產者；接上
            // diarization 的二次校正時要改成建立新 revision，而不是就地改。
            let n = tx.execute(
                "UPDATE transcript_segment_revisions SET speaker_id = ?4
                 WHERE meeting_id = ?1 AND segment_id = ?2 AND revision = ?3",
                params![meeting, *segment_id as i64, *revision as i64, speaker_id],
            )?;
            expect_touched(seq, &format!("片段 {segment_id} r{revision}"), n)?;
        }

        DomainEvent::NoteAdded { note_id, text } => {
            tx.execute(
                "INSERT INTO notes (meeting_id, id, event_seq, text, meeting_time_ms,
                                    captured_audio_ms, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT (meeting_id, id) DO NOTHING",
                params![
                    meeting,
                    *note_id as i64,
                    seq as i64,
                    text,
                    tl.meeting_time_ms as i64,
                    tl.captured_audio_ms as i64,
                    now
                ],
            )?;
        }
        DomainEvent::NoteEdited { note_id, text } => {
            let n = tx.execute(
                "UPDATE notes SET text = ?3 WHERE meeting_id = ?1 AND id = ?2",
                params![meeting, *note_id as i64, text],
            )?;
            expect_touched(seq, &format!("筆記 {note_id}"), n)?;
        }
        DomainEvent::NoteRemoved { note_id } => {
            // 標記而不是刪除：已匯出的文件可能引用它，實體刪除會讓引用指向虛空
            let n = tx.execute(
                "UPDATE notes SET removed = 1 WHERE meeting_id = ?1 AND id = ?2",
                params![meeting, *note_id as i64],
            )?;
            expect_touched(seq, &format!("筆記 {note_id}"), n)?;
        }

        DomainEvent::SpeakerProposed {
            speaker_id,
            ordinal,
            proposed_name,
            provider_labels,
        } => {
            tx.execute(
                "INSERT INTO speakers (meeting_id, id, ordinal, proposed_name, status,
                                       provider_labels, created_event_seq)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT (meeting_id, id) DO UPDATE SET
                     proposed_name   = excluded.proposed_name,
                     provider_labels = excluded.provider_labels",
                params![
                    meeting,
                    speaker_id,
                    *ordinal as i64,
                    proposed_name,
                    if proposed_name.is_some() {
                        "proposed"
                    } else {
                        "unconfirmed"
                    },
                    serde_json::to_string(provider_labels)?,
                    seq as i64
                ],
            )?;
        }
        DomainEvent::SpeakerConfirmed { speaker_id, name }
        | DomainEvent::SpeakerRenamed { speaker_id, name } => {
            let n = tx.execute(
                "UPDATE speakers SET confirmed_name = ?3, status = 'confirmed'
                 WHERE meeting_id = ?1 AND id = ?2",
                params![meeting, speaker_id, name],
            )?;
            // 沒有先被 SpeakerProposed 過的語者不能被確認：那代表 UI 在
            // 確認一個日誌裡不存在的人，而確認結果會無聲消失
            expect_touched(seq, &format!("語者 {speaker_id}"), n)?;
        }
        DomainEvent::SpeakerMerged {
            from_speaker_id,
            into_speaker_id,
        } => {
            let n = tx.execute(
                "UPDATE speakers SET status = 'merged', merged_into = ?3
                 WHERE meeting_id = ?1 AND id = ?2",
                params![meeting, from_speaker_id, into_speaker_id],
            )?;
            expect_touched(seq, &format!("語者 {from_speaker_id}"), n)?;
            // 底下的片段改派可以是 0 列：這位語者可能還沒說過話
            tx.execute(
                "UPDATE transcript_segment_revisions SET speaker_id = ?3
                 WHERE meeting_id = ?1 AND speaker_id = ?2",
                params![meeting, from_speaker_id, into_speaker_id],
            )?;
        }
        DomainEvent::SpeakerSplit {
            new_speaker_id,
            ordinal,
            ..
        } => {
            tx.execute(
                "INSERT INTO speakers (meeting_id, id, ordinal, status, created_event_seq)
                 VALUES (?1,?2,?3,'unconfirmed',?4)
                 ON CONFLICT (meeting_id, id) DO NOTHING",
                params![meeting, new_speaker_id, *ordinal as i64, seq as i64],
            )?;
        }

        DomainEvent::AttachmentAdded {
            attachment_id,
            path,
            mime,
            sha256,
        } => {
            tx.execute(
                "INSERT INTO attachments (id, meeting_id, event_seq, path, mime, sha256,
                                          status, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,'pending',?7)
                 ON CONFLICT (id) DO NOTHING",
                params![attachment_id, meeting, seq as i64, path, mime, sha256, now],
            )?;
        }
        DomainEvent::AttachmentExtracted {
            attachment_id,
            extraction_revision,
            chunks,
        } => {
            let n = tx.execute(
                "UPDATE attachments SET status = 'extracted' WHERE id = ?1",
                params![attachment_id],
            )?;
            expect_touched(seq, &format!("附件 {attachment_id}"), n)?;
            // 同一 extraction_revision 重播時先清掉，避免重建後 chunk 加倍
            tx.execute(
                "DELETE FROM attachment_chunks
                 WHERE attachment_id = ?1 AND extraction_revision = ?2",
                params![attachment_id, *extraction_revision as i64],
            )?;
            for c in chunks {
                tx.execute(
                    "INSERT INTO attachment_chunks
                         (attachment_id, extraction_revision, page_no, start_offset,
                          end_offset, text)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                    params![
                        attachment_id,
                        *extraction_revision as i64,
                        c.page_no,
                        c.start_offset as i64,
                        c.end_offset as i64,
                        c.text
                    ],
                )?;
            }
        }
        DomainEvent::AttachmentRemoved { attachment_id } => {
            let n = tx.execute(
                "UPDATE attachments SET status = 'removed' WHERE id = ?1",
                params![attachment_id],
            )?;
            expect_touched(seq, &format!("附件 {attachment_id}"), n)?;
        }

        DomainEvent::AudioSegmentFinalized { segment } => {
            tx.execute(
                "INSERT INTO audio_segments
                     (id, meeting_id, track, source_epoch, path, captured_start_ms,
                      captured_end_ms, meeting_start_ms, meeting_end_ms, is_silence_fill,
                      checksum, created_event_seq)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                 ON CONFLICT (id) DO NOTHING",
                params![
                    segment.id,
                    meeting,
                    segment.track.as_str(),
                    segment.source_epoch as i64,
                    segment.path,
                    segment.captured_start_ms as i64,
                    segment.captured_end_ms as i64,
                    segment.meeting_start_ms as i64,
                    segment.meeting_end_ms as i64,
                    segment.is_silence_fill as i64,
                    segment.checksum,
                    seq as i64
                ],
            )?;
        }

        DomainEvent::SnapshotCreated {
            document_id,
            run_id,
            parent_run_id,
            version_no,
            purpose,
            title,
            through_event_seq,
            prompt,
        } => {
            tx.execute(
                "INSERT INTO documents (id, meeting_id, purpose, title, created_at)
                 VALUES (?1,?2,?3,?4,?5) ON CONFLICT (id) DO NOTHING",
                params![document_id, meeting, purpose, title, now],
            )?;
            tx.execute(
                "INSERT INTO generation_runs
                     (id, document_id, parent_run_id, version_no, through_event_seq,
                      prompt, status, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,'running',?7)
                 ON CONFLICT (id) DO NOTHING",
                params![
                    run_id,
                    document_id,
                    parent_run_id,
                    *version_no as i64,
                    *through_event_seq as i64,
                    prompt,
                    now
                ],
            )?;
        }
        DomainEvent::GenerationCompleted {
            run_id,
            blocks,
            usage,
        } => {
            let n = tx.execute(
                "UPDATE generation_runs SET status = 'completed', usage = ?2 WHERE id = ?1",
                params![run_id, serde_json::to_string(usage)?],
            )?;
            expect_touched(seq, &format!("生成 {run_id}"), n)?;
            tx.execute(
                "DELETE FROM document_blocks WHERE run_id = ?1",
                params![run_id],
            )?;
            for b in blocks {
                tx.execute(
                    "INSERT INTO document_blocks (run_id, position, kind, claim_kind, content)
                     VALUES (?1,?2,?3,?4,?5)",
                    params![
                        run_id,
                        b.position as i64,
                        b.kind,
                        b.claim_kind.as_str(),
                        b.content
                    ],
                )?;
                let block_id = tx.last_insert_rowid();
                for s in &b.source_refs {
                    tx.execute(
                        "INSERT INTO source_refs (block_id, source_kind, source_id,
                             source_revision, locator, quoted_text, quoted_text_sha256,
                             validation_status)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                        params![
                            block_id,
                            s.source_kind,
                            s.source_id,
                            s.source_revision as i64,
                            s.locator,
                            s.quoted_text,
                            s.quoted_text_sha256,
                            s.validation_status
                        ],
                    )?;
                }
            }
        }
        DomainEvent::GenerationFailed { run_id, reason } => {
            // 失敗的 run 保留快照游標，重試可以沿用同一個涵蓋範圍
            let n = tx.execute(
                "UPDATE generation_runs SET status = 'failed', failure_reason = ?2 WHERE id = ?1",
                params![run_id, reason],
            )?;
            expect_touched(seq, &format!("生成 {run_id}"), n)?;
        }
    }
    Ok(())
}

fn insert_revision(
    tx: &Transaction<'_>,
    meeting: MeetingId,
    seq: u64,
    s: &SegmentRevision,
    now: &str,
) -> Result<()> {
    // revisions 不可變。重播時同一 (segment_id, revision) 會再出現一次，
    // DO NOTHING 讓重建保持冪等，同時擋掉「改寫既有版本」這件事。
    tx.execute(
        "INSERT INTO transcript_segment_revisions
             (meeting_id, segment_id, revision, text, speaker_id, track,
              meeting_start_ms, meeting_end_ms, captured_start_ms, captured_end_ms,
              echo_likelihood, overlap_group_id, provider_stream_id, provider_result_id,
              rollover_generation, origin, created_event_seq, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
         ON CONFLICT (meeting_id, segment_id, revision) DO NOTHING",
        params![
            meeting,
            s.segment_id as i64,
            s.revision as i64,
            s.text,
            s.speaker_id,
            s.track.as_str(),
            s.meeting_start_ms as i64,
            s.meeting_end_ms as i64,
            s.captured_start_ms as i64,
            s.captured_end_ms as i64,
            s.echo_likelihood,
            s.overlap_group_id,
            s.provider_stream_id,
            s.provider_result_id,
            s.rollover_generation as i64,
            s.origin.as_str(),
            seq as i64,
            now
        ],
    )?;
    tx.execute(
        "DELETE FROM transcript_segment_speaker_spans
         WHERE meeting_id = ?1 AND segment_id = ?2 AND revision = ?3",
        params![meeting, s.segment_id as i64, s.revision as i64],
    )?;
    for (i, span) in s.speaker_spans.iter().enumerate() {
        tx.execute(
            "INSERT INTO transcript_segment_speaker_spans
                 (meeting_id, segment_id, revision, span_index, speaker_id,
                  meeting_start_ms, meeting_end_ms, char_start, char_end)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                meeting,
                s.segment_id as i64,
                s.revision as i64,
                i as i64,
                span.speaker_id,
                span.meeting_start_ms as i64,
                span.meeting_end_ms as i64,
                span.char_start as i64,
                span.char_end as i64
            ],
        )?;
    }
    Ok(())
}

// ── 去重（§5.3.2） ──────────────────────────────────────────────────────

/// 音訊區間的正規化量化單位。
///
/// Provider 對同一段音訊回報的起訖會有幾十毫秒的抖動，逐毫秒比對等於永遠
/// 判不出重複。20 ms 是常見 STT frame 的量級：小於一個 frame 的差異視為
/// 同一區間，大於它就當作不同內容。這個值連同容差在 M2 固定（§11）。
pub const INTERVAL_QUANTUM_MS: u64 = 20;

/// 跨串流的去重身分（§5.3.2）。
///
/// 重連與輪替之後 Provider 的三個識別碼全都會變，唯一不變的是音訊區間，
/// 因此去重鍵是正規化後的 `(track, captured_start_ms, captured_end_ms)`。
pub fn interval_key(track: Track, start_ms: u64, end_ms: u64) -> (Track, u64, u64) {
    let q = INTERVAL_QUANTUM_MS;
    (track, (start_ms + q / 2) / q * q, (end_ms + q / 2) / q * q)
}

/// 判重用的文字正規化：去頭尾空白、壓縮內部連續空白。
///
/// 不動大小寫也不動標點：`API` 與 `api` 在技術會議裡可能是刻意的區別，
/// 而 Provider 修掉標點正是「同區間但文字不同」該建立新 revision 的情形。
pub fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// 狀態變更走事件，這是唯一的路徑
    fn set_state(s: &mut Store, m: MeetingId, state: MeetingState) {
        s.append(
            m,
            &[(
                DomainEvent::MeetingStateChanged { state },
                Timeline::new(0, 0),
            )],
        )
        .unwrap();
    }

    fn recording(s: &mut Store, m: MeetingId) {
        set_state(s, m, MeetingState::Recording);
    }

    /// 兩個配發者不能算出同一個號碼。
    ///
    /// 舊做法是各自讀 `MAX(id)+1`：錄音中的會議在開始時讀一次就記在記憶體
    /// 裡，歷史頁的摘要每次重讀，兩邊都還沒寫入時就會拿到同一個 run_id。
    /// 撞號之後 `SnapshotCreated` 的 ON CONFLICT DO NOTHING 靜默吞掉，接著
    /// `GenerationCompleted` 就把另一場會議的成果刪掉換成這一場的。
    #[test]
    fn allocated_ids_are_never_handed_out_twice_even_before_anything_is_written() {
        let mut s = Store::new(db::open_in_memory().unwrap());
        let n = 50;
        let mut runs = Vec::with_capacity(n);
        let mut docs = Vec::with_capacity(n);
        for _ in 0..n {
            // 一個字都還沒寫進 documents 或 generation_runs
            let ids = s.allocate_run_ids().unwrap();
            runs.push(ids.run_id);
            docs.push(ids.document_id);
        }
        runs.sort_unstable();
        runs.dedup();
        docs.sort_unstable();
        docs.dedup();
        assert_eq!(runs.len(), n, "run_id 重複配發");
        assert_eq!(docs.len(), n, "document_id 重複配發");
    }

    /// 刪掉會議留下的空號不再使用。
    ///
    /// `MAX(id)+1` 在刪除之後會倒退，於是新的一輪生成拿到一個曾經屬於別人的
    /// 號碼。號碼只前進，代價是空號，而空號沒有代價。
    #[test]
    fn ids_move_forward_even_after_the_rows_that_used_them_are_gone() {
        let mut s = Store::new(db::open_in_memory().unwrap());
        let first = s.allocate_run_ids().unwrap();
        let m = s.create_meeting("要被刪掉的").unwrap();
        s.append(
            m,
            &[(
                DomainEvent::SnapshotCreated {
                    document_id: first.document_id,
                    run_id: first.run_id,
                    parent_run_id: None,
                    version_no: 1,
                    purpose: "meeting-summary".into(),
                    title: "會議摘要".into(),
                    through_event_seq: 0,
                    prompt: String::new(),
                },
                Timeline::new(0, 0),
            )],
        )
        .unwrap();
        s.delete_meeting(m).unwrap();
        let next = s.allocate_run_ids().unwrap();
        assert!(next.run_id > first.run_id, "刪除之後號碼倒退了");
    }

    #[test]
    fn test_an_abandoned_recording_is_closed_as_failed_not_completed() {
        // app 被強制關閉後狀態會停在 recording。標成 completed 會讓一場
        // 逐字稿可能斷在半句話的會議看起來跟正常結束的一樣。
        let (mut s, m) = store();
        recording(&mut s, m);

        assert_eq!(s.close_abandoned_meetings().unwrap(), 1);
        let listed = s.list_meetings().unwrap();
        let found = listed.iter().find(|x| x.id == m).unwrap();
        assert_eq!(found.state, MeetingState::Failed);
        assert!(found.ended_at.is_some(), "沒有補上結束時間");
    }

    /// 重建投影之後，被中斷的會議不能變回「錄音中」。
    ///
    /// `failed` 與使用者改的標題原本都是就地 UPDATE，日誌裡沒有它們。
    /// 重建是修復投影的手段，而它會把這兩件事一起抹掉 —— 那不是重建，
    /// 是資料遺失。
    #[test]
    fn a_rebuild_restores_the_failed_state_and_the_user_title_from_the_log() {
        let (mut s, m) = store();
        recording(&mut s, m);
        s.rename_meeting(m, "Q3 預算會議").unwrap();
        assert_eq!(s.close_abandoned_meetings().unwrap(), 1);

        s.rebuild_projections(m).unwrap();

        let found = s.list_meetings().unwrap().into_iter().find(|x| x.id == m);
        let found = found.expect("會議不見了");
        assert_eq!(found.state, MeetingState::Failed, "重建之後狀態退回去了");
        assert!(found.ended_at.is_some(), "重建之後沒有結束時間");
        assert_eq!(
            s.meeting_title(m).unwrap(),
            "Q3 預算會議",
            "重建之後標題不見了"
        );
    }

    #[test]
    fn test_closing_abandoned_meetings_leaves_finished_ones_alone() {
        let (mut s, m) = store();
        set_state(&mut s, m, MeetingState::Completed);
        assert_eq!(
            s.close_abandoned_meetings().unwrap(),
            0,
            "動到了已結束的會議"
        );
    }

    #[test]
    fn test_closing_abandoned_meetings_is_idempotent() {
        // 每次啟動都會跑一次，第二次不該再動任何東西
        let (mut s, m) = store();
        set_state(&mut s, m, MeetingState::Paused);
        assert_eq!(s.close_abandoned_meetings().unwrap(), 1);
        assert_eq!(s.close_abandoned_meetings().unwrap(), 0);
    }

    #[test]
    fn test_an_abandoned_meeting_keeps_its_transcript() {
        // 收尾只改狀態。內容全部保留，否則使用者會連殘缺的紀錄都拿不到。
        let (mut s, m) = store();
        let seq = *s
            .append(
                m,
                &[(
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "會議中斷前說的話", Origin::Provider),
                    },
                    Timeline::new(0, 0),
                )],
            )
            .unwrap()
            .last()
            .unwrap();
        recording(&mut s, m);
        s.close_abandoned_meetings().unwrap();

        // 用實際的序號，不是 u64::MAX —— 那個值進 SQLite 會溢位成 -1
        let segs = s.segments_through(m, seq).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "會議中斷前說的話");
    }

    fn store() -> (Store, MeetingId) {
        let mut s = Store::new(db::open_in_memory().unwrap());
        let m = s.create_meeting("測試會議").unwrap();
        (s, m)
    }

    fn seg(id: u64, revision: u32, text: &str, origin: Origin) -> SegmentRevision {
        SegmentRevision {
            segment_id: id,
            revision,
            text: text.into(),
            speaker_id: Some("s1".into()),
            track: Track::System,
            meeting_start_ms: id * 1000,
            meeting_end_ms: id * 1000 + 900,
            captured_start_ms: id * 1000,
            captured_end_ms: id * 1000 + 900,
            echo_likelihood: None,
            overlap_group_id: None,
            provider_stream_id: Some("stream-a".into()),
            provider_result_id: Some(format!("r{id}-{revision}")),
            rollover_generation: 0,
            origin,
            speaker_spans: Vec::new(),
        }
    }

    fn tl(ms: u64) -> Timeline {
        Timeline::new(ms, ms)
    }

    /// 投影的可比較快照，排除會因插入順序而變的 surrogate id。
    fn dump(s: &Store, m: MeetingId) -> String {
        let d = s.meeting(m).unwrap();
        let runs = s.runs(m).unwrap();
        let blocks: Vec<_> = runs
            .iter()
            .map(|r| s.run_blocks(r.run_id).unwrap())
            .collect();
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            d.summary.state, d.segments, d.notes, d.speakers, runs, blocks
        )
    }

    #[test]
    fn append_assigns_contiguous_sequence_numbers() {
        let (mut s, m) = store();
        let seqs = s
            .append(
                m,
                &[
                    (
                        DomainEvent::MeetingStateChanged {
                            state: MeetingState::Recording,
                        },
                        tl(0),
                    ),
                    (
                        DomainEvent::NoteAdded {
                            note_id: 1,
                            text: "一".into(),
                        },
                        tl(100),
                    ),
                ],
            )
            .unwrap();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[test]
    fn high_seq_does_not_regress_after_a_projection_rebuild() {
        let (mut s, m) = store();
        s.append(
            m,
            &[(
                DomainEvent::NoteAdded {
                    note_id: 1,
                    text: "一".into(),
                },
                tl(10),
            )],
        )
        .unwrap();
        s.rebuild_projections(m).unwrap();
        let next = s
            .append(
                m,
                &[(
                    DomainEvent::NoteAdded {
                        note_id: 2,
                        text: "二".into(),
                    },
                    tl(20),
                )],
            )
            .unwrap();
        assert_eq!(next, vec![2], "seq 倒退會讓已匯出文件的游標指到別的內容");
    }

    #[test]
    fn rebuild_from_the_event_log_reproduces_the_same_projection() {
        let (mut s, m) = store();
        s.append(
            m,
            &[
                (
                    DomainEvent::MeetingStateChanged {
                        state: MeetingState::Recording,
                    },
                    tl(0),
                ),
                (
                    DomainEvent::SpeakerProposed {
                        speaker_id: "s1".into(),
                        ordinal: 1,
                        proposed_name: Some("語者 1".into()),
                        provider_labels: vec!["spk_0".into()],
                    },
                    tl(10),
                ),
                (
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "第一句", Origin::Provider),
                    },
                    tl(1000),
                ),
                (
                    DomainEvent::TranscriptSegmentEdited {
                        segment: seg(1, 2, "使用者改過的第一句", Origin::User),
                    },
                    tl(1500),
                ),
                (
                    DomainEvent::NoteAdded {
                        note_id: 1,
                        text: "重點".into(),
                    },
                    tl(2000),
                ),
                (
                    DomainEvent::SpeakerConfirmed {
                        speaker_id: "s1".into(),
                        name: "小明".into(),
                    },
                    tl(2100),
                ),
                (
                    DomainEvent::SnapshotCreated {
                        document_id: 1,
                        run_id: 1,
                        parent_run_id: None,
                        version_no: 1,
                        purpose: "meeting-summary".into(),
                        title: "會議摘要".into(),
                        through_event_seq: 5,
                        prompt: String::new(),
                    },
                    tl(2200),
                ),
                (
                    DomainEvent::GenerationCompleted {
                        run_id: 1,
                        blocks: vec![DocumentBlock {
                            position: 0,
                            kind: "paragraph".into(),
                            claim_kind: ClaimKind::Fact,
                            content: "會議決定 X".into(),
                            source_refs: vec![SourceRef {
                                source_kind: "transcript_segment".into(),
                                source_id: "1".into(),
                                source_revision: 2,
                                locator: "1000-1900".into(),
                                quoted_text: "使用者改過的第一句".into(),
                                quoted_text_sha256: "abc".into(),
                                validation_status: "valid".into(),
                            }],
                        }],
                        usage: serde_json::json!({"tokens": 10}),
                    },
                    tl(2300),
                ),
                (
                    DomainEvent::MeetingStateChanged {
                        state: MeetingState::Completed,
                    },
                    tl(3000),
                ),
            ],
        )
        .unwrap();

        let before = dump(&s, m);
        s.rebuild_projections(m).unwrap();
        assert_eq!(before, dump(&s, m), "重播結果與逐筆寫入不一致");
    }

    #[test]
    fn rebuilding_twice_does_not_duplicate_rows() {
        let (mut s, m) = store();
        s.append(
            m,
            &[
                (
                    DomainEvent::AttachmentAdded {
                        attachment_id: 1,
                        path: "/tmp/a.pdf".into(),
                        mime: "application/pdf".into(),
                        sha256: "x".into(),
                    },
                    tl(0),
                ),
                (
                    DomainEvent::AttachmentExtracted {
                        attachment_id: 1,
                        extraction_revision: 1,
                        chunks: vec![AttachmentChunk {
                            page_no: Some(1),
                            start_offset: 0,
                            end_offset: 5,
                            text: "hello".into(),
                        }],
                    },
                    tl(10),
                ),
            ],
        )
        .unwrap();
        s.rebuild_projections(m).unwrap();
        s.rebuild_projections(m).unwrap();
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM attachment_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_stale_provider_revision_cannot_move_the_pointer_backwards() {
        let (mut s, m) = store();
        s.append(
            m,
            &[
                (
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "原始", Origin::Provider),
                    },
                    tl(0),
                ),
                (
                    DomainEvent::TranscriptSegmentEdited {
                        segment: seg(1, 2, "使用者改過", Origin::User),
                    },
                    tl(10),
                ),
                // Provider 重連後重送 r1
                (
                    DomainEvent::TranscriptSegmentRevised {
                        segment: seg(1, 1, "重連後的舊結果", Origin::Provider),
                    },
                    tl(20),
                ),
            ],
        )
        .unwrap();
        let d = s.meeting(m).unwrap();
        assert_eq!(d.segments[0].text, "使用者改過");
        assert_eq!(d.segments[0].revision, 2);
        assert!(d.segments[0].user_edited);
    }

    #[test]
    /// 游標之後才確認的名字不得進入更早的快照。
    ///
    /// 生成期間使用者確認語者名稱是很常見的動作，而查詢原本只用
    /// `created_event_seq` 界定「這位語者出現過沒有」，名字卻讀的是投影上
    /// 現在的值。那讓「凍結的涵蓋範圍」在名字這一欄破了個洞。
    #[test]
    fn a_name_confirmed_after_the_cursor_does_not_leak_into_the_snapshot() {
        let (mut s, m) = store();
        let seqs = s
            .append(
                m,
                &[
                    (
                        DomainEvent::SpeakerProposed {
                            speaker_id: "s1".into(),
                            ordinal: 1,
                            proposed_name: None,
                            provider_labels: vec![],
                        },
                        Timeline::new(0, 0),
                    ),
                    (
                        DomainEvent::SpeakerConfirmed {
                            speaker_id: "s1".into(),
                            name: "李部長".into(),
                        },
                        Timeline::new(1000, 1000),
                    ),
                ],
            )
            .unwrap();
        let (proposed_at, confirmed_at) = (seqs[0], seqs[1]);

        // 游標停在「出現了」那一刻：這時他還沒有名字
        let at_proposal = s.speakers_through(m, proposed_at).unwrap();
        assert_eq!(at_proposal.len(), 1);
        assert_eq!(
            at_proposal[0].confirmed_name, None,
            "游標之後的名字漏進來了"
        );
        assert_eq!(at_proposal[0].status, "unconfirmed");

        // 游標包含確認之後才看得到名字
        let at_confirm = s.speakers_through(m, confirmed_at).unwrap();
        assert_eq!(at_confirm[0].confirmed_name.as_deref(), Some("李部長"));
        assert_eq!(at_confirm[0].status, "confirmed");

        // 再改一次名。游標仍停在第一次確認，看到的必須是第一個名字 ——
        // 投影只留得下最後一個，所以這一項只有日誌答得出來
        let renamed = s
            .append(
                m,
                &[(
                    DomainEvent::SpeakerRenamed {
                        speaker_id: "s1".into(),
                        name: "李次長".into(),
                    },
                    Timeline::new(2000, 2000),
                )],
            )
            .unwrap()[0];
        assert_eq!(
            s.speakers_through(m, confirmed_at).unwrap()[0]
                .confirmed_name
                .as_deref(),
            Some("李部長"),
            "游標之後的改名蓋掉了快照當下的名字"
        );
        assert_eq!(
            s.speakers_through(m, renamed).unwrap()[0]
                .confirmed_name
                .as_deref(),
            Some("李次長")
        );
    }

    #[test]
    fn snapshot_reads_the_revision_current_at_its_cursor_not_the_latest() {
        let (mut s, m) = store();
        let seqs = s
            .append(
                m,
                &[(
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "當時的內容", Origin::Provider),
                    },
                    tl(0),
                )],
            )
            .unwrap();
        let cursor = seqs[0];
        s.append(
            m,
            &[(
                DomainEvent::TranscriptSegmentEdited {
                    segment: seg(1, 2, "之後改過的內容", Origin::User),
                },
                tl(10),
            )],
        )
        .unwrap();

        let at_cursor = s.segments_through(m, cursor).unwrap();
        assert_eq!(
            at_cursor[0].text, "當時的內容",
            "已匯出文件被後續修訂改寫了"
        );
        let now = s.meeting(m).unwrap();
        assert_eq!(now.segments[0].text, "之後改過的內容");
    }

    #[test]
    fn removing_a_note_keeps_the_row_so_citations_still_resolve() {
        let (mut s, m) = store();
        let seqs = s
            .append(
                m,
                &[(
                    DomainEvent::NoteAdded {
                        note_id: 1,
                        text: "會被刪掉".into(),
                    },
                    tl(0),
                )],
            )
            .unwrap();
        s.append(m, &[(DomainEvent::NoteRemoved { note_id: 1 }, tl(10))])
            .unwrap();
        assert!(s.meeting(m).unwrap().notes.is_empty());
        assert!(s.notes_through(m, seqs[0]).unwrap().is_empty());
        let raw: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE meeting_id = ?1",
                params![m],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, 1, "實體刪除會讓已匯出文件的引用指向虛空");
    }

    #[test]
    fn merging_a_speaker_reassigns_every_segment_it_owned() {
        let (mut s, m) = store();
        s.append(
            m,
            &[
                (
                    DomainEvent::SpeakerProposed {
                        speaker_id: "s1".into(),
                        ordinal: 1,
                        proposed_name: None,
                        provider_labels: vec![],
                    },
                    tl(0),
                ),
                (
                    DomainEvent::SpeakerProposed {
                        speaker_id: "s2".into(),
                        ordinal: 2,
                        proposed_name: None,
                        provider_labels: vec![],
                    },
                    tl(1),
                ),
                (
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "一", Origin::Provider),
                    },
                    tl(10),
                ),
                (
                    DomainEvent::SpeakerMerged {
                        from_speaker_id: "s1".into(),
                        into_speaker_id: "s2".into(),
                    },
                    tl(20),
                ),
            ],
        )
        .unwrap();
        let d = s.meeting(m).unwrap();
        assert_eq!(d.segments[0].speaker_id.as_deref(), Some("s2"));
        assert_eq!(d.speakers.len(), 1, "已合併的語者不該再出現在名單裡");
    }

    #[test]
    fn speaker_spans_are_replaced_not_appended_when_a_revision_replays() {
        let (mut s, m) = store();
        let mut sr = seg(1, 1, "你先說 我補充", Origin::Provider);
        sr.speaker_spans = vec![
            SpeakerSpan {
                speaker_id: "s1".into(),
                meeting_start_ms: 1000,
                meeting_end_ms: 1400,
                char_start: 0,
                char_end: 3,
            },
            SpeakerSpan {
                speaker_id: "s2".into(),
                meeting_start_ms: 1400,
                meeting_end_ms: 1900,
                char_start: 4,
                char_end: 7,
            },
        ];
        s.append(
            m,
            &[(
                DomainEvent::TranscriptSegmentFinalized { segment: sr },
                tl(0),
            )],
        )
        .unwrap();
        s.rebuild_projections(m).unwrap();
        let n: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM transcript_segment_speaker_spans",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn interval_key_quantizes_provider_jitter_to_the_same_bucket() {
        // 同一段音訊，Provider 兩次回報差了幾毫秒
        assert_eq!(
            interval_key(Track::System, 1_003, 1_897),
            interval_key(Track::System, 998, 1_902)
        );
        // 差距超過一個量子就是不同區間
        assert_ne!(
            interval_key(Track::System, 1_000, 1_900),
            interval_key(Track::System, 1_000, 1_940)
        );
        // 軌道不同就不是同一段音訊，回音重複由 §8.2 另外處理
        assert_ne!(
            interval_key(Track::Mic, 1_000, 1_900),
            interval_key(Track::System, 1_000, 1_900)
        );
    }

    #[test]
    fn normalize_text_folds_whitespace_but_keeps_case_and_punctuation() {
        assert_eq!(normalize_text("  我們  用 API  "), "我們 用 API");
        assert_ne!(normalize_text("API"), normalize_text("api"));
        assert_ne!(normalize_text("好的。"), normalize_text("好的"));
    }

    #[test]
    fn a_corrupt_payload_is_reported_instead_of_silently_skipped() {
        let (mut s, m) = store();
        s.append(
            m,
            &[(
                DomainEvent::NoteAdded {
                    note_id: 1,
                    text: "一".into(),
                },
                tl(0),
            )],
        )
        .unwrap();
        s.conn
            .execute(
                "UPDATE meeting_events SET payload = '{not json' WHERE meeting_id = ?1",
                params![m],
            )
            .unwrap();
        assert!(matches!(
            s.rebuild_projections(m),
            Err(StoreError::Corrupt { seq: 1, .. })
        ));
    }

    #[test]
    fn a_generation_killed_mid_flight_does_not_stay_running_forever() {
        // app 在生成途中被關掉，那筆 run 會停在 running：歷史頁顯示「生成中」，
        // 沒有重試入口，而它早就沒人在等了
        let (mut s, m) = store();
        s.append(
            m,
            &[(
                DomainEvent::SnapshotCreated {
                    document_id: 1,
                    run_id: 1,
                    parent_run_id: None,
                    version_no: 1,
                    purpose: "meeting-summary".into(),
                    title: "會議摘要".into(),
                    through_event_seq: 0,
                    prompt: String::new(),
                },
                tl(0),
            )],
        )
        .unwrap();
        assert_eq!(s.runs(m).unwrap()[0].status, "running");

        assert_eq!(s.close_abandoned_runs().unwrap(), 1);
        let run = s.runs(m).unwrap().into_iter().next().unwrap();
        assert_eq!(run.status, "failed");
        assert!(run.failure_reason.is_some(), "沒有說明為什麼失敗");

        // 寫成事件而不是只改投影：重建投影之後不該又變回「生成中」
        s.rebuild_projections(m).unwrap();
        assert_eq!(s.runs(m).unwrap()[0].status, "failed");

        // 已經收尾過的不會再被收一次
        assert_eq!(s.close_abandoned_runs().unwrap(), 0);
    }

    #[test]
    fn the_export_title_follows_the_name_the_user_gave_the_meeting() {
        // 匯出檔的 <title> 與 <h1> 用這個值。先前用的是 documents.title，
        // 而那一律是「會議摘要」，於是每一份匯出檔在瀏覽器分頁上都分不出誰是誰。
        let (mut s, m) = store();
        assert_eq!(s.meeting_title(m).unwrap(), "測試會議");
        s.rename_meeting(m, "八月預算審查").unwrap();
        assert_eq!(s.meeting_title(m).unwrap(), "八月預算審查");
    }

    /* ── 跨會議搜尋（§2.1） ───────────────────────────────────── */

    fn searchable() -> Store {
        let mut s = Store::new(db::open_in_memory().unwrap());
        let a = s.create_meeting("報價討論").unwrap();
        s.append(
            a,
            &[
                (
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "維運的月費區間下週給你", Origin::Provider),
                    },
                    tl(1000),
                ),
                (
                    DomainEvent::NoteAdded {
                        note_id: 1,
                        text: "記得追維運報價".into(),
                    },
                    tl(2000),
                ),
            ],
        )
        .unwrap();

        let b = s.create_meeting("預算審查").unwrap();
        s.append(
            b,
            &[(
                DomainEvent::TranscriptSegmentFinalized {
                    segment: seg(1, 1, "決議凍結兩百萬元", Origin::Provider),
                },
                tl(1000),
            )],
        )
        .unwrap();
        s
    }

    #[test]
    fn search_finds_a_meeting_by_what_was_said_in_it() {
        let s = searchable();
        let hits = s.search_meetings("月費", 3).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].summary.title, "報價討論");
        // 命中原因要回得出來，否則使用者不知道為什麼是這一場
        assert_eq!(hits[0].excerpts[0].kind, "transcript");
        assert!(hits[0].excerpts[0].text.contains("月費"));
    }

    #[test]
    fn search_covers_notes_and_titles_not_just_the_transcript() {
        let s = searchable();
        // 只出現在筆記裡
        let by_note = s.search_meetings("記得追", 3).unwrap();
        assert_eq!(by_note.len(), 1);
        assert_eq!(by_note[0].excerpts[0].kind, "note");

        // 只出現在標題裡。標題已經顯示在結果列上，不另外附摘錄
        let by_title = s.search_meetings("預算審查", 3).unwrap();
        assert_eq!(by_title.len(), 1);
        assert!(by_title[0].excerpts.is_empty());
    }

    #[test]
    fn search_counts_every_hit_even_when_it_only_returns_a_few() {
        let mut s = Store::new(db::open_in_memory().unwrap());
        let m = s.create_meeting("很多次").unwrap();
        let events: Vec<_> = (1..=5u64)
            .map(|i| {
                (
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(i, 1, "報價", Origin::Provider),
                    },
                    tl(i * 1000),
                )
            })
            .collect();
        s.append(m, &events).unwrap();

        let hits = s.search_meetings("報價", 2).unwrap();
        assert_eq!(hits[0].excerpts.len(), 2, "摘錄沒有被截斷");
        assert_eq!(hits[0].total_hits, 5, "截斷之後就數不出總數了");
    }

    #[test]
    fn search_treats_wildcards_as_literal_characters() {
        // 使用者搜「100%」時，未跳脫的 % 會讓 LIKE 命中所有東西
        let mut s = Store::new(db::open_in_memory().unwrap());
        let m = s.create_meeting("百分比").unwrap();
        s.append(
            m,
            &[(
                DomainEvent::TranscriptSegmentFinalized {
                    segment: seg(1, 1, "毛利大概三成", Origin::Provider),
                },
                tl(1000),
            )],
        )
        .unwrap();
        assert!(
            s.search_meetings("%", 3).unwrap().is_empty(),
            "% 被當成萬用字元"
        );
        assert!(
            s.search_meetings("_", 3).unwrap().is_empty(),
            "_ 被當成萬用字元"
        );
        assert!(
            s.search_meetings("三成", 3).unwrap().len() == 1,
            "正常字串搜不到"
        );
    }

    #[test]
    fn search_only_sees_the_current_revision_of_a_segment() {
        // 修訂過的片段，搜舊內容不該把它挖出來：使用者看到的逐字稿是新版，
        // 命中一段畫面上不存在的文字只會讓人以為程式壞了
        let (mut s, m) = store();
        s.append(
            m,
            &[
                (
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "達物族的傳統領域", Origin::Provider),
                    },
                    tl(1000),
                ),
                (
                    DomainEvent::TranscriptSegmentRevised {
                        segment: seg(1, 2, "達悟族的傳統領域", Origin::User),
                    },
                    tl(2000),
                ),
            ],
        )
        .unwrap();
        assert!(
            s.search_meetings("達物族", 3).unwrap().is_empty(),
            "搜到了被改掉的舊版"
        );
        assert_eq!(s.search_meetings("達悟族", 3).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_query_returns_nothing_rather_than_everything() {
        // 呼叫端在沒有查詢字串時該顯示完整清單，那是另一條路徑
        let s = searchable();
        assert!(s.search_meetings("", 3).unwrap().is_empty());
        assert!(s.search_meetings("   ", 3).unwrap().is_empty());
    }

    /// 搜尋走 LIKE 掃描而不是 FTS5，這個決定只有在實測數字之下才成立。
    /// 用 `--ignored` 執行，數字記在 `search_meetings` 的註解裡。
    #[test]
    #[ignore = "效能量測，跑一次數十秒。用 --ignored 執行"]
    fn probe_how_long_a_like_scan_takes_at_realistic_scale() {
        // 兩小時會議約 2600 段。50 場是「用了兩年」的量級
        const MEETINGS: usize = 50;
        const SEGMENTS: u64 = 2600;
        let mut s = Store::new(db::open_in_memory().unwrap());
        for n in 0..MEETINGS {
            let m = s.create_meeting(&format!("會議 {n}")).unwrap();
            let events: Vec<_> = (1..=SEGMENTS)
                .map(|i| {
                    (
                        DomainEvent::TranscriptSegmentFinalized {
                            segment: seg(
                                i,
                                1,
                                "這是一段長度接近真實逐字稿的內容，講的是報價、範圍與時程",
                                Origin::Provider,
                            ),
                        },
                        tl(i * 1000),
                    )
                })
                .collect();
            s.append(m, &events).unwrap();
        }

        // 兩端都量：全命中是最壞情況（每一列都要搬進 Rust），
        // 少量命中才是真實查詢的樣子，兩個數字差很多就不能只報一個
        let timed = |q: &str| {
            let started = std::time::Instant::now();
            let hits = s.search_meetings(q, 3).unwrap();
            let elapsed = started.elapsed();
            eprintln!(
                "{} 場 × {SEGMENTS} 段（{} 列）搜「{q}」命中 {} 場，耗時 {:?}",
                MEETINGS,
                MEETINGS as u64 * SEGMENTS,
                hits.len(),
                elapsed
            );
            (hits.len(), elapsed)
        };
        let (_, rare) = timed("這個詞不存在於任何一段");
        let (all, elapsed) = timed("時程");
        assert_eq!(all, MEETINGS);
        assert!(rare < elapsed, "沒命中反而比較慢，量測方式有問題");
        // 使用者一邊打字一邊搜，超過這條線就該換索引了
        assert!(elapsed.as_millis() < 500, "掃描慢到會擋住輸入：{elapsed:?}");
    }

    #[test]
    fn list_meetings_reports_counts_without_replaying_the_log() {
        let (mut s, m) = store();
        s.append(
            m,
            &[
                (
                    DomainEvent::MeetingStateChanged {
                        state: MeetingState::Recording,
                    },
                    tl(0),
                ),
                (
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "一", Origin::Provider),
                    },
                    tl(1000),
                ),
                (
                    DomainEvent::NoteAdded {
                        note_id: 1,
                        text: "n".into(),
                    },
                    tl(1100),
                ),
                (
                    DomainEvent::MeetingStateChanged {
                        state: MeetingState::Completed,
                    },
                    tl(5000),
                ),
            ],
        )
        .unwrap();
        let list = s.list_meetings().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].segment_count, 1);
        assert_eq!(list[0].note_count, 1);
        assert_eq!(list[0].meeting_time_ms, 5000);
        assert_eq!(list[0].state, MeetingState::Completed);
        assert!(list[0].started_at.is_some() && list[0].ended_at.is_some());
    }

    #[test]
    fn a_higher_provider_revision_still_cannot_overwrite_a_user_edit() {
        let (mut s, m) = store();
        s.append(
            m,
            &[
                (
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "原始", Origin::Provider),
                    },
                    tl(0),
                ),
                (
                    DomainEvent::TranscriptSegmentEdited {
                        segment: seg(1, 2, "使用者改過", Origin::User),
                    },
                    tl(10),
                ),
                // Provider 之後送出版本更高的結果。版本號比較大不代表它有權覆蓋。
                (
                    DomainEvent::TranscriptSegmentRevised {
                        segment: seg(1, 3, "Provider 的 r3", Origin::Provider),
                    },
                    tl(20),
                ),
            ],
        )
        .unwrap();
        let d = s.meeting(m).unwrap();
        assert_eq!(d.segments[0].text, "使用者改過");
        assert_eq!(d.segments[0].revision, 2);
        // 該版本本身仍然存進 revisions，只是不成為目前指標
        let n: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM transcript_segment_revisions WHERE meeting_id = ?1",
                params![m],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3, "被拒絕的版本仍要留在不可變歷史裡");
    }

    #[test]
    fn a_later_user_edit_still_wins_over_an_earlier_one() {
        let (mut s, m) = store();
        s.append(
            m,
            &[
                (
                    DomainEvent::TranscriptSegmentEdited {
                        segment: seg(1, 1, "第一次改", Origin::User),
                    },
                    tl(0),
                ),
                (
                    DomainEvent::TranscriptSegmentEdited {
                        segment: seg(1, 2, "第二次改", Origin::User),
                    },
                    tl(10),
                ),
            ],
        )
        .unwrap();
        assert_eq!(s.meeting(m).unwrap().segments[0].text, "第二次改");
    }

    #[test]
    fn replaying_every_event_kind_does_not_trip_the_row_count_contract() {
        // expect_touched 讓「投影影響 0 列」變成錯誤。這個測試存在是為了證明
        // 它不會在合法情況下誤判 —— 尤其是重建，重建會把每個事件再套用一次。
        let (mut s, m) = store();
        s.append(
            m,
            &[
                (
                    DomainEvent::MeetingStateChanged {
                        state: MeetingState::Recording,
                    },
                    tl(0),
                ),
                (
                    DomainEvent::SpeakerProposed {
                        speaker_id: "s1".into(),
                        ordinal: 1,
                        proposed_name: Some("語者 1".into()),
                        provider_labels: vec!["spk_0".into()],
                    },
                    tl(10),
                ),
                (
                    DomainEvent::SpeakerProposed {
                        speaker_id: "s2".into(),
                        ordinal: 2,
                        proposed_name: None,
                        provider_labels: vec![],
                    },
                    tl(11),
                ),
                (
                    DomainEvent::SpeakerRenamed {
                        speaker_id: "s1".into(),
                        name: "小明".into(),
                    },
                    tl(12),
                ),
                (
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "一", Origin::Provider),
                    },
                    tl(100),
                ),
                (
                    DomainEvent::TranscriptSegmentRevised {
                        segment: seg(1, 2, "一改", Origin::Provider),
                    },
                    tl(110),
                ),
                (
                    DomainEvent::SpeakerReassigned {
                        segment_id: 1,
                        revision: 2,
                        speaker_id: Some("s2".into()),
                    },
                    tl(120),
                ),
                (
                    DomainEvent::NoteAdded {
                        note_id: 1,
                        text: "一".into(),
                    },
                    tl(200),
                ),
                (
                    DomainEvent::NoteEdited {
                        note_id: 1,
                        text: "一改".into(),
                    },
                    tl(210),
                ),
                (DomainEvent::NoteRemoved { note_id: 1 }, tl(220)),
                (
                    DomainEvent::SpeakerSplit {
                        from_speaker_id: "s2".into(),
                        new_speaker_id: "s3".into(),
                        ordinal: 3,
                    },
                    tl(230),
                ),
                (
                    DomainEvent::SpeakerMerged {
                        from_speaker_id: "s3".into(),
                        into_speaker_id: "s2".into(),
                    },
                    tl(240),
                ),
                (
                    DomainEvent::AttachmentAdded {
                        attachment_id: 1,
                        path: "/tmp/a.pdf".into(),
                        mime: "application/pdf".into(),
                        sha256: "x".into(),
                    },
                    tl(300),
                ),
                (
                    DomainEvent::AttachmentExtracted {
                        attachment_id: 1,
                        extraction_revision: 1,
                        chunks: vec![AttachmentChunk {
                            page_no: Some(1),
                            start_offset: 0,
                            end_offset: 5,
                            text: "hello".into(),
                        }],
                    },
                    tl(310),
                ),
                (DomainEvent::AttachmentRemoved { attachment_id: 1 }, tl(320)),
                (
                    DomainEvent::AudioSegmentFinalized {
                        segment: AudioSegment {
                            id: 1,
                            track: Track::Mic,
                            source_epoch: 0,
                            path: "/tmp/a.wav".into(),
                            captured_start_ms: 0,
                            captured_end_ms: 900,
                            meeting_start_ms: 0,
                            meeting_end_ms: 900,
                            is_silence_fill: false,
                            checksum: "sha".into(),
                        },
                    },
                    tl(400),
                ),
                (
                    DomainEvent::SnapshotCreated {
                        document_id: 1,
                        run_id: 1,
                        parent_run_id: None,
                        version_no: 1,
                        purpose: "p".into(),
                        title: "t".into(),
                        through_event_seq: 10,
                        prompt: String::new(),
                    },
                    tl(500),
                ),
                (
                    DomainEvent::GenerationFailed {
                        run_id: 1,
                        reason: "限流".into(),
                    },
                    tl(510),
                ),
                (
                    DomainEvent::SnapshotCreated {
                        document_id: 1,
                        run_id: 2,
                        parent_run_id: Some(1),
                        version_no: 2,
                        purpose: "p".into(),
                        title: "t".into(),
                        through_event_seq: 12,
                        prompt: String::new(),
                    },
                    tl(520),
                ),
                (
                    DomainEvent::GenerationCompleted {
                        run_id: 2,
                        blocks: vec![],
                        usage: serde_json::json!({}),
                    },
                    tl(530),
                ),
                (
                    DomainEvent::MeetingStateChanged {
                        state: MeetingState::Completed,
                    },
                    tl(600),
                ),
            ],
        )
        .expect("正常寫入時列數契約就誤判了");

        let before = dump(&s, m);
        // 重建會把同一批事件再套用一次。冪等的 INSERT 不會影響列數，
        // 但每個 UPDATE 分支都會再跑一遍，這才是契約真正的壓力測試。
        s.rebuild_projections(m).expect("重建時列數契約誤判");
        assert_eq!(before, dump(&s, m));
        s.rebuild_projections(m).expect("第二次重建時列數契約誤判");
        assert_eq!(before, dump(&s, m));
    }

    #[test]
    fn an_equal_revision_from_a_provider_does_not_displace_a_user_edit() {
        // 等版本號是三層規則裡最容易寫歪的一格
        let (mut s, m) = store();
        s.append(
            m,
            &[
                (
                    DomainEvent::TranscriptSegmentEdited {
                        segment: seg(1, 2, "使用者改過", Origin::User),
                    },
                    tl(0),
                ),
                (
                    DomainEvent::TranscriptSegmentRevised {
                        segment: seg(1, 2, "Provider 的同版本", Origin::Provider),
                    },
                    tl(10),
                ),
            ],
        )
        .unwrap();
        assert_eq!(s.meeting(m).unwrap().segments[0].text, "使用者改過");
    }

    #[test]
    fn the_exported_evidence_shows_the_same_revision_the_screen_does() {
        // 這條分歧的代價是使用者的更正在離開 app 的成果裡消失，
        // 而畫面上完全看不出來。四層規則必須判出同一個版本。
        let (mut s, m) = store();
        let seqs = s
            .append(
                m,
                &[
                    (
                        DomainEvent::TranscriptSegmentFinalized {
                            segment: seg(1, 1, "原始", Origin::Provider),
                        },
                        tl(0),
                    ),
                    (
                        DomainEvent::TranscriptSegmentEdited {
                            segment: seg(1, 2, "使用者改過", Origin::User),
                        },
                        tl(10),
                    ),
                    (
                        DomainEvent::TranscriptSegmentRevised {
                            segment: seg(1, 3, "Provider 的 r3", Origin::Provider),
                        },
                        tl(20),
                    ),
                ],
            )
            .unwrap();
        let cursor = *seqs.last().unwrap();
        let on_screen = &s.meeting(m).unwrap().segments[0];
        let in_evidence = &s.segments_through(m, cursor).unwrap()[0];
        assert_eq!(on_screen.text, "使用者改過");
        assert_eq!(in_evidence.text, on_screen.text);
        assert_eq!(in_evidence.revision, on_screen.revision);
        assert!(in_evidence.user_edited, "證據沒有標示這段被使用者改過");
    }

    #[test]
    fn evidence_before_a_user_edit_still_shows_what_was_current_then() {
        // 游標的意義不變：早於使用者修訂的快照仍然看到當時的 Provider 版本
        let (mut s, m) = store();
        let seqs = s
            .append(
                m,
                &[(
                    DomainEvent::TranscriptSegmentFinalized {
                        segment: seg(1, 1, "當時的內容", Origin::Provider),
                    },
                    tl(0),
                )],
            )
            .unwrap();
        let early = seqs[0];
        s.append(
            m,
            &[(
                DomainEvent::TranscriptSegmentEdited {
                    segment: seg(1, 2, "之後改的", Origin::User),
                },
                tl(10),
            )],
        )
        .unwrap();
        assert_eq!(s.segments_through(m, early).unwrap()[0].text, "當時的內容");
    }

    #[test]
    fn an_identical_revision_number_does_not_displace_the_current_pointer() {
        // 等版本號那一格：三層規則裡最容易寫歪的地方，四種組合都要一致
        let (mut s, m) = store();
        s.append(
            m,
            &[(
                DomainEvent::TranscriptSegmentFinalized {
                    segment: seg(1, 1, "第一次", Origin::Provider),
                },
                tl(0),
            )],
        )
        .unwrap();
        // Provider 對 Provider，同版本：不動
        let mut same = seg(1, 1, "同版本重送", Origin::Provider);
        same.meeting_start_ms = 999_999;
        s.append(
            m,
            &[(
                DomainEvent::TranscriptSegmentRevised { segment: same },
                tl(10),
            )],
        )
        .unwrap();
        let d = s.meeting(m).unwrap();
        assert_eq!(d.segments[0].text, "第一次");
        assert_eq!(
            d.segments[0].meeting_start_ms, 1000,
            "同版本重送改寫了片段的起始時間"
        );
    }
}
