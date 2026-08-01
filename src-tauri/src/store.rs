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
    pub quoted_text_sha256: String,
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
            Self::MeetingStateChanged { .. } => (None, None),
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
        for e in &events {
            project(&tx, meeting, e.seq, e.timeline, &e.created_at, &e.event)?;
        }
        tx.commit()?;
        Ok(())
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
            "SELECT id, text, meeting_time_ms, captured_audio_ms
             FROM notes WHERE meeting_id = ?1 AND removed = 0 ORDER BY meeting_time_ms, id",
        )?;
        let notes = stmt
            .query_map(params![meeting], |r| {
                Ok(StoredNote {
                    note_id: r.get::<_, i64>(0)? as u64,
                    text: r.get(1)?,
                    meeting_time_ms: r.get::<_, i64>(2)? as u64,
                    captured_audio_ms: r.get::<_, i64>(3)? as u64,
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
        let mut stmt = self.conn.prepare(
            "SELECT r.segment_id, r.revision, r.origin, r.speaker_id, r.text, r.track,
                    r.meeting_start_ms, r.meeting_end_ms
             FROM transcript_segment_revisions r
             WHERE r.meeting_id = ?1
               AND r.created_event_seq <= ?2
               AND r.revision = (
                   SELECT MAX(r2.revision) FROM transcript_segment_revisions r2
                   WHERE r2.meeting_id = r.meeting_id
                     AND r2.segment_id = r.segment_id
                     AND r2.created_event_seq <= ?2)
             ORDER BY r.meeting_start_ms, r.segment_id",
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
                user_edited: false,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn notes_through(
        &self,
        meeting: MeetingId,
        through_event_seq: u64,
    ) -> Result<Vec<StoredNote>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, text, meeting_time_ms, captured_audio_ms
             FROM notes WHERE meeting_id = ?1 AND removed = 0 AND event_seq <= ?2
             ORDER BY meeting_time_ms, id",
        )?;
        let rows = stmt.query_map(params![meeting, through_event_seq as i64], |r| {
            Ok(StoredNote {
                note_id: r.get::<_, i64>(0)? as u64,
                text: r.get(1)?,
                meeting_time_ms: r.get::<_, i64>(2)? as u64,
                captured_audio_ms: r.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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

    pub fn rename_meeting(&mut self, meeting: MeetingId, title: &str) -> Result<()> {
        // 標題由使用者直接命名，不是事件的投影，因此就地更新
        let n = self.conn.execute(
            "UPDATE meetings SET title = ?2 WHERE id = ?1",
            params![meeting, title],
        )?;
        if n == 0 {
            return Err(StoreError::NoSuchMeeting(meeting));
        }
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
        kind: crate::config::ProviderKind,
    ) -> Result<crate::config::StoredProvider> {
        Ok(self
            .conn
            .query_row(
                "SELECT provider, model, base_url, options FROM provider_settings WHERE kind = ?1",
                params![kind.as_str()],
                |r| {
                    Ok(crate::config::StoredProvider {
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
        kind: crate::config::ProviderKind,
        v: &crate::config::StoredProvider,
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

    /// 全域 rowid 的目前上限，供 Session 配發新 id。
    ///
    /// 事件必須自帶 id（否則重播時 rowid 會隨插入順序改變），因此配發者是
    /// Session 而不是資料庫。開新會議時從這裡續號。
    pub fn id_seeds(&self) -> Result<IdSeeds> {
        let max = |table: &str| -> Result<i64> {
            Ok(self.conn.query_row(
                &format!("SELECT COALESCE(MAX(id), 0) FROM {table}"),
                [],
                |r| r.get(0),
            )?)
        };
        Ok(IdSeeds {
            document_id: max("documents")?,
            run_id: max("generation_runs")?,
            attachment_id: max("attachments")?,
            audio_segment_id: max("audio_segments")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct EvidenceText {
    pub text: String,
    /// 這個版本是由哪個事件產生的。驗證要確認它落在快照涵蓋範圍內。
    pub created_event_seq: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IdSeeds {
    pub document_id: i64,
    pub run_id: i64,
    // 附件與音訊分段還沒有產生者（前者無 UI，後者是 M1 的硬體工作）。
    // 種子先備著，因為配發者是 Session，接上時不該再改這個結構。
    #[allow(dead_code)]
    pub attachment_id: i64,
    #[allow(dead_code)]
    pub audio_segment_id: i64,
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
            tx.execute(
                "UPDATE meetings SET state = ?2,
                     started_at = CASE WHEN ?3 THEN COALESCE(started_at, ?5) ELSE started_at END,
                     ended_at   = CASE WHEN ?4 THEN COALESCE(ended_at,   ?5) ELSE ended_at   END
                 WHERE id = ?1",
                params![
                    meeting,
                    state.as_str(),
                    *state == MeetingState::Recording,
                    *state == MeetingState::Completed,
                    now
                ],
            )?;
        }

        DomainEvent::TranscriptSegmentFinalized { segment }
        | DomainEvent::TranscriptSegmentRevised { segment }
        | DomainEvent::TranscriptSegmentEdited { segment } => {
            insert_revision(tx, meeting, seq, segment, now)?;
            let user_edited = matches!(event, DomainEvent::TranscriptSegmentEdited { .. })
                || segment.origin == Origin::User;
            // 指標只前進不後退：Provider 重連後重送舊版本不得讓內容倒退（§5.3）
            tx.execute(
                "INSERT INTO transcript_segments
                     (meeting_id, id, current_revision, stability, user_edited, meeting_start_ms)
                 VALUES (?1,?2,?3,'final',?4,?5)
                 ON CONFLICT (meeting_id, id) DO UPDATE SET
                     current_revision = MAX(current_revision, excluded.current_revision),
                     stability        = 'final',
                     user_edited      = MAX(user_edited, excluded.user_edited),
                     meeting_start_ms = excluded.meeting_start_ms
                 WHERE excluded.current_revision >= transcript_segments.current_revision",
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
            tx.execute(
                "UPDATE transcript_segment_revisions SET speaker_id = ?4
                 WHERE meeting_id = ?1 AND segment_id = ?2 AND revision = ?3",
                params![meeting, *segment_id as i64, *revision as i64, speaker_id],
            )?;
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
            tx.execute(
                "UPDATE notes SET text = ?3 WHERE meeting_id = ?1 AND id = ?2",
                params![meeting, *note_id as i64, text],
            )?;
        }
        DomainEvent::NoteRemoved { note_id } => {
            // 標記而不是刪除：已匯出的文件可能引用它，實體刪除會讓引用指向虛空
            tx.execute(
                "UPDATE notes SET removed = 1 WHERE meeting_id = ?1 AND id = ?2",
                params![meeting, *note_id as i64],
            )?;
        }

        DomainEvent::SpeakerProposed {
            speaker_id,
            ordinal,
            proposed_name,
            provider_labels,
        } => {
            tx.execute(
                "INSERT INTO speakers (meeting_id, id, ordinal, proposed_name, status,
                                       provider_labels)
                 VALUES (?1,?2,?3,?4,?5,?6)
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
                    serde_json::to_string(provider_labels)?
                ],
            )?;
        }
        DomainEvent::SpeakerConfirmed { speaker_id, name }
        | DomainEvent::SpeakerRenamed { speaker_id, name } => {
            tx.execute(
                "UPDATE speakers SET confirmed_name = ?3, status = 'confirmed'
                 WHERE meeting_id = ?1 AND id = ?2",
                params![meeting, speaker_id, name],
            )?;
        }
        DomainEvent::SpeakerMerged {
            from_speaker_id,
            into_speaker_id,
        } => {
            tx.execute(
                "UPDATE speakers SET status = 'merged', merged_into = ?3
                 WHERE meeting_id = ?1 AND id = ?2",
                params![meeting, from_speaker_id, into_speaker_id],
            )?;
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
                "INSERT INTO speakers (meeting_id, id, ordinal, status)
                 VALUES (?1,?2,?3,'unconfirmed')
                 ON CONFLICT (meeting_id, id) DO NOTHING",
                params![meeting, new_speaker_id, *ordinal as i64],
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
            tx.execute(
                "UPDATE attachments SET status = 'extracted' WHERE id = ?1",
                params![attachment_id],
            )?;
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
            tx.execute(
                "UPDATE attachments SET status = 'removed' WHERE id = ?1",
                params![attachment_id],
            )?;
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
            tx.execute(
                "UPDATE generation_runs SET status = 'completed', usage = ?2 WHERE id = ?1",
                params![run_id, serde_json::to_string(usage)?],
            )?;
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
            tx.execute(
                "UPDATE generation_runs SET status = 'failed', failure_reason = ?2 WHERE id = ?1",
                params![run_id, reason],
            )?;
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
}
