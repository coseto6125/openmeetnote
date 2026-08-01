//! 會議工作階段：命令進、批次事件出。
//!
//! 對應 BLUEPRINT.md §5.1、§5.4.1 與 §7：
//! - 呼叫端只有命令與事件兩個概念，不直接碰 Audio、STT 或儲存。
//! - partial 逐字稿只出現在送往 UI 的批次裡，不取得 seq，也不進入事件序列。
//! - 決定性事件才配發 seq，快照以 `through_event_seq` 凍結涵蓋範圍。
//!
//! 三個結構性決定值得先說明：
//!
//! 1. **時鐘在 `SessionHandle`，不在 `Session`。** 任何會改變狀態的操作都先 `settle`
//!    到當下時刻，狀態轉換因此發生在精確的時間點，而不是「下一個批次窗看到的狀態」。
//!    `Session` 只接受「已經過多少毫秒」，所以邏輯完全可測。
//!
//! 2. **逐字稿來源是 `TranscriptSource` trait。** Fixture 與未來的真實 STT Adapter
//!    實作同一個 seam，接上平台 Capture 時替換的是 source，不是 session pump。
//!
//! 3. **命令邏輯是 `Session` 的方法，`#[tauri::command]` 只是薄包裝。**
//!    測試因此呼叫真正的命令路徑，而不是複製一份實作。

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::model::{Timeline, Track};
use crate::store::{DomainEvent, MeetingId, SegmentRevision, Store, StoreHandle};

pub const EVENT_CHANNEL: &str = "session://events";

/// UI 事件批次的節流窗。逐 frame 或逐字呼叫 WebView 會讓渲染成為熱點（§12）。
const BATCH_MS: u64 = 100;

// 這三個型別由 Store 與 Document 共用，定義在 model 以免兩邊各自演化。
pub use crate::model::{Health, MeetingState, Origin};

#[derive(Debug, Clone, Copy)]
struct SegmentState {
    revision: u32,
    origin: Origin,
    finalized: bool,
}

/// 事件分兩類：帶 `seq` 的決定性事件會進入事件序列，
/// 不帶 `seq` 的只是本批次的暫態狀態，重開會議後不存在。
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SessionEvent {
    /// 暫態：同一個 `segmentId` 會被反覆改寫，UI 就地更新，不新增節點。
    TranscriptPartial {
        segment_id: u64,
        speaker_id: String,
        text: String,
        meeting_time_ms: u64,
    },
    TranscriptFinalized {
        seq: u64,
        segment_id: u64,
        revision: u32,
        origin: Origin,
        speaker_id: String,
        text: String,
        meeting_time_ms: u64,
        captured_audio_ms: u64,
    },
    TranscriptEdited {
        seq: u64,
        segment_id: u64,
        revision: u32,
        origin: Origin,
        text: String,
    },
    NoteAdded {
        seq: u64,
        note_id: u64,
        text: String,
        meeting_time_ms: u64,
        captured_audio_ms: u64,
    },
    /// 第一次聽到某位語者。帶軌道，UI 才能依 §8.1 的軌道先驗給預設稱呼。
    SpeakerProposed {
        seq: u64,
        speaker_id: String,
        ordinal: u32,
        proposed_name: Option<String>,
        track: Track,
    },
    SpeakerConfirmed {
        seq: u64,
        speaker_id: String,
        name: String,
    },
    SnapshotCreated {
        seq: u64,
        version: u32,
        through_event_seq: u64,
        meeting_time_ms: u64,
    },
    GenerationCompleted {
        seq: u64,
        version: u32,
    },
    GenerationFailed {
        seq: u64,
        version: u32,
        reason: String,
    },
    /// 狀態轉換帶當下的兩條時間軸，UI 不必從批次邊界回推轉換時刻。
    MeetingStateChanged {
        seq: u64,
        state: MeetingState,
        meeting_time_ms: u64,
        captured_audio_ms: u64,
    },
    /// 暫態：子系統健康與會議生命週期正交（§6），降級不改變 Recording，
    /// 而且暫停期間同樣要送，否則「正交」只是說法。
    TrackActivity {
        mic_level: f32,
        system_level: f32,
        mic_health: Health,
        system_health: Health,
        stt_health: Health,
    },
}

impl SessionEvent {
    fn seq(&self) -> Option<u64> {
        match self {
            SessionEvent::TranscriptFinalized { seq, .. }
            | SessionEvent::TranscriptEdited { seq, .. }
            | SessionEvent::NoteAdded { seq, .. }
            | SessionEvent::SpeakerProposed { seq, .. }
            | SessionEvent::SpeakerConfirmed { seq, .. }
            | SessionEvent::SnapshotCreated { seq, .. }
            | SessionEvent::GenerationCompleted { seq, .. }
            | SessionEvent::GenerationFailed { seq, .. }
            | SessionEvent::MeetingStateChanged { seq, .. } => Some(*seq),
            SessionEvent::TranscriptPartial { .. } | SessionEvent::TrackActivity { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEventBatch {
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    /// 送出這批之前，事件序列已配發到的最大 seq。
    /// UI 用它偵測缺號：`prev_high + 1 != first_seq` 就是漏了東西。
    pub prev_high_seq: u64,
    pub emitted_at_ms: u64,
    pub meeting_time_ms: u64,
    pub captured_audio_ms: u64,
    pub state: MeetingState,
    /// 本機資料庫寫入失敗的原因。
    ///
    /// 這個欄位存在的理由：命令產生的事件會先送到 UI，寫入才發生。中間失敗
    /// 時畫面上已經有那筆內容了，只靠「下一個命令被拒絕」通知使用者，等於
    /// 讓他先相信一段時間。因此每個批次都帶著它，畫面一有機會就說實話。
    pub journal_error: Option<String>,
    pub events: Vec<SessionEvent>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandReceipt {
    pub accepted: bool,
    pub seq: Option<u64>,
    pub note: Option<String>,
}

impl CommandReceipt {
    fn ok(seq: u64) -> Self {
        Self {
            accepted: true,
            seq: Some(seq),
            note: None,
        }
    }
    fn rejected(note: &str) -> Self {
        Self {
            accepted: false,
            seq: None,
            note: Some(note.to_owned()),
        }
    }
}

/// UI 重新同步用的完整投影。事件缺號或 WebView 重載之後，
/// 前端用這個重建狀態，而不是帶著破洞繼續套用增量。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionProjection {
    pub state: MeetingState,
    pub seq: u64,
    pub meeting_time_ms: u64,
    pub captured_audio_ms: u64,
    pub segments: Vec<ProjectedSegment>,
    pub speakers: Vec<ProjectedSpeaker>,
    pub notes: Vec<ProjectedNote>,
    pub snapshots: Vec<ProjectedSnapshot>,
    pub pauses: Vec<ProjectedPause>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedSegment {
    pub segment_id: u64,
    pub revision: u32,
    pub origin: Origin,
    pub speaker_id: String,
    pub text: String,
    pub meeting_time_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedSpeaker {
    pub speaker_id: String,
    pub ordinal: u32,
    pub proposed_name: Option<String>,
    pub confirmed_name: Option<String>,
    pub track: Track,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedNote {
    pub note_id: u64,
    pub text: String,
    pub meeting_time_ms: u64,
    pub captured_audio_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedSnapshot {
    pub version: u32,
    pub through_event_seq: u64,
    pub meeting_time_ms: u64,
    pub state: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedPause {
    pub from_ms: u64,
    pub to_ms: Option<u64>,
}

/* ── 逐字稿來源的接縫 ─────────────────────────────────────────────── */

/// 平台 STT Adapter 與 Fixture 共用的輸入形狀。
#[derive(Debug, Clone)]
pub enum TranscriptInput {
    Partial {
        segment_id: u64,
        speaker_id: String,
        text: String,
        track: Track,
    },
    Final {
        segment_id: u64,
        speaker_id: String,
        text: String,
        track: Track,
    },
}

/// `MeetingSession` 只認識這個 trait。換成真實 STT 時，
/// 替換的是實作，不是 session 的事件管線。
pub trait TranscriptSource: Send {
    /// 推進 `window_ms` 毫秒，回傳這段時間內產生的逐字稿輸入。
    fn poll(&mut self, window_ms: u64) -> Vec<TranscriptInput>;
}

/// partial 每個批次窗吐出的字元數。1 字 / 100ms 大致是一般說話速度。
const CHARS_PER_TICK: usize = 1;
/// 兩句之間的靜默窗數，約兩秒。
const IDLE_TICKS_BETWEEN_UTTERANCES: u32 = 20;

/// Fixture 腳本。真實 STT Adapter 之後產生同樣形狀的結果。
const SCRIPT: &[(&str, &str)] = &[
    ("s1", "首頁我們希望案例可以往上拉，現在要滑三屏才看得到。"),
    ("me", "了解。那案例頁本身要不要一起改版？"),
    ("s1", "要，但範圍我們想分開報價，設計、開發、維運各自拆開。"),
    (
        "s2",
        "維運的部分可以先給一個月費的區間嗎，我們內部要走預算。",
    ),
    (
        "me",
        "可以，我列一份維運選配，含服務時段跟回應時間。SLA 我們下次再談細節。",
    ),
    ("s1", "另外 CMS 我們現在用的是舊版，這次想一起換掉。"),
    ("s2", "搬遷的內容量大概是四百多頁，這部分責任要寫清楚。"),
    ("me", "那我在範圍書裡把內容搬遷單獨列一條，標明由誰負責。"),
    ("s1", "效能的部分有沒有一個驗收標準？"),
    ("me", "通常會用實際載入時間當門檻，這個可以寫進驗收條件。"),
    ("s2", "下次麻煩直接帶頁面清單來對，不要只有口頭描述。"),
];

pub struct FixtureSource {
    next_segment_id: u64,
    script_idx: usize,
    idle_ticks: u32,
    live: Option<(u64, &'static str, Vec<char>, usize)>,
}

impl Default for FixtureSource {
    fn default() -> Self {
        Self {
            next_segment_id: 1000,
            script_idx: 0,
            idle_ticks: 0,
            live: None,
        }
    }
}

impl TranscriptSource for FixtureSource {
    fn poll(&mut self, window_ms: u64) -> Vec<TranscriptInput> {
        // 一個窗可能涵蓋多個 100ms 步進（系統落後時），逐步補上。
        let steps = (window_ms / BATCH_MS).clamp(1, 8);
        let mut out = Vec::new();

        for _ in 0..steps {
            match self.live.take() {
                Some((segment_id, speaker_id, full, shown)) => {
                    let shown = (shown + CHARS_PER_TICK).min(full.len());
                    let text: String = full[..shown].iter().collect();
                    // 軌道先驗只到「本機 vs 遠端」，不再往「遠端只有一人」推（§8.1）
                    let track = if speaker_id == "me" {
                        Track::Mic
                    } else {
                        Track::System
                    };
                    if shown >= full.len() {
                        out.push(TranscriptInput::Final {
                            segment_id,
                            speaker_id: speaker_id.to_owned(),
                            text,
                            track,
                        });
                    } else {
                        out.push(TranscriptInput::Partial {
                            segment_id,
                            speaker_id: speaker_id.to_owned(),
                            text,
                            track,
                        });
                        self.live = Some((segment_id, speaker_id, full, shown));
                    }
                }
                None => {
                    // 腳本播完就靜默。循環重播會讓同一段對話重複出現，
                    // 那是真實 STT 不會有的行為，會誤導在看 demo 的人。
                    if self.script_idx >= SCRIPT.len() {
                        break;
                    }
                    self.idle_ticks += 1;
                    if self.idle_ticks >= IDLE_TICKS_BETWEEN_UTTERANCES {
                        self.idle_ticks = 0;
                        let (speaker_id, text) = SCRIPT[self.script_idx];
                        self.script_idx += 1;
                        self.next_segment_id += 1;
                        self.live =
                            Some((self.next_segment_id, speaker_id, text.chars().collect(), 0));
                    }
                }
            }
        }
        out
    }
}

/* ── 會議狀態 ─────────────────────────────────────────────────────── */

struct NoteRecord {
    note_id: u64,
    text: String,
    meeting_time_ms: u64,
    captured_audio_ms: u64,
}

struct SnapshotRecord {
    version: u32,
    through_event_seq: u64,
    meeting_time_ms: u64,
    state: &'static str,
}

#[derive(Debug, Clone)]
struct SpeakerRecord {
    ordinal: u32,
    proposed_name: Option<String>,
    confirmed_name: Option<String>,
    track: Track,
}

struct SegmentRecord {
    speaker_id: String,
    text: String,
    meeting_time_ms: u64,
    track: Track,
    /// 第一次看到這個片段時的兩條時間軸。
    ///
    /// 真實 STT Adapter 會直接給出片段的音訊起訖，屆時改用它給的值。
    /// 在那之前，用「第一個 partial 出現的時刻」到「定稿時刻」當作區間，
    /// 這是本機唯一能觀測到的邊界，也足以支撐 §5.3.2 的區間去重。
    first_seen: Timeline,
    state: SegmentState,
}

pub struct Session {
    /// 這場會議在 SQLite 的 id。`start` 之前是 None，因為還沒有會議。
    meeting_id: Option<MeetingId>,
    state: MeetingState,
    seq: u64,
    meeting_time_ms: u64,
    captured_audio_ms: u64,
    /// 上次結算的時刻，對應 `SessionHandle` 的 epoch 經過毫秒數。
    last_settled_ms: u64,
    next_note_id: u64,
    segments: HashMap<u64, SegmentRecord>,
    /// 正規化音訊區間到片段的對應（§5.3.2）。
    ///
    /// 重連與輪替之後 Provider 的識別碼全都會變，唯一不變的是音訊區間，
    /// 因此去重鍵是它，不是 Provider 給的 segment id。
    intervals: HashMap<(Track, u64, u64), u64>,
    /// 已經提出過的語者與它們的出現序。
    ///
    /// 語者必須先被 `SpeakerProposed` 才能被確認：投影裡沒有那一列的話，
    /// 確認會更新 0 列而畫面照樣顯示成功。這個對照表就是那條前提。
    speakers: HashMap<String, SpeakerRecord>,
    notes: Vec<NoteRecord>,
    snapshots: Vec<SnapshotRecord>,
    pauses: Vec<ProjectedPause>,
    pending: Vec<SessionEvent>,
    /// 等待寫入 SQLite 的決定性事件，與 `pending` 的 seq 一一對應。
    journal: Vec<(DomainEvent, Timeline)>,
    /// 日誌寫入失敗。發生後拒絕所有會產生新事件的命令：
    /// 繼續接受命令等於讓使用者以為內容有被保存。
    journal_error: Option<String>,
    next_document_id: i64,
    next_run_id: i64,
    document_id: Option<i64>,
    /// 版本號到 run_id 的對應。UI 用版本號，日誌用 run_id。
    run_ids: HashMap<u32, i64>,
    source: Box<dyn TranscriptSource>,
    mic_health: Health,
    stt_health: Health,
    rng: u32,
}

impl Default for Session {
    fn default() -> Self {
        Self::with_source(Box::new(FixtureSource::default()))
    }
}

impl Session {
    pub fn with_source(source: Box<dyn TranscriptSource>) -> Self {
        Self {
            meeting_id: None,
            state: MeetingState::Idle,
            seq: 0,
            meeting_time_ms: 0,
            captured_audio_ms: 0,
            last_settled_ms: 0,
            next_note_id: 1,
            segments: HashMap::new(),
            intervals: HashMap::new(),
            speakers: HashMap::new(),
            notes: Vec::new(),
            snapshots: Vec::new(),
            pauses: Vec::new(),
            pending: Vec::new(),
            journal: Vec::new(),
            journal_error: None,
            next_document_id: 1,
            next_run_id: 1,
            document_id: None,
            run_ids: HashMap::new(),
            source,
            mic_health: Health::Ok,
            stt_health: Health::Ok,
            rng: 0x2545_f491,
        }
    }

    /// 決定性事件的唯一出口。
    ///
    /// seq 只在這裡配發，而配發的同時一定產生兩份表述：送 UI 的 `SessionEvent`
    /// 與寫日誌的 `DomainEvent`。兩者形狀不同（UI 不需要音訊區間，日誌不需要
    /// 音量），但共用同一個 seq。只留一個入口，是為了讓「有事件卻沒進日誌」
    /// 這種漏洞無處可生。
    fn record(&mut self, domain: DomainEvent, ui: impl FnOnce(u64) -> SessionEvent) -> u64 {
        self.seq += 1;
        let seq = self.seq;
        let ev = ui(seq);
        debug_assert_eq!(ev.seq(), Some(seq), "決定性事件必須帶上配發到的 seq");
        self.pending.push(ev);
        self.journal.push((
            domain,
            Timeline::new(self.meeting_time_ms, self.captured_audio_ms),
        ));
        seq
    }

    /// 取走待寫入的日誌。呼叫端負責寫進 Store，失敗時用 `journal_failed` 回報。
    fn take_journal(&mut self) -> Vec<(DomainEvent, Timeline)> {
        std::mem::take(&mut self.journal)
    }

    fn journal_failed(&mut self, reason: String) {
        self.journal_error.get_or_insert(reason);
    }

    /// 日誌寫不進去時，任何會產生新事件的命令都必須拒絕。
    fn journal_guard(&self) -> Option<CommandReceipt> {
        self.journal_error
            .as_ref()
            .map(|e| CommandReceipt::rejected(&format!("內容無法寫入本機資料庫：{e}")))
    }

    /// xorshift：只用來讓 Fixture 的音量看起來像真的，不需要密碼學品質。
    fn rand_unit(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x % 1000) as f32 / 1000.0
    }

    fn push(&mut self, ev: SessionEvent) {
        self.pending.push(ev);
    }

    /// 把時間推進到 `elapsed_ms`，依「目前」狀態分配這段時間。
    ///
    /// 任何改變狀態的操作都必須先呼叫這個，否則轉換前後的時間會被算到錯的一邊：
    /// 例如 tick 之後 90 ms 才暫停，那 90 ms 屬於 Recording，
    /// 若等下一個窗才發現已是 Paused，整個 100 ms 都會被排除在錄音時間之外。
    fn settle(&mut self, elapsed_ms: u64) {
        let delta = elapsed_ms.saturating_sub(self.last_settled_ms);
        self.last_settled_ms = elapsed_ms;
        if delta == 0 {
            return;
        }
        // meeting_time 含暫停與靜音，captured_audio 只在實際錄音時前進（§11）。
        match self.state {
            MeetingState::Recording => {
                self.meeting_time_ms += delta;
                self.captured_audio_ms += delta;
            }
            MeetingState::Paused | MeetingState::Stopping | MeetingState::Finalizing => {
                self.meeting_time_ms += delta;
            }
            MeetingState::Idle | MeetingState::Completed => {}
        }
    }

    fn transition(&mut self, state: MeetingState) -> u64 {
        match (self.state, state) {
            (_, MeetingState::Paused) => self.pauses.push(ProjectedPause {
                from_ms: self.meeting_time_ms,
                to_ms: None,
            }),
            // 離開 Paused 就封口，包含直接停止的路徑，
            // 否則暫停區間會永遠開著。
            (MeetingState::Paused, _) => {
                if let Some(last) = self.pauses.last_mut() {
                    if last.to_ms.is_none() {
                        last.to_ms = Some(self.meeting_time_ms);
                    }
                }
            }
            _ => {}
        }

        self.state = state;
        let (meeting_time_ms, captured_audio_ms) = (self.meeting_time_ms, self.captured_audio_ms);
        self.record(DomainEvent::MeetingStateChanged { state }, |seq| {
            SessionEvent::MeetingStateChanged {
                seq,
                state,
                meeting_time_ms,
                captured_audio_ms,
            }
        })
    }

    /// 一個批次窗的推進：結算時間、回報健康、拉取逐字稿。
    fn tick(&mut self, elapsed_ms: u64) {
        let window = elapsed_ms.saturating_sub(self.last_settled_ms);
        self.settle(elapsed_ms);

        if matches!(self.state, MeetingState::Idle | MeetingState::Completed) {
            return;
        }

        // 健康狀態在 Recording 與 Paused 都要可觀察，這是「與生命週期正交」的實際含意。
        let recording = self.state == MeetingState::Recording;
        let mic_on = recording && self.mic_health == Health::Ok;
        let mic_level = if mic_on {
            0.15 + self.rand_unit() * 0.7
        } else {
            0.0
        };
        let system_level = if recording {
            0.2 + self.rand_unit() * 0.7
        } else {
            0.0
        };
        self.push(SessionEvent::TrackActivity {
            mic_level,
            system_level,
            mic_health: self.mic_health,
            system_health: Health::Ok,
            stt_health: self.stt_health,
        });

        if !recording || self.stt_health != Health::Ok {
            return;
        }

        for input in self.source.poll(window.max(BATCH_MS)) {
            self.apply_transcript_input(input);
        }
    }

    /// 第一次聽到某位語者時把它記進日誌。
    ///
    /// 在 partial 就提出而不是等定稿：語者存在與否跟那句話會不會定稿無關，
    /// 而且 UI 在 partial 階段就要顯示是誰在講。partial 本身仍然不進日誌。
    ///
    /// 暫定名稱目前一律為 None。§8.3 的自我介紹推定是 M3 的工作，
    /// 在那之前不憑軌道或發言順序猜任何人的真實身份。
    fn ensure_speaker(&mut self, speaker_id: &str, track: Track) {
        if self.speakers.contains_key(speaker_id) {
            return;
        }
        let ordinal = self.speakers.len() as u32 + 1;
        self.speakers.insert(
            speaker_id.to_owned(),
            SpeakerRecord {
                ordinal,
                proposed_name: None,
                confirmed_name: None,
                track,
            },
        );
        let id = speaker_id.to_owned();
        self.record(
            DomainEvent::SpeakerProposed {
                speaker_id: id.clone(),
                ordinal,
                proposed_name: None,
                provider_labels: vec![id.clone()],
            },
            |seq| SessionEvent::SpeakerProposed {
                seq,
                speaker_id: id,
                ordinal,
                proposed_name: None,
                track,
            },
        );
    }

    /// 來源送進來的逐字稿要通過同一組優先權規則，
    /// Fixture 與真實 STT 都不例外。
    fn apply_transcript_input(&mut self, input: TranscriptInput) {
        match &input {
            TranscriptInput::Partial {
                speaker_id, track, ..
            }
            | TranscriptInput::Final {
                speaker_id, track, ..
            } => self.ensure_speaker(speaker_id, *track),
        }
        match input {
            TranscriptInput::Partial {
                segment_id,
                speaker_id,
                text,
                track,
            } => {
                // 已定稿的片段不接受 partial。真實 STT 的 late partial
                // 會在 reconnect 之後出現，讓它改寫 final 等於資料倒退。
                if let Some(rec) = self.segments.get(&segment_id) {
                    if rec.state.finalized {
                        return;
                    }
                }
                let meeting_time_ms = self.meeting_time_ms;
                // partial 不進日誌（§5.4.1），但要記下區間起點：
                // 定稿時才知道結束時刻，開始時刻只有現在能觀測到。
                let first_seen = Timeline::new(meeting_time_ms, self.captured_audio_ms);
                self.segments
                    .entry(segment_id)
                    .or_insert_with(|| SegmentRecord {
                        speaker_id: speaker_id.clone(),
                        text: String::new(),
                        meeting_time_ms,
                        track,
                        first_seen,
                        state: SegmentState {
                            revision: 0,
                            origin: Origin::Provider,
                            finalized: false,
                        },
                    })
                    .text = text.clone();
                self.push(SessionEvent::TranscriptPartial {
                    segment_id,
                    speaker_id,
                    text,
                    meeting_time_ms,
                });
            }
            TranscriptInput::Final {
                segment_id,
                speaker_id,
                text,
                track,
            } => {
                if let Some(rec) = self.segments.get(&segment_id) {
                    // 使用者修訂優先於 Provider（§5.3）。
                    if rec.state.origin == Origin::User {
                        return;
                    }
                    // 同一個片段的重複 final 不再配發新 seq。
                    if rec.state.finalized {
                        return;
                    }
                }
                let (meeting_time_ms, captured_audio_ms) =
                    (self.meeting_time_ms, self.captured_audio_ms);
                let first_seen = self
                    .segments
                    .get(&segment_id)
                    .map(|r| r.first_seen)
                    .unwrap_or(Timeline::new(meeting_time_ms, captured_audio_ms));

                // 跨串流去重（§5.3.2）。同一段音訊被重播時，Provider 會給
                // 一個全新的 segment id，唯一能認出它的是音訊區間。
                let key = crate::store::interval_key(
                    track,
                    first_seen.captured_audio_ms,
                    captured_audio_ms,
                );
                match self.intervals.get(&key).copied() {
                    Some(existing) if existing != segment_id => {
                        let same_text = self.segments.get(&existing).is_some_and(|r| {
                            crate::store::normalize_text(&r.text)
                                == crate::store::normalize_text(&text)
                        });
                        // 同區間同內容就是重播，捨棄；同區間不同內容是
                        // Provider 改了結果，那是新 revision 不是新片段
                        if same_text {
                            return;
                        }
                        self.revise_segment(existing, &text, track);
                        return;
                    }
                    _ => {
                        self.intervals.insert(key, segment_id);
                    }
                }
                self.segments.insert(
                    segment_id,
                    SegmentRecord {
                        speaker_id: speaker_id.clone(),
                        text: text.clone(),
                        meeting_time_ms: first_seen.meeting_time_ms,
                        track,
                        first_seen,
                        state: SegmentState {
                            revision: 1,
                            origin: Origin::Provider,
                            finalized: true,
                        },
                    },
                );
                let domain = DomainEvent::TranscriptSegmentFinalized {
                    segment: SegmentRevision {
                        segment_id,
                        revision: 1,
                        text: text.clone(),
                        speaker_id: Some(speaker_id.clone()),
                        track,
                        meeting_start_ms: first_seen.meeting_time_ms,
                        meeting_end_ms: meeting_time_ms,
                        captured_start_ms: first_seen.captured_audio_ms,
                        captured_end_ms: captured_audio_ms,
                        echo_likelihood: None,
                        overlap_group_id: None,
                        provider_stream_id: None,
                        provider_result_id: None,
                        rollover_generation: 0,
                        origin: Origin::Provider,
                        speaker_spans: Vec::new(),
                    },
                };
                let start_ms = first_seen.meeting_time_ms;
                self.record(domain, |seq| SessionEvent::TranscriptFinalized {
                    seq,
                    segment_id,
                    revision: 1,
                    origin: Origin::Provider,
                    speaker_id,
                    text,
                    meeting_time_ms: start_ms,
                    captured_audio_ms,
                });
            }
        }
    }

    /// Provider 對同一段音訊給出不同結果：建立新 revision，不建立新片段。
    fn revise_segment(&mut self, segment_id: u64, text: &str, track: Track) {
        let Some(rec) = self.segments.get_mut(&segment_id) else {
            return;
        };
        // 使用者修訂優先於 Provider（§5.3），這裡同樣不例外
        if rec.state.origin == Origin::User {
            return;
        }
        rec.state.revision += 1;
        rec.text = text.to_owned();
        let (revision, speaker_id, first_seen, start_ms) = (
            rec.state.revision,
            rec.speaker_id.clone(),
            rec.first_seen,
            rec.meeting_time_ms,
        );
        let captured_end = self.captured_audio_ms;
        let domain = DomainEvent::TranscriptSegmentRevised {
            segment: SegmentRevision {
                segment_id,
                revision,
                text: text.to_owned(),
                speaker_id: Some(speaker_id.clone()),
                track,
                meeting_start_ms: first_seen.meeting_time_ms,
                meeting_end_ms: self.meeting_time_ms,
                captured_start_ms: first_seen.captured_audio_ms,
                captured_end_ms: captured_end,
                echo_likelihood: None,
                overlap_group_id: None,
                provider_stream_id: None,
                provider_result_id: None,
                rollover_generation: 0,
                origin: Origin::Provider,
                speaker_spans: Vec::new(),
            },
        };
        let owned = text.to_owned();
        self.record(domain, |seq| SessionEvent::TranscriptFinalized {
            seq,
            segment_id,
            revision,
            origin: Origin::Provider,
            speaker_id,
            text: owned,
            meeting_time_ms: start_ms,
            captured_audio_ms: captured_end,
        });
    }

    fn drain(&mut self, prev_high_seq: u64) -> SessionEventBatch {
        let events = std::mem::take(&mut self.pending);
        let seqs: Vec<u64> = events.iter().filter_map(|e| e.seq()).collect();
        SessionEventBatch {
            first_seq: seqs.first().copied(),
            last_seq: seqs.last().copied(),
            prev_high_seq,
            emitted_at_ms: now_ms(),
            meeting_time_ms: self.meeting_time_ms,
            captured_audio_ms: self.captured_audio_ms,
            state: self.state,
            journal_error: self.journal_error.clone(),
            events,
        }
    }

    fn projection(&self) -> SessionProjection {
        let mut segments: Vec<ProjectedSegment> = self
            .segments
            .iter()
            .map(|(id, rec)| ProjectedSegment {
                segment_id: *id,
                revision: rec.state.revision,
                origin: rec.state.origin,
                speaker_id: rec.speaker_id.clone(),
                text: rec.text.clone(),
                meeting_time_ms: rec.meeting_time_ms,
            })
            .collect();
        segments.sort_by_key(|s| (s.meeting_time_ms, s.segment_id));

        SessionProjection {
            state: self.state,
            seq: self.seq,
            meeting_time_ms: self.meeting_time_ms,
            captured_audio_ms: self.captured_audio_ms,
            segments,
            speakers: {
                let mut v: Vec<_> = self
                    .speakers
                    .iter()
                    .map(|(id, r)| ProjectedSpeaker {
                        speaker_id: id.clone(),
                        ordinal: r.ordinal,
                        proposed_name: r.proposed_name.clone(),
                        confirmed_name: r.confirmed_name.clone(),
                        track: r.track,
                    })
                    .collect();
                v.sort_by_key(|s| s.ordinal);
                v
            },
            notes: self
                .notes
                .iter()
                .map(|n| ProjectedNote {
                    note_id: n.note_id,
                    text: n.text.clone(),
                    meeting_time_ms: n.meeting_time_ms,
                    captured_audio_ms: n.captured_audio_ms,
                })
                .collect(),
            snapshots: self
                .snapshots
                .iter()
                .map(|s| ProjectedSnapshot {
                    version: s.version,
                    through_event_seq: s.through_event_seq,
                    meeting_time_ms: s.meeting_time_ms,
                    state: s.state,
                })
                .collect(),
            pauses: self.pauses.clone(),
        }
    }

    /* ── 命令邏輯。#[tauri::command] 只是薄包裝，測試走這裡。 ────────── */

    /// 開始錄音。`meeting` 是呼叫端事先在 Store 建立好的會議。
    ///
    /// 會議列在 `start` 才建立，不在啟動時建立：每次開 app 就多一列空會議，
    /// 歷史頁會被沒發生過的會議塞滿。
    pub fn start(&mut self, meeting: MeetingId, seeds: crate::store::IdSeeds) -> CommandReceipt {
        match self.state {
            MeetingState::Idle => {
                self.meeting_id = Some(meeting);
                self.next_document_id = seeds.document_id + 1;
                self.next_run_id = seeds.run_id + 1;
                CommandReceipt::ok(self.transition(MeetingState::Recording))
            }
            // 結束後要開新會議必須是全新的 Session，不能沿用舊 seq、片段與快照。
            MeetingState::Completed => CommandReceipt::rejected("會議已結束，請建立新會議"),
            _ => CommandReceipt::rejected("會議已在進行中"),
        }
    }

    pub fn pause(&mut self) -> CommandReceipt {
        match self.state {
            MeetingState::Recording => CommandReceipt::ok(self.transition(MeetingState::Paused)),
            _ => CommandReceipt::rejected("目前不在錄音中"),
        }
    }

    pub fn resume(&mut self) -> CommandReceipt {
        match self.state {
            MeetingState::Paused => CommandReceipt::ok(self.transition(MeetingState::Recording)),
            _ => CommandReceipt::rejected("目前不在暫停狀態"),
        }
    }

    pub fn add_note(&mut self, text: &str) -> CommandReceipt {
        let text = text.trim();
        if text.is_empty() {
            return CommandReceipt::rejected("筆記內容為空");
        }
        if !self.state.accepts_document_work() {
            return CommandReceipt::rejected("目前的會議狀態不接受筆記");
        }
        if let Some(r) = self.journal_guard() {
            return r;
        }
        let note_id = self.next_note_id;
        self.next_note_id += 1;
        let (meeting_time_ms, captured_audio_ms) = (self.meeting_time_ms, self.captured_audio_ms);
        self.notes.push(NoteRecord {
            note_id,
            text: text.to_owned(),
            meeting_time_ms,
            captured_audio_ms,
        });
        let owned = text.to_owned();
        let seq = self.record(
            DomainEvent::NoteAdded {
                note_id,
                text: owned.clone(),
            },
            |seq| SessionEvent::NoteAdded {
                seq,
                note_id,
                text: owned,
                meeting_time_ms,
                captured_audio_ms,
            },
        );
        CommandReceipt::ok(seq)
    }

    pub fn confirm_speaker(&mut self, speaker_id: &str, name: &str) -> CommandReceipt {
        if !self.state.accepts_document_work() {
            return CommandReceipt::rejected("目前的會議狀態不接受語者確認");
        }
        if name.trim().is_empty() {
            return CommandReceipt::rejected("語者名稱為空");
        }
        // 沒被提出過的語者不能確認：投影裡沒有那一列，確認會無聲消失
        if !self.speakers.contains_key(speaker_id) {
            return CommandReceipt::rejected("尚未聽到這位語者發言");
        }
        if let Some(r) = self.journal_guard() {
            return r;
        }
        let (id, name) = (speaker_id.to_owned(), name.trim().to_owned());
        if let Some(rec) = self.speakers.get_mut(&id) {
            rec.confirmed_name = Some(name.clone());
        }
        let seq = self.record(
            DomainEvent::SpeakerConfirmed {
                speaker_id: id.clone(),
                name: name.clone(),
            },
            |seq| SessionEvent::SpeakerConfirmed {
                seq,
                speaker_id: id,
                name,
            },
        );
        CommandReceipt::ok(seq)
    }

    /// 使用者修訂。建立新的 revision 而不是就地改寫，
    /// 之後 Provider 的結果不得覆蓋它（§5.3）。
    pub fn edit_transcript(&mut self, segment_id: u64, text: &str) -> CommandReceipt {
        let text = text.trim();
        if text.is_empty() {
            return CommandReceipt::rejected("修訂內容為空");
        }
        if !self.state.accepts_document_work() {
            return CommandReceipt::rejected("目前的會議狀態不接受修訂");
        }
        if let Some(r) = self.journal_guard() {
            return r;
        }
        let Some(rec) = self.segments.get_mut(&segment_id) else {
            return CommandReceipt::rejected("找不到這個逐字稿片段");
        };
        rec.state.revision += 1;
        rec.state.origin = Origin::User;
        rec.text = text.to_owned();
        let revision = rec.state.revision;
        // 使用者修訂建立新 revision，內容換掉但區間沿用：改的是文字不是音訊
        let domain = DomainEvent::TranscriptSegmentEdited {
            segment: SegmentRevision {
                segment_id,
                revision,
                text: text.to_owned(),
                speaker_id: Some(rec.speaker_id.clone()),
                track: rec.track,
                meeting_start_ms: rec.first_seen.meeting_time_ms,
                meeting_end_ms: rec.meeting_time_ms.max(rec.first_seen.meeting_time_ms),
                captured_start_ms: rec.first_seen.captured_audio_ms,
                captured_end_ms: rec.first_seen.captured_audio_ms,
                echo_likelihood: None,
                overlap_group_id: None,
                provider_stream_id: None,
                provider_result_id: None,
                rollover_generation: 0,
                origin: Origin::User,
                speaker_spans: Vec::new(),
            },
        };
        let owned = text.to_owned();
        let seq = self.record(domain, |seq| SessionEvent::TranscriptEdited {
            seq,
            segment_id,
            revision,
            origin: Origin::User,
            text: owned,
        });
        CommandReceipt::ok(seq)
    }

    /// 快照凍結 `through_event_seq` 之後立即回應，生成在背景進行，錄音不中斷（§5.4.2）。
    /// 回傳收據與（版本號、快照游標）。游標必須帶出去，背景生成才知道
    /// 自己涵蓋到哪裡，而不是在執行當下重新問一次「現在到哪了」。
    pub fn create_snapshot(&mut self) -> (CommandReceipt, Option<(u32, u64)>) {
        if !self.state.accepts_document_work() {
            return (CommandReceipt::rejected("目前的會議狀態不接受快照"), None);
        }
        if let Some(r) = self.journal_guard() {
            return (r, None);
        }
        // 游標在配發本事件的 seq 之前凍結：快照不涵蓋建立快照這件事本身
        let through = self.seq;
        let version = self.snapshots.len() as u32 + 1;
        let meeting_time_ms = self.meeting_time_ms;
        self.snapshots.push(SnapshotRecord {
            version,
            through_event_seq: through,
            meeting_time_ms,
            state: "running",
        });
        let (document_id, run_id) = self.allocate_run(version);
        let seq = self.record(
            DomainEvent::SnapshotCreated {
                document_id,
                run_id,
                parent_run_id: None,
                version_no: version,
                purpose: "meeting-summary".into(),
                title: "會議摘要".into(),
                through_event_seq: through,
                prompt: String::new(),
            },
            |seq| SessionEvent::SnapshotCreated {
                seq,
                version,
                through_event_seq: through,
                meeting_time_ms,
            },
        );
        (CommandReceipt::ok(seq), Some((version, through)))
    }

    /// 一個文件加一輪生成的 id。事件必須自帶 id，重播才不會因插入順序而變。
    fn allocate_run(&mut self, version: u32) -> (i64, i64) {
        // 同一場會議目前只產生一份文件，版本鏈掛在它下面
        let document_id = self.document_id.get_or_insert_with(|| {
            let id = self.next_document_id;
            self.next_document_id += 1;
            id
        });
        let document_id = *document_id;
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        self.run_ids.insert(version, run_id);
        (document_id, run_id)
    }

    fn finish_generation(
        &mut self,
        version: u32,
        blocks: Vec<crate::store::DocumentBlock>,
        usage: serde_json::Value,
        failure: Option<String>,
    ) {
        if let Some(rec) = self.snapshots.iter_mut().find(|s| s.version == version) {
            rec.state = if failure.is_some() {
                "failed"
            } else {
                "completed"
            };
        }
        let Some(run_id) = self.run_ids.get(&version).copied() else {
            return;
        };
        match failure {
            None => {
                self.record(
                    DomainEvent::GenerationCompleted {
                        run_id,
                        blocks,
                        usage,
                    },
                    |seq| SessionEvent::GenerationCompleted { seq, version },
                );
            }
            Some(reason) => {
                let r = reason.clone();
                self.record(
                    DomainEvent::GenerationFailed {
                        run_id,
                        reason: reason.clone(),
                    },
                    |seq| SessionEvent::GenerationFailed {
                        seq,
                        version,
                        reason: r,
                    },
                );
            }
        }
    }

    /// Stop 走 §6 的 Stopping → Finalizing → Completed，
    /// 不把三個狀態壓成一次跳轉，背景工作才有結算的位置。
    pub fn stop(&mut self) -> CommandReceipt {
        if !self.state.accepts_document_work() {
            return CommandReceipt::rejected("會議尚未開始或已結束");
        }
        self.transition(MeetingState::Stopping);
        self.transition(MeetingState::Finalizing);
        CommandReceipt::ok(self.transition(MeetingState::Completed))
    }

    /// 這個 Session 目前寫入的會議。`flush` 用它決定事件要寫到哪裡。
    pub fn meeting_id(&self) -> Option<MeetingId> {
        self.meeting_id
    }

    /// 還在進行中的會議。
    ///
    /// 與 `meeting_id` 分開：結束之後 Session 仍然指著那場會議（投影還在
    /// 畫面上），但它不會再產生任何事件，因此可以安全刪除，狀態也不該
    /// 再顯示成進行中。
    pub fn active_meeting_id(&self) -> Option<MeetingId> {
        match self.state {
            MeetingState::Idle | MeetingState::Completed => None,
            _ => self.meeting_id,
        }
    }

    pub fn inject_fault(&mut self, kind: &str) -> CommandReceipt {
        match kind {
            "micLost" => {
                self.mic_health = Health::Lost;
                CommandReceipt::ok(self.seq)
            }
            "micRestored" => {
                self.mic_health = Health::Ok;
                CommandReceipt::ok(self.seq)
            }
            "sttDown" => {
                self.stt_health = Health::Degraded;
                CommandReceipt::ok(self.seq)
            }
            "sttUp" => {
                self.stt_health = Health::Ok;
                CommandReceipt::ok(self.seq)
            }
            "generationFailed" => {
                if !self.state.accepts_document_work() {
                    return CommandReceipt::rejected("目前的會議狀態不接受生成");
                }
                let version = self.snapshots.len() as u32 + 1;
                let through = self.seq;
                let meeting_time_ms = self.meeting_time_ms;
                self.snapshots.push(SnapshotRecord {
                    version,
                    through_event_seq: through,
                    meeting_time_ms,
                    state: "failed",
                });
                let (document_id, run_id) = self.allocate_run(version);
                self.record(
                    DomainEvent::SnapshotCreated {
                        document_id,
                        run_id,
                        parent_run_id: None,
                        version_no: version,
                        purpose: "meeting-summary".into(),
                        title: "會議摘要".into(),
                        through_event_seq: through,
                        prompt: String::new(),
                    },
                    |seq| SessionEvent::SnapshotCreated {
                        seq,
                        version,
                        through_event_seq: through,
                        meeting_time_ms,
                    },
                );
                self.finish_generation(
                    version,
                    Vec::new(),
                    serde_json::json!({}),
                    Some("Provider 回報限流".to_owned()),
                );
                CommandReceipt::ok(self.seq)
            }
            _ => CommandReceipt::rejected("未知的注入類型"),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/* ── Handle：持有時鐘，並在鎖損毀時 fail closed ───────────────────── */

pub struct SessionHandle {
    inner: Mutex<Session>,
    epoch: Instant,
}

impl Default for SessionHandle {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Session::default()),
            epoch: Instant::now(),
        }
    }
}

impl SessionHandle {
    fn elapsed_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// 取得鎖並先把時間結算到現在。
    ///
    /// 鎖損毀代表某次操作在持鎖時 panic，狀態可能停在半途。
    /// 這時繼續配發 seq 會把壞掉的會議偽裝成正常會議，因此拒絕而不是沿用。
    fn with<F, R>(&self, f: F) -> Result<R, PoisonedSession>
    where
        F: FnOnce(&mut Session) -> R,
    {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_: PoisonError<_>| PoisonedSession)?;
        let now = self.elapsed_ms();
        guard.settle(now);
        Ok(f(&mut guard))
    }
}

impl SessionHandle {
    /// 用既有的 Session 建一個把手，供測試直接驅動 `run_generation`。
    #[cfg(test)]
    fn from_session(session: Session) -> Self {
        Self {
            inner: Mutex::new(session),
            epoch: Instant::now(),
        }
    }

    /// 換上全新的 Session。結束後開新會議走這條路，
    /// 而不是重置欄位：漏掉一個欄位就會把上一場的片段帶進新會議。
    fn reset(&self) {
        if let Ok(mut g) = self.inner.lock() {
            *g = Session::default();
        }
    }
}

#[derive(Debug)]
pub struct PoisonedSession;

const POISONED_NOTE: &str = "工作階段狀態已損毀，請重新啟動應用程式";

/* ── 持久化 ───────────────────────────────────────────────────────── */

/// 把 Session 累積的決定性事件寫進 SQLite。
///
/// 全程只有這一個 append 呼叫點，事件的持久化順序因此等同 `record` 的順序。
/// 寫入失敗會標記 Session，之後所有產生新事件的命令都被拒絕：讓使用者
/// 以為內容有被保存，比直接說寫不進去糟得多。
pub fn flush(session: &SessionHandle, store: &StoreHandle) {
    let Ok(Some((meeting, batch))) =
        session.with(|s| s.meeting_id().map(|m| (m, s.take_journal())))
    else {
        return;
    };
    if batch.is_empty() {
        return;
    }
    let result = match store.exclusive() {
        Ok(mut st) => st.append(meeting, &batch).map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    if let Err(reason) = result {
        let _ = session.with(|s| s.journal_failed(reason));
    }
}

fn command<F>(state: &State<SessionHandle>, store: &State<StoreHandle>, f: F) -> CommandReceipt
where
    F: FnOnce(&mut Session) -> CommandReceipt,
{
    let receipt = state
        .with(f)
        .unwrap_or_else(|_| CommandReceipt::rejected(POISONED_NOTE));
    flush(state, store);
    receipt
}

/// 批次泵：唯一會 emit 事件的地方，因此 UI 不可能收到未經節流的事件流。
pub fn spawn_pump(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(BATCH_MS));
        // 落後時直接跳過漏掉的窗，不要事後補打。
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut prev_high_seq = 0u64;
        loop {
            ticker.tick().await;
            let handle: State<SessionHandle> = app.state();
            let now = handle.elapsed_ms();

            let batch = {
                let mut guard = match handle.inner.lock() {
                    Ok(g) => g,
                    // 狀態損毀時停止送事件。繼續送等於宣稱一切正常。
                    Err(_) => break,
                };
                guard.tick(now);
                // 沒有新事件時通常不送批次，但寫入失敗必須送出去：
                // 這個狀態下 pending 往往正好是空的，不送就沒人會知道。
                if guard.pending.is_empty() && guard.journal_error.is_none() {
                    continue;
                }
                guard.drain(prev_high_seq)
            };
            if let Some(last) = batch.last_seq {
                prev_high_seq = last;
            }
            // 先落地再送 UI：畫面上出現的內容必須已經寫進資料庫，
            // 反過來的話崩潰之後畫面看過的東西會消失。
            flush(&handle, &app.state::<StoreHandle>());
            // emit 在鎖外，避免 WebView 的序列化成本拖住命令處理。
            let _ = app.emit(EVENT_CHANNEL, batch);
        }
    });
}

/* ── 命令 ─────────────────────────────────────────────────────────── */

#[tauri::command]
pub fn start_meeting(state: State<SessionHandle>, store: State<StoreHandle>) -> CommandReceipt {
    // 會議列在這裡建立，不在 app 啟動時：否則每開一次程式就多一列空會議
    let created = match store.exclusive() {
        Ok(mut st) => {
            let title = format!("會議 {}", crate::clock::now_utc()[..16].replace('T', " "));
            st.create_meeting(&title)
                .and_then(|id| Ok((id, st.id_seeds()?)))
        }
        Err(_) => return CommandReceipt::rejected("資料庫連線狀態已損毀"),
    };
    let (meeting, seeds) = match created {
        Ok(v) => v,
        Err(e) => return CommandReceipt::rejected(&format!("無法建立會議：{e}")),
    };
    command(&state, &store, |s| s.start(meeting, seeds))
}

#[tauri::command]
pub fn pause_meeting(state: State<SessionHandle>, store: State<StoreHandle>) -> CommandReceipt {
    command(&state, &store, |s| s.pause())
}

#[tauri::command]
pub fn resume_meeting(state: State<SessionHandle>, store: State<StoreHandle>) -> CommandReceipt {
    command(&state, &store, |s| s.resume())
}

#[tauri::command]
pub fn add_note(
    state: State<SessionHandle>,
    store: State<StoreHandle>,
    text: String,
) -> CommandReceipt {
    command(&state, &store, |s| s.add_note(&text))
}

#[tauri::command]
pub fn confirm_speaker(
    state: State<SessionHandle>,
    store: State<StoreHandle>,
    speaker_id: String,
    name: String,
) -> CommandReceipt {
    command(&state, &store, |s| s.confirm_speaker(&speaker_id, &name))
}

#[tauri::command]
pub fn edit_transcript(
    state: State<SessionHandle>,
    store: State<StoreHandle>,
    segment_id: u64,
    text: String,
) -> CommandReceipt {
    command(&state, &store, |s| s.edit_transcript(segment_id, &text))
}

#[tauri::command]
pub fn create_snapshot(
    app: AppHandle,
    state: State<SessionHandle>,
    store: State<StoreHandle>,
) -> CommandReceipt {
    let outcome = state.with(|s| s.create_snapshot());
    let (receipt, snapshot) = match outcome {
        Ok(v) => v,
        Err(_) => return CommandReceipt::rejected(POISONED_NOTE),
    };
    // 先讓 SnapshotCreated 落地，生成才有一個已持久化的游標可以依附
    flush(&state, &store);
    let Some((version, through)) = snapshot else {
        return receipt;
    };
    // 沒有會議就沒有東西可生成。run_generation 自己也會檢查，
    // 這裡先擋是為了不要白開一個背景工作。
    if state.with(|s| s.meeting_id()).ok().flatten().is_none() {
        return receipt;
    }

    // 生成在背景進行，錄音與逐字稿不中斷（§5.4.2）。
    // 編排本身在 run_generation，這裡只負責把它送進背景。
    tauri::async_runtime::spawn(async move {
        let worker = app.clone();
        // SQLite 與 Agent 迴圈都是同步的，放進 blocking pool 才不會卡住事件泵
        let _ = tauri::async_runtime::spawn_blocking(move || {
            let session: State<SessionHandle> = worker.state();
            let store: State<StoreHandle> = worker.state();
            run_generation(
                &session,
                &store,
                &mut crate::agent::FixturePlanner,
                version,
                through,
            );
        })
        .await;
    });
    receipt
}

/// 一輪摘要生成的完整編排。
///
/// 這個函式不碰 `AppHandle`，也不是 `#[tauri::command]`，因此測試可以直接
/// 呼叫它。先前這段程式住在 spawn 的 closure 裡，唯一的驗證方式是打包出
/// Windows 安裝檔再用滑鼠點一次 — 那個代價讓一個 `CHECK constraint` 的錯誤
/// 活到了實機，而且畫面上還顯示「已完成」。
///
/// 失敗一律落在 `GenerationFailed`：快照游標保留，使用者可以直接重試，
/// 而錄音與逐字稿全程不受影響。
pub fn run_generation(
    session: &SessionHandle,
    store: &StoreHandle,
    planner: &mut dyn crate::agent::Planner,
    version: u32,
    through: u64,
) {
    let Ok(Some(meeting)) = session.with(|s| s.meeting_id()) else {
        return;
    };

    // 讀證據用另開的唯讀連線，不動寫入鎖。抱著寫入鎖跑完整趟 LLM 呼叫，
    // 錄音的落地就會被卡住那麼久，那正好違反 §5.4.2 的「生成不中斷錄音」。
    let produced = store
        .reader()
        .map_err(|e| e.to_string())
        .and_then(|st| {
            crate::agent::generate(
                &st,
                &crate::agent::GenerationRequest {
                    meeting,
                    through_event_seq: through,
                    prompt: "",
                    budget: crate::agent::BudgetInputs::default(),
                    limits: crate::agent::Limits::default(),
                },
                planner,
                &crate::agent::CharUpperBound,
            )
            .map_err(|e| e.to_string())
        })
        .map(|r| {
            let blocks = r
                .blocks
                .iter()
                .enumerate()
                .map(|(i, b)| b.to_stored(i as u32))
                .collect::<Vec<_>>();
            let usage = serde_json::json!({
                "rounds": r.rounds,
                "stopReason": r.stop_reason,
                "evidenceTokens": r.evidence_tokens,
                "segmentsOmitted": r.segments_omitted,
                "rejectedBlocks": r.rejected_blocks,
                "degraded": r.degraded,
            });
            (blocks, usage)
        });

    let _ = match produced {
        Ok((blocks, usage)) => session.with(|s| s.finish_generation(version, blocks, usage, None)),
        Err(reason) => session.with(|s| {
            s.finish_generation(version, Vec::new(), serde_json::json!({}), Some(reason))
        }),
    };
    flush(session, store);
}

/// 某個摘要版本的成果區塊。
///
/// UI 講版本號，資料庫講 run_id，轉換在這裡做一次，前端不必知道兩者的關係。
#[tauri::command]
pub fn snapshot_document(
    store: State<StoreHandle>,
    meeting_id: i64,
    version_no: u32,
) -> Result<Vec<crate::store::DocumentBlock>, String> {
    with_store(&store, |st| {
        let Some(run) = st
            .runs(meeting_id)?
            .into_iter()
            .find(|r| r.version_no == version_no)
        else {
            return Ok(Vec::new());
        };
        st.run_blocks(run.run_id)
    })
}

/// 匯出成果 HTML（§10）。寫到 app data dir 的 exports/ 並回傳路徑。
#[tauri::command]
pub fn export_document(
    app: AppHandle,
    store: State<StoreHandle>,
    meeting_id: i64,
    run_id: i64,
) -> Result<String, String> {
    use tauri::Manager as _;

    let st = store.exclusive().map_err(|e| e.to_string())?;
    let run = st
        .runs(meeting_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|r| r.run_id == run_id)
        .ok_or("找不到這個生成版本")?;
    let blocks: Vec<_> = st
        .run_blocks(run_id)
        .map_err(|e| e.to_string())?
        .iter()
        .filter_map(crate::document::Block::from_stored)
        .collect();
    // 逐字稿讀的是快照當時的版本，不是現在的：已匯出文件不被後續修訂改寫
    let transcript = st
        .segments_through(meeting_id, run.through_event_seq)
        .map_err(|e| e.to_string())?;

    let html = crate::document::render_html(
        &crate::document::RenderContext {
            title: &run.title,
            version_no: run.version_no,
            through_event_seq: run.through_event_seq,
            created_at: &run.created_at,
            transcript: &transcript,
        },
        &blocks,
    );

    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("exports");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("meeting-{meeting_id}-v{}.html", run.version_no));
    std::fs::write(&path, html).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn stop_meeting(state: State<SessionHandle>, store: State<StoreHandle>) -> CommandReceipt {
    command(&state, &store, |s| s.stop())
}

/// 目前正在進行的會議 id。歷史頁用它標出「這場還在錄」並擋掉刪除。
#[tauri::command]
pub fn active_meeting(state: State<SessionHandle>) -> Option<i64> {
    state.with(|s| s.active_meeting_id()).ok().flatten()
}

/* ── 歷史 ─────────────────────────────────────────────────────────── */

#[tauri::command]
pub fn list_meetings(
    store: State<StoreHandle>,
) -> Result<Vec<crate::store::MeetingSummary>, String> {
    with_store(&store, |st| st.list_meetings())
}

#[tauri::command]
pub fn open_meeting(
    store: State<StoreHandle>,
    meeting_id: i64,
) -> Result<crate::store::MeetingDetail, String> {
    with_store(&store, |st| st.meeting(meeting_id))
}

#[tauri::command]
pub fn rename_meeting(
    store: State<StoreHandle>,
    meeting_id: i64,
    title: String,
) -> Result<(), String> {
    let title = title.trim().to_owned();
    if title.is_empty() {
        return Err("標題不得為空".into());
    }
    // 標題不是事件的投影，它由使用者直接命名，因此就地更新。
    with_store(&store, |st| st.rename_meeting(meeting_id, &title))
}

#[tauri::command]
pub fn delete_meeting(
    state: State<SessionHandle>,
    store: State<StoreHandle>,
    meeting_id: i64,
) -> Result<(), String> {
    // 正在錄的那場不能刪：刪掉之後 flush 會寫進不存在的會議
    if matches!(state.with(|s| s.active_meeting_id()), Ok(Some(id)) if id == meeting_id) {
        return Err("這場會議正在進行中，請先停止錄音".into());
    }
    with_store(&store, |st| st.delete_meeting(meeting_id))
}

/// 從事件日誌重建投影。
///
/// 這是 §5.4.1 那條「投影必須能在任何時候重建」的實際入口。平時用不到，
/// 但它存在本身就是規則成立的證據，而且災難復原時需要它。
#[tauri::command]
pub fn rebuild_projections(
    state: State<SessionHandle>,
    store: State<StoreHandle>,
    meeting_id: i64,
) -> Result<(), String> {
    if matches!(state.with(|s| s.active_meeting_id()), Ok(Some(id)) if id == meeting_id) {
        return Err("這場會議正在進行中，請先停止錄音".into());
    }
    with_store(&store, |st| st.rebuild_projections(meeting_id))
}

fn with_store<T, F>(store: &State<StoreHandle>, f: F) -> Result<T, String>
where
    F: FnOnce(&mut Store) -> crate::store::Result<T>,
{
    let mut st = store.exclusive().map_err(|e| e.to_string())?;
    f(&mut st).map_err(|e| e.to_string())
}

/// 結束後開新會議。沿用同一個 Session 會把上一場的片段與快照帶進來。
#[tauri::command]
pub fn new_meeting(state: State<SessionHandle>) -> CommandReceipt {
    state.reset();
    CommandReceipt {
        accepted: true,
        seq: Some(0),
        note: None,
    }
}

/// UI 偵測到事件缺號或重新載入時，用完整投影重建，而不是帶著破洞繼續套用增量。
#[tauri::command]
pub fn resync(state: State<SessionHandle>) -> Result<SessionProjection, String> {
    state
        .with(|s| s.projection())
        .map_err(|_| POISONED_NOTE.to_owned())
}

/// 原型用的降級注入。真實來源是 Adapter 回報，不是命令。
#[tauri::command]
pub fn inject_fault(
    state: State<SessionHandle>,
    store: State<StoreHandle>,
    kind: String,
) -> CommandReceipt {
    command(&state, &store, |s| s.inject_fault(&kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// 把 Session 累積的日誌寫進 Store，回傳配發到的 seq。
    /// 這是 `flush` 的測試版：同樣只有這一個 append 呼叫點。
    fn drain_to(s: &mut Session, store: &mut Store, meeting: crate::store::MeetingId) -> Vec<u64> {
        let batch = s.take_journal();
        store.append(meeting, &batch).unwrap()
    }

    fn with_store() -> (Session, Store, crate::store::MeetingId) {
        let mut store = Store::new(db::open_in_memory().unwrap());
        let meeting = store.create_meeting("測試").unwrap();
        let mut s = Session::default();
        assert!(s.start(meeting, Default::default()).accepted);
        (s, store, meeting)
    }

    /// 可控的逐字稿來源，讓測試直接描述 STT 的行為，
    /// 包含亂序 partial 與重複 final 這些 Fixture 不會產生的情況。
    #[derive(Default)]
    struct ScriptedSource {
        queue: Vec<Vec<TranscriptInput>>,
    }

    impl ScriptedSource {
        fn push(&mut self, batch: Vec<TranscriptInput>) {
            self.queue.push(batch);
        }
    }

    impl TranscriptSource for ScriptedSource {
        fn poll(&mut self, _window_ms: u64) -> Vec<TranscriptInput> {
            if self.queue.is_empty() {
                Vec::new()
            } else {
                self.queue.remove(0)
            }
        }
    }

    fn partial(id: u64, text: &str) -> TranscriptInput {
        TranscriptInput::Partial {
            segment_id: id,
            speaker_id: "s1".into(),
            text: text.into(),
            track: Track::System,
        }
    }

    fn final_(id: u64, text: &str) -> TranscriptInput {
        TranscriptInput::Final {
            segment_id: id,
            speaker_id: "s1".into(),
            text: text.into(),
            track: Track::System,
        }
    }

    /// 走真正的 `start` 命令，不手動設定狀態。
    fn recording() -> Session {
        let mut s = Session::default();
        assert!(s.start(1, Default::default()).accepted);
        s.pending.clear();
        s.take_journal();
        s
    }

    fn scripted(batches: Vec<Vec<TranscriptInput>>) -> Session {
        scripted_for(1, batches)
    }

    /// 綁到指定會議 id 的腳本 Session，供需要真實 Store 的測試使用。
    fn scripted_for(
        meeting: crate::store::MeetingId,
        batches: Vec<Vec<TranscriptInput>>,
    ) -> Session {
        let mut src = ScriptedSource::default();
        for b in batches {
            src.push(b);
        }
        let mut s = Session::with_source(Box::new(src));
        assert!(s.start(meeting, Default::default()).accepted);
        s.pending.clear();
        s
    }

    #[test]
    fn test_partial_carries_no_seq_and_stays_out_of_the_log() {
        let mut s = scripted(vec![vec![partial(1, "測")]]);
        s.tick(100);
        let batch = s.drain(0);
        assert!(
            batch
                .events
                .iter()
                .any(|e| matches!(e, SessionEvent::TranscriptPartial { .. })),
            "應該有 partial"
        );
        // partial 本身不帶 seq。這一批唯一的 seq 來自「第一次聽到 s1」，
        // 那是關於語者的決定性事實，與這句話會不會定稿無關。
        assert!(batch
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::SpeakerProposed { .. })));
        assert_eq!(s.seq, 2, "MeetingStateChanged 與 SpeakerProposed 各佔一個");
    }

    #[test]
    fn test_finalized_segment_advances_seq_exactly_once() {
        let mut s = scripted(vec![vec![final_(1, "完成")]]);
        s.tick(100);
        // start 佔 1，第一次聽到 s1 佔 1，final 佔 1
        assert_eq!(s.seq, 3);
        // 同一位語者第二次發言不再提出
        s.source = Box::new({
            let mut src = ScriptedSource::default();
            src.push(vec![final_(2, "第二句")]);
            src
        });
        s.tick(200);
        assert_eq!(s.seq, 4, "重複的語者不得再配發一個 seq");
    }

    #[test]
    fn test_late_partial_cannot_rewrite_a_finalized_segment() {
        let mut s = scripted(vec![vec![final_(1, "定稿內容")], vec![partial(1, "遲到")]]);
        s.tick(100);
        s.pending.clear();
        s.tick(200);
        let batch = s.drain(0);
        assert!(
            !batch
                .events
                .iter()
                .any(|e| matches!(e, SessionEvent::TranscriptPartial { .. })),
            "已定稿的片段不得再接受 partial"
        );
        assert_eq!(s.segments[&1].text, "定稿內容");
    }

    #[test]
    fn test_duplicate_final_does_not_allocate_a_second_seq() {
        let mut s = scripted(vec![vec![final_(1, "第一次")], vec![final_(1, "重送")]]);
        s.tick(100);
        let after_first = s.seq;
        s.tick(200);
        assert_eq!(s.seq, after_first, "重複的 final 不得再配發 seq");
        assert_eq!(s.segments[&1].text, "第一次");
    }

    #[test]
    fn test_provider_final_cannot_overwrite_a_user_edit() {
        let mut s = scripted(vec![
            vec![final_(1, "原始內容")],
            vec![final_(1, "重連後的結果")],
        ]);
        s.tick(100);
        assert!(s.edit_transcript(1, "使用者改過的內容").accepted);
        s.tick(200);
        assert_eq!(
            s.segments[&1].text, "使用者改過的內容",
            "Provider 的後到結果不得覆蓋使用者修訂"
        );
        assert_eq!(s.segments[&1].state.origin, Origin::User);
        assert_eq!(s.segments[&1].state.revision, 2);
    }

    #[test]
    fn test_edit_rejects_unknown_segment() {
        let mut s = recording();
        let r = s.edit_transcript(999, "不存在的片段");
        assert!(!r.accepted);
        assert_eq!(s.seq, 1, "被拒絕的命令不得配發 seq");
    }

    #[test]
    fn test_pause_boundary_is_exact_not_sampled() {
        let mut s = recording();
        s.tick(100);
        // 距上一次結算 90 ms 之後才暫停，那 90 ms 屬於錄音時間
        s.settle(190);
        assert!(s.pause().accepted);
        assert_eq!(s.captured_audio_ms, 190);

        // 暫停 500 ms：會議時間前進，錄音時間不動
        s.settle(690);
        assert_eq!(s.captured_audio_ms, 190);
        assert_eq!(s.meeting_time_ms, 690);

        assert!(s.resume().accepted);
        s.settle(790);
        assert_eq!(s.captured_audio_ms, 290);
    }

    #[test]
    fn test_state_change_event_carries_the_transition_timestamps() {
        let mut s = recording();
        s.settle(250);
        s.pause();
        let batch = s.drain(0);
        let ev = batch
            .events
            .iter()
            .find(|e| matches!(e, SessionEvent::MeetingStateChanged { .. }))
            .expect("應有狀態事件");
        match ev {
            SessionEvent::MeetingStateChanged {
                meeting_time_ms,
                captured_audio_ms,
                ..
            } => {
                assert_eq!(*meeting_time_ms, 250);
                assert_eq!(*captured_audio_ms, 250);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_stop_while_paused_closes_the_pause_interval() {
        let mut s = recording();
        s.settle(100);
        s.pause();
        s.settle(400);
        s.stop();
        let open = s.pauses.iter().filter(|p| p.to_ms.is_none()).count();
        assert_eq!(open, 0, "停止時暫停區間必須封口");
        assert_eq!(s.pauses[0].from_ms, 100);
        assert_eq!(s.pauses[0].to_ms, Some(400));
    }

    #[test]
    fn test_paused_session_stops_capture_but_accepts_notes() {
        let mut s = recording();
        s.settle(100);
        s.pause();
        s.pending.clear();

        let captured_before = s.captured_audio_ms;
        s.settle(600);
        assert_eq!(
            s.captured_audio_ms, captured_before,
            "暫停期間不得推進錄音位置"
        );
        assert!(
            s.meeting_time_ms > captured_before,
            "會議時間軸含暫停，仍須前進"
        );

        let r = s.add_note("暫停中也能記");
        assert!(r.accepted, "暫停的是擷取，不是文件工作");
        assert_eq!(s.notes.len(), 1);
    }

    #[test]
    fn test_health_events_are_emitted_while_paused() {
        let mut s = recording();
        s.settle(100);
        s.pause();
        s.pending.clear();
        s.inject_fault("micLost");
        s.tick(200);
        let batch = s.drain(0);
        assert!(
            batch.events.iter().any(|e| matches!(
                e,
                SessionEvent::TrackActivity {
                    mic_health: Health::Lost,
                    ..
                }
            )),
            "暫停期間仍必須看得到健康狀態變化"
        );
        assert_eq!(batch.state, MeetingState::Paused, "健康變化不改變生命週期");
    }

    #[test]
    fn test_stt_outage_keeps_recording_and_produces_no_transcript() {
        let mut s = scripted(vec![vec![final_(1, "不該出現")]]);
        s.inject_fault("sttDown");
        s.tick(400);
        let batch = s.drain(0);
        assert_eq!(
            batch.state,
            MeetingState::Recording,
            "STT 斷線不改變會議狀態"
        );
        assert!(batch.captured_audio_ms > 0, "錄音仍在前進");
        assert!(
            !batch
                .events
                .iter()
                .any(|e| matches!(e, SessionEvent::TranscriptFinalized { .. })),
            "斷線期間不產生逐字稿"
        );
    }

    #[test]
    fn test_commands_are_rejected_outside_a_live_meeting() {
        let mut s = Session::default();
        assert!(!s.add_note("尚未開始").accepted);
        assert!(!s.create_snapshot().0.accepted);
        assert!(!s.confirm_speaker("s1", "某人").accepted);
        assert_eq!(s.seq, 0, "被拒絕的命令不得配發 seq");

        s.start(1, Default::default());
        s.stop();
        assert_eq!(s.state, MeetingState::Completed);
        assert!(!s.add_note("已結束").accepted);
        assert!(
            !s.start(1, Default::default()).accepted,
            "結束後不得沿用同一個 Session 重新開始"
        );
    }

    #[test]
    fn test_stop_walks_through_finalizing() {
        let mut s = recording();
        s.stop();
        let batch = s.drain(0);
        let states: Vec<MeetingState> = batch
            .events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::MeetingStateChanged { state, .. } => Some(*state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![
                MeetingState::Stopping,
                MeetingState::Finalizing,
                MeetingState::Completed
            ]
        );
    }

    #[test]
    fn test_batch_exposes_the_previous_high_seq_for_gap_detection() {
        let mut s = recording();
        s.add_note("第一筆");
        let first = s.drain(0);
        assert_eq!(first.prev_high_seq, 0);
        let high = first.last_seq.unwrap();

        s.add_note("第二筆");
        let second = s.drain(high);
        assert_eq!(second.prev_high_seq, high);
        assert_eq!(
            second.first_seq,
            Some(high + 1),
            "連續批次的 seq 必須銜接，UI 才能靠它偵測缺號"
        );
    }

    #[test]
    fn test_snapshot_cursor_excludes_events_after_it() {
        let mut s = recording();
        s.add_note("快照前");
        let (receipt, version) = s.create_snapshot();
        assert!(receipt.accepted);
        assert_eq!(version.map(|(v, _)| v), Some(1));
        let through = s.snapshots[0].through_event_seq;

        s.add_note("快照後");
        assert!(s.seq > through, "快照之後仍持續產生決定性事件");
        assert_eq!(
            s.snapshots[0].through_event_seq, through,
            "游標不得被之後的事件改動"
        );
    }

    #[test]
    fn test_projection_reflects_committed_state_for_resync() {
        let mut s = scripted(vec![vec![final_(1, "第一句")]]);
        s.tick(100);
        s.add_note("一筆筆記");
        s.create_snapshot();

        let p = s.projection();
        assert_eq!(p.segments.len(), 1);
        assert_eq!(p.segments[0].text, "第一句");
        assert_eq!(p.notes.len(), 1);
        assert_eq!(p.snapshots.len(), 1);
        assert_eq!(p.state, MeetingState::Recording);
        assert_eq!(p.seq, s.seq);
    }

    /* ── 持久化 ───────────────────────────────────────────────────── */

    #[test]
    fn test_every_decisive_event_reaches_the_event_log() {
        let (mut s, mut store, m) = with_store();
        s.tick(100);
        s.add_note("記一筆");
        s.confirm_speaker("s1", "小明");
        s.stop();

        let seqs = drain_to(&mut s, &mut store, m);
        // 日誌裡的 seq 必須與 Session 配發的完全一致。
        // 兩邊各自配號的話，快照游標會指到不同的內容。
        assert_eq!(seqs.last().copied(), Some(s.seq));
        assert_eq!(seqs.len() as u64, s.seq);
        assert_eq!(store.events(m).unwrap().len() as u64, s.seq);
    }

    #[test]
    fn test_partials_never_reach_the_event_log() {
        let mut store = Store::new(db::open_in_memory().unwrap());
        let m = store.create_meeting("測試").unwrap();
        let mut s = scripted_for(
            m,
            vec![
                vec![partial(1, "一")],
                vec![partial(1, "一二")],
                vec![partial(1, "一二三")],
                vec![final_(1, "一二三")],
            ],
        );
        for i in 1..=4 {
            s.tick(i * 100);
        }
        drain_to(&mut s, &mut store, m);

        let kinds: Vec<_> = store
            .events(m)
            .unwrap()
            .iter()
            .map(|e| e.event.kind())
            .collect();
        // 一場兩小時會議的 partial 是 final 的數十倍，
        // 放進日誌會讓 through_event_seq 失去意義（§5.4.1）
        assert_eq!(
            kinds,
            vec![
                "MeetingStateChanged",
                "SpeakerProposed",
                "TranscriptSegmentFinalized"
            ]
        );
    }

    #[test]
    fn test_a_reopened_meeting_shows_what_was_recorded() {
        let mut store = Store::new(db::open_in_memory().unwrap());
        let m = store.create_meeting("測試").unwrap();
        let mut s = scripted_for(
            m,
            vec![vec![partial(1, "重點")], vec![final_(1, "重點內容")]],
        );
        s.tick(100);
        s.tick(200);
        s.add_note("待辦：回報進度");
        s.edit_transcript(1, "使用者修正過的重點內容");
        s.stop();
        drain_to(&mut s, &mut store, m);

        // 這裡故意不看 Session，只看資料庫：重開 app 之後能拿到的就是這些
        let d = store.meeting(m).unwrap();
        assert_eq!(d.summary.state, MeetingState::Completed);
        assert_eq!(d.segments.len(), 1);
        assert_eq!(d.segments[0].text, "使用者修正過的重點內容");
        assert_eq!(d.segments[0].revision, 2);
        assert!(d.segments[0].user_edited);
        assert_eq!(d.notes.len(), 1);
        assert_eq!(d.notes[0].text, "待辦：回報進度");
        assert!(d.summary.started_at.is_some() && d.summary.ended_at.is_some());
    }

    #[test]
    fn test_projection_rebuild_after_a_real_session_matches_live_writes() {
        let mut store = Store::new(db::open_in_memory().unwrap());
        let m = store.create_meeting("測試").unwrap();
        let mut s = scripted_for(
            m,
            vec![vec![final_(1, "第一句")], vec![final_(2, "第二句")]],
        );
        s.tick(100);
        s.add_note("筆記");
        s.tick(200);
        s.pause();
        s.resume();
        s.stop();
        drain_to(&mut s, &mut store, m);

        let before = format!("{:?}", store.meeting(m).unwrap().segments);
        store.rebuild_projections(m).unwrap();
        assert_eq!(before, format!("{:?}", store.meeting(m).unwrap().segments));
    }

    #[test]
    fn test_a_journal_failure_rejects_every_command_that_would_create_events() {
        let (mut s, _store, _m) = with_store();
        s.speakers.insert(
            "s1".into(),
            SpeakerRecord {
                ordinal: 1,
                proposed_name: None,
                confirmed_name: None,
                track: Track::System,
            },
        );
        s.journal_failed("磁碟已滿".into());
        for r in [
            s.add_note("記一筆"),
            s.confirm_speaker("s1", "小明"),
            s.edit_transcript(1, "改一下"),
            s.create_snapshot().0,
        ] {
            assert!(!r.accepted, "日誌寫不進去時不得假裝命令成功");
            assert!(r.note.unwrap().contains("無法寫入本機資料庫"));
        }
    }

    #[test]
    fn test_finalized_segment_records_the_interval_it_was_spoken_in() {
        let mut store = Store::new(db::open_in_memory().unwrap());
        let m = store.create_meeting("測試").unwrap();
        let mut s = scripted_for(
            m,
            vec![vec![partial(1, "開")], vec![], vec![final_(1, "開始說話")]],
        );
        s.tick(100);
        s.tick(200);
        s.tick(300);
        drain_to(&mut s, &mut store, m);

        let ev = store
            .events(m)
            .unwrap()
            .into_iter()
            .find_map(|e| match e.event {
                DomainEvent::TranscriptSegmentFinalized { segment } => Some(segment),
                _ => None,
            })
            .expect("沒有寫入定稿事件");
        // 區間從第一個 partial 出現的時刻起算，不是定稿的瞬間。
        // 去重靠的正是這個區間（§5.3.2），塌成一個點就永遠判不出重複。
        assert_eq!(ev.meeting_start_ms, 100);
        assert_eq!(ev.meeting_end_ms, 300);
        assert!(ev.captured_end_ms > ev.captured_start_ms);
        assert_eq!(ev.track, Track::System);
    }

    /* ── 跨串流去重（§5.3.2） ─────────────────────────────────── */

    /// 同一段音訊在重連後被重播：Provider 給的是全新的 segment id。
    fn replay(id: u64, text: &str) -> TranscriptInput {
        TranscriptInput::Final {
            segment_id: id,
            speaker_id: "s1".into(),
            text: text.into(),
            track: Track::System,
        }
    }

    #[test]
    fn test_a_replayed_interval_with_the_same_text_creates_no_second_segment() {
        let mut store = Store::new(db::open_in_memory().unwrap());
        let m = store.create_meeting("測試").unwrap();
        // 兩個不同的 segment id，落在同一個音訊區間，內容相同
        let mut s = scripted_for(
            m,
            vec![
                vec![replay(1, "報價要分三項")],
                vec![replay(77, "報價要分三項")],
            ],
        );
        s.tick(100);
        s.tick(100);
        drain_to(&mut s, &mut store, m);

        let d = store.meeting(m).unwrap();
        assert_eq!(d.segments.len(), 1, "重播的音訊長出了第二個片段");
        assert_eq!(d.segments[0].revision, 1);
    }

    #[test]
    fn test_a_replayed_interval_with_different_text_becomes_a_revision() {
        let mut store = Store::new(db::open_in_memory().unwrap());
        let m = store.create_meeting("測試").unwrap();
        let mut s = scripted_for(
            m,
            vec![
                vec![replay(1, "報價要分三項")],
                vec![replay(77, "報價要分三項，含維運")],
            ],
        );
        s.tick(100);
        s.tick(100);
        drain_to(&mut s, &mut store, m);

        let d = store.meeting(m).unwrap();
        // 同區間不同內容是 Provider 改了結果，不是又講了一次
        assert_eq!(d.segments.len(), 1);
        assert_eq!(d.segments[0].revision, 2);
        assert_eq!(d.segments[0].text, "報價要分三項，含維運");
    }

    #[test]
    fn test_a_replayed_interval_cannot_overwrite_a_user_edit() {
        let mut store = Store::new(db::open_in_memory().unwrap());
        let m = store.create_meeting("測試").unwrap();
        let mut s = scripted_for(
            m,
            vec![
                vec![replay(1, "報價要分三項")],
                vec![],
                vec![replay(77, "Provider 重連後的版本")],
            ],
        );
        s.tick(100);
        s.edit_transcript(1, "使用者改過的內容");
        s.tick(200);
        s.tick(300);
        drain_to(&mut s, &mut store, m);

        let d = store.meeting(m).unwrap();
        assert_eq!(d.segments[0].text, "使用者改過的內容");
        assert!(d.segments[0].user_edited);
    }

    #[test]
    fn test_different_intervals_stay_separate_segments() {
        let mut store = Store::new(db::open_in_memory().unwrap());
        let m = store.create_meeting("測試").unwrap();
        let mut s = scripted_for(
            m,
            vec![vec![replay(1, "第一句")], vec![], vec![replay(2, "第二句")]],
        );
        s.tick(100);
        s.tick(200);
        s.tick(300);
        drain_to(&mut s, &mut store, m);
        assert_eq!(store.meeting(m).unwrap().segments.len(), 2);
    }

    #[test]
    fn test_a_journal_failure_is_announced_in_the_next_batch() {
        let (mut s, _store, _m) = with_store();
        s.tick(100);
        s.drain(0);
        s.journal_failed("磁碟已滿".into());
        // pending 是空的，但這個批次還是要送，否則畫面會停在「一切正常」
        let batch = s.drain(s.seq);
        assert_eq!(batch.journal_error.as_deref(), Some("磁碟已滿"));
    }

    #[test]
    fn test_the_first_journal_failure_is_the_one_reported() {
        let (mut s, _store, _m) = with_store();
        s.journal_failed("磁碟已滿".into());
        // 後續失敗多半是前一個的連鎖反應，換掉原因會蓋掉真正的線索
        s.journal_failed("資料庫連線狀態已損毀".into());
        assert_eq!(s.drain(0).journal_error.as_deref(), Some("磁碟已滿"));
    }

    #[test]
    fn test_a_finished_meeting_is_no_longer_active() {
        let (mut s, _store, m) = with_store();
        assert_eq!(s.active_meeting_id(), Some(m));
        s.pause();
        assert_eq!(s.active_meeting_id(), Some(m), "暫停中的會議仍在進行");
        s.stop();
        // 結束後不會再產生事件，因此可以安全刪除，也不該顯示成進行中
        assert_eq!(s.active_meeting_id(), None);
        // 但 flush 仍要知道最後一批事件屬於哪場會議
        assert_eq!(s.meeting_id(), Some(m));
    }

    /* ── 語者（§8.3） ─────────────────────────────────────────── */

    #[test]
    fn test_a_speaker_is_proposed_before_it_can_be_confirmed() {
        let mut store = Store::new(db::open_in_memory().unwrap());
        let m = store.create_meeting("測試").unwrap();
        let mut s = scripted_for(m, vec![vec![final_(1, "我先講")]]);
        s.tick(100);
        assert!(s.confirm_speaker("s1", "小明").accepted);
        drain_to(&mut s, &mut store, m);

        // 這正是先前無聲遺失的東西：畫面說已確認，磁碟上要真的有
        let d = store.meeting(m).unwrap();
        assert_eq!(d.speakers.len(), 1);
        assert_eq!(d.speakers[0].speaker_id, "s1");
        assert_eq!(d.speakers[0].confirmed_name.as_deref(), Some("小明"));
        assert_eq!(d.speakers[0].ordinal, 1);
    }

    #[test]
    fn test_confirming_a_speaker_nobody_has_heard_is_rejected() {
        let mut s = recording();
        let r = s.confirm_speaker("ghost", "誰");
        assert!(!r.accepted);
        assert!(r.note.unwrap().contains("尚未聽到"));
    }

    #[test]
    fn test_speakers_are_numbered_in_order_of_first_appearance() {
        let mut s = scripted(vec![
            vec![TranscriptInput::Final {
                segment_id: 1,
                speaker_id: "b".into(),
                text: "先".into(),
                track: Track::System,
            }],
            vec![TranscriptInput::Final {
                segment_id: 2,
                speaker_id: "a".into(),
                text: "後".into(),
                track: Track::Mic,
            }],
        ]);
        s.tick(100);
        s.tick(200);
        assert_eq!(s.speakers["b"].ordinal, 1);
        assert_eq!(s.speakers["a"].ordinal, 2);
        assert_eq!(s.speakers["a"].track, Track::Mic);
    }

    #[test]
    fn test_a_proposed_speaker_carries_its_track_to_the_ui() {
        let mut s = scripted(vec![vec![TranscriptInput::Final {
            segment_id: 1,
            speaker_id: "me".into(),
            text: "我說的".into(),
            track: Track::Mic,
        }]]);
        s.tick(100);
        let batch = s.drain(0);
        let proposed = batch
            .events
            .iter()
            .find_map(|e| match e {
                SessionEvent::SpeakerProposed { track, .. } => Some(*track),
                _ => None,
            })
            .expect("沒有提出語者");
        // §8.1 的軌道先驗只到「本機 vs 遠端」，稱呼由 UI 依它決定
        assert_eq!(proposed, Track::Mic);
    }

    /* ── 生成編排（先前無法測試的那 60 行） ──────────────────── */

    use crate::agent::{DraftRequest, Planner};
    use crate::document::Block;

    /// 錄好一場會議並凍結快照，回傳可直接驅動 `run_generation` 的兩個把手。
    fn ready_to_generate() -> (
        SessionHandle,
        StoreHandle,
        crate::store::MeetingId,
        u32,
        u64,
    ) {
        // 檔案型資料庫，因為 run_generation 會另開唯讀連線讀證據。
        // 用記憶體資料庫的話那條連線看不到任何東西，測試就繞過了正式路徑。
        let store = StoreHandle::temp().unwrap();
        let m = store.exclusive().unwrap().create_meeting("測試").unwrap();
        let mut s = scripted_for(m, vec![vec![final_(1, "報價要拆成設計、開發、維運三項")]]);
        s.tick(100);
        let (receipt, snap) = s.create_snapshot();
        assert!(receipt.accepted);
        let (version, through) = snap.unwrap();
        store
            .exclusive()
            .unwrap()
            .append(m, &s.take_journal())
            .unwrap();
        (SessionHandle::from_session(s), store, m, version, through)
    }

    #[test]
    fn test_generation_output_reaches_the_database_not_just_the_screen() {
        let (session, store, m, version, through) = ready_to_generate();
        run_generation(
            &session,
            &store,
            &mut crate::agent::FixturePlanner,
            version,
            through,
        );

        // 這正是先前那個 CHECK constraint 的 bug 會被擋下的地方：
        // 畫面顯示「已完成」不算數，磁碟上要真的有區塊
        let st = store.exclusive().unwrap();
        let run = st
            .runs(m)
            .unwrap()
            .into_iter()
            .find(|r| r.version_no == version)
            .unwrap();
        assert_eq!(run.status, "completed");
        assert!(!st.run_blocks(run.run_id).unwrap().is_empty());
        assert!(session.with(|s| s.journal_error.clone()).unwrap().is_none());
    }

    #[test]
    fn test_every_block_kind_the_planner_can_emit_is_writable() {
        /// 一次吐出全部 12 種區塊，把 schema 與 BlockKind 釘在一起。
        struct EveryKind;
        impl Planner for EveryKind {
            fn draft(&mut self, _req: &DraftRequest<'_>) -> crate::agent::Result<Vec<Block>> {
                use crate::document::{BlockContent, BlockKind};
                use crate::model::ClaimKind;
                Ok(crate::document::ALL_BLOCK_KINDS
                    .iter()
                    .filter(|k| {
                        // Fact 要引用，這個測試只關心「寫得進去嗎」
                        k.required_claim_kind() != Some(ClaimKind::Fact)
                    })
                    .map(|k| Block {
                        kind: *k,
                        claim_kind: k.required_claim_kind().unwrap_or(ClaimKind::Inference),
                        content: match k {
                            BlockKind::Heading => BlockContent::Heading {
                                level: 1,
                                text: "標題".into(),
                            },
                            BlockKind::BulletList => BlockContent::Bullets {
                                items: vec!["一".into()],
                            },
                            BlockKind::Table => BlockContent::Table {
                                headers: vec!["項目".into()],
                                rows: vec![vec!["設計".into()]],
                            },
                            BlockKind::MermaidDiagram => BlockContent::Mermaid {
                                source: "graph TD".into(),
                            },
                            BlockKind::Callout => BlockContent::Callout {
                                tone: "info".into(),
                                title: "注意".into(),
                                body: "內容".into(),
                            },
                            BlockKind::SourceLink => BlockContent::Link {
                                label: "來源".into(),
                                target: "#seg-1-r1".into(),
                            },
                            _ => BlockContent::Text {
                                text: "內容".into(),
                            },
                        },
                        source_refs: vec![],
                    })
                    .collect())
            }
        }

        let (session, store, m, version, through) = ready_to_generate();
        run_generation(&session, &store, &mut EveryKind, version, through);

        assert!(
            session.with(|s| s.journal_error.clone()).unwrap().is_none(),
            "有區塊種類寫不進 schema"
        );
        let st = store.exclusive().unwrap();
        let run = st
            .runs(m)
            .unwrap()
            .into_iter()
            .find(|r| r.version_no == version)
            .unwrap();
        assert_eq!(run.status, "completed");
        assert!(st.run_blocks(run.run_id).unwrap().len() >= 8);
    }

    #[test]
    fn test_a_planner_error_fails_the_run_and_keeps_the_snapshot_cursor() {
        struct Broken;
        impl Planner for Broken {
            fn draft(&mut self, _req: &DraftRequest<'_>) -> crate::agent::Result<Vec<Block>> {
                Err(crate::agent::AgentError::Provider(
                    "Provider 回報限流".into(),
                ))
            }
        }

        let (session, store, m, version, through) = ready_to_generate();
        run_generation(&session, &store, &mut Broken, version, through);

        let st = store.exclusive().unwrap();
        let run = st
            .runs(m)
            .unwrap()
            .into_iter()
            .find(|r| r.version_no == version)
            .unwrap();
        assert_eq!(run.status, "failed");
        assert!(run.failure_reason.unwrap().contains("限流"));
        // 游標保留，重試沿用同一個涵蓋範圍
        assert_eq!(run.through_event_seq, through);
    }

    #[test]
    fn test_recording_after_the_cursor_does_not_enter_this_round() {
        let (session, store, m, version, through) = ready_to_generate();
        // 生成開始之前又講了一句：它在游標之後，本輪不得引用
        let late = session
            .with(|s| {
                s.apply_transcript_input(final_(2, "這句話在快照之後"));
                s.take_journal()
            })
            .unwrap();
        store.exclusive().unwrap().append(m, &late).unwrap();

        run_generation(
            &session,
            &store,
            &mut crate::agent::FixturePlanner,
            version,
            through,
        );

        let st = store.exclusive().unwrap();
        let run = st
            .runs(m)
            .unwrap()
            .into_iter()
            .find(|r| r.version_no == version)
            .unwrap();
        let text = format!("{:?}", st.run_blocks(run.run_id).unwrap());
        assert!(
            !text.contains("這句話在快照之後"),
            "快照游標沒有凍結涵蓋範圍"
        );
        assert!(text.contains("報價要拆成設計"));
    }

    #[test]
    fn test_recording_keeps_landing_while_a_snapshot_generates() {
        use std::sync::mpsc;

        /// 在 draft 裡卡住，模擬一趟真實的 LLM 往返。
        struct Blocking {
            started: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Planner for Blocking {
            fn draft(&mut self, req: &DraftRequest<'_>) -> crate::agent::Result<Vec<Block>> {
                self.started.send(()).unwrap();
                self.release.recv().unwrap();
                crate::agent::FixturePlanner.draft(req)
            }
        }

        let (session, store, m, version, through) = ready_to_generate();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let wrote = std::sync::atomic::AtomicBool::new(false);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut p = Blocking {
                    started: started_tx.clone(),
                    release: release_rx,
                };
                run_generation(&session, &store, &mut p, version, through);
            });

            // 等生成真的進到模型那一步
            started_rx.recv().unwrap();

            // §5.4.2：這段期間錄音必須繼續落地。
            //
            // 這裡不能直接 assert：生成執行緒還在等 release，而 thread::scope
            // 會先 join 再讓 panic 傳出去，斷言失敗只會變成卡死。因此先記下
            // 結果、無條件放行，離開 scope 之後才斷言。
            // 時限也是必要的：規則被破壞時要在兩秒內判定，不是等到 CI 逾時。
            let ok = match store.try_exclusive_for(std::time::Duration::from_secs(2)) {
                Ok(mut st) => st
                    .append(
                        m,
                        &[(
                            DomainEvent::NoteAdded {
                                note_id: 99,
                                text: "生成期間記的一筆".into(),
                            },
                            Timeline::new(9_000, 9_000),
                        )],
                    )
                    .is_ok(),
                Err(_) => false,
            };
            wrote.store(ok, std::sync::atomic::Ordering::SeqCst);

            release_tx.send(()).unwrap();
        });

        assert!(
            wrote.load(std::sync::atomic::Ordering::SeqCst),
            "生成期間拿不到寫入鎖：生成抱著鎖跑完了整趟"
        );

        let st = store.exclusive().unwrap();
        // 生成期間寫進去的筆記還在
        assert!(st.meeting(m).unwrap().notes.iter().any(|n| n.note_id == 99));
        // 而且本輪成果仍然只涵蓋游標之前的內容
        let run = st
            .runs(m)
            .unwrap()
            .into_iter()
            .find(|r| r.version_no == version)
            .unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(run.through_event_seq, through);
    }
}
