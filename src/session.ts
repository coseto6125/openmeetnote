/**
 * Rust 核心的唯一入口。
 *
 * UI 只有兩個概念：送命令、收批次事件（BLUEPRINT.md §5.1）。
 * 這個模組把 Tauri 的 invoke 與 event 包成那兩個概念，
 * 其餘元件不直接 import @tauri-apps/api。
 */
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export type MeetingState = 'idle' | 'recording' | 'paused' | 'stopping' | 'finalizing' | 'completed';
export type Health = 'ok' | 'degraded' | 'lost';
export type Origin = 'provider' | 'user';

export type SessionEvent =
  /** 暫態：同一個 segmentId 會被反覆改寫，不帶 seq，不進事件序列 */
  | { kind: 'transcriptPartial'; segmentId: number; speakerId: string; text: string; meetingTimeMs: number }
  | {
      kind: 'transcriptFinalized';
      seq: number;
      segmentId: number;
      revision: number;
      origin: Origin;
      speakerId: string;
      text: string;
      meetingTimeMs: number;
      capturedAudioMs: number;
    }
  | {
      kind: 'transcriptEdited';
      seq: number;
      segmentId: number;
      revision: number;
      origin: Origin;
      text: string;
    }
  | {
      kind: 'noteAdded';
      seq: number;
      noteId: number;
      text: string;
      meetingTimeMs: number;
      capturedAudioMs: number;
    }
  | { kind: 'speakerConfirmed'; seq: number; speakerId: string; name: string }
  | { kind: 'snapshotCreated'; seq: number; version: number; throughEventSeq: number; meetingTimeMs: number }
  | { kind: 'generationCompleted'; seq: number; version: number }
  | { kind: 'generationFailed'; seq: number; version: number; reason: string }
  /** 帶轉換當下的兩條時間軸，UI 不必從批次邊界回推 */
  | {
      kind: 'meetingStateChanged';
      seq: number;
      state: MeetingState;
      meetingTimeMs: number;
      capturedAudioMs: number;
    }
  /** 暫態：子系統健康與生命週期正交（§6），暫停期間同樣會送 */
  | {
      kind: 'trackActivity';
      micLevel: number;
      systemLevel: number;
      micHealth: Health;
      systemHealth: Health;
      sttHealth: Health;
    };

export interface SessionEventBatch {
  firstSeq: number | null;
  lastSeq: number | null;
  /** 送出這批之前已配發到的最大 seq。`prevHighSeq + 1 !== firstSeq` 就是漏了事件 */
  prevHighSeq: number;
  emittedAtMs: number;
  meetingTimeMs: number;
  capturedAudioMs: number;
  state: MeetingState;
  /** 本機資料庫寫入失敗的原因。非 null 時畫面上的內容不保證有被保存。 */
  journalError: string | null;
  events: SessionEvent[];
}

export interface CommandReceipt {
  accepted: boolean;
  seq: number | null;
  note: string | null;
}

export interface ProjectedSegment {
  segmentId: number;
  revision: number;
  origin: Origin;
  speakerId: string;
  text: string;
  meetingTimeMs: number;
}

export interface SessionProjection {
  state: MeetingState;
  seq: number;
  meetingTimeMs: number;
  capturedAudioMs: number;
  segments: ProjectedSegment[];
  notes: { noteId: number; text: string; meetingTimeMs: number; capturedAudioMs: number }[];
  snapshots: {
    version: number;
    throughEventSeq: number;
    meetingTimeMs: number;
    state: 'queued' | 'running' | 'completed' | 'failed';
  }[];
  pauses: { fromMs: number; toMs: number | null }[];
}

export type FaultKind = 'micLost' | 'micRestored' | 'sttDown' | 'sttUp' | 'generationFailed';

export const commands = {
  start: () => invoke<CommandReceipt>('start_meeting'),
  pause: () => invoke<CommandReceipt>('pause_meeting'),
  resume: () => invoke<CommandReceipt>('resume_meeting'),
  addNote: (text: string) => invoke<CommandReceipt>('add_note', { text }),
  confirmSpeaker: (speakerId: string, name: string) =>
    invoke<CommandReceipt>('confirm_speaker', { speakerId, name }),
  editTranscript: (segmentId: number, text: string) =>
    invoke<CommandReceipt>('edit_transcript', { segmentId, text }),
  createSnapshot: () => invoke<CommandReceipt>('create_snapshot'),
  stop: () => invoke<CommandReceipt>('stop_meeting'),
  /** 事件缺號或重新載入之後，用完整投影重建，而不是帶著破洞繼續套用增量 */
  resync: () => invoke<SessionProjection>('resync'),
  /** 結束後開新會議。沿用同一個 Session 會把上一場的內容帶進來。 */
  newMeeting: () => invoke<CommandReceipt>('new_meeting'),
  injectFault: (kind: FaultKind) => invoke<CommandReceipt>('inject_fault', { kind }),
};

/* ── 歷史 ─────────────────────────────────────────────────────── */

export interface MeetingSummary {
  id: number;
  title: string;
  state: MeetingState;
  startedAt: string | null;
  endedAt: string | null;
  meetingTimeMs: number;
  capturedAudioMs: number;
  highSeq: number;
  segmentCount: number;
  noteCount: number;
  documentCount: number;
}

export interface StoredSegment {
  segmentId: number;
  revision: number;
  origin: Origin;
  speakerId: string | null;
  text: string;
  track: 'mic' | 'system';
  meetingStartMs: number;
  meetingEndMs: number;
  userEdited: boolean;
}

export interface StoredRun {
  runId: number;
  documentId: number;
  versionNo: number;
  throughEventSeq: number;
  status: 'queued' | 'running' | 'completed' | 'failed';
  title: string;
  purpose: string;
  prompt: string;
  failureReason: string | null;
  createdAt: string;
}

export interface MeetingDetail {
  summary: MeetingSummary;
  segments: StoredSegment[];
  notes: { noteId: number; text: string; meetingTimeMs: number; capturedAudioMs: number }[];
  speakers: {
    speakerId: string;
    ordinal: number;
    proposedName: string | null;
    confirmedName: string | null;
    status: string;
  }[];
  runs: StoredRun[];
}

export type ClaimKind = 'fact' | 'inference' | 'suggestion' | 'gap';

/** 已寫入資料庫的成果區塊。content 是該 kind 的 JSON。 */
export interface DocumentBlock {
  position: number;
  kind: string;
  claimKind: ClaimKind;
  content: string;
  sourceRefs: {
    sourceKind: string;
    sourceId: string;
    sourceRevision: number;
    locator: string;
    quotedText: string;
    quotedTextSha256: string;
    validationStatus: string;
  }[];
}

export const snapshotDocument = (meetingId: number, versionNo: number) =>
  invoke<DocumentBlock[]>('snapshot_document', { meetingId, versionNo });

export const activeMeeting = () => invoke<number | null>('active_meeting');

export const history = {
  list: () => invoke<MeetingSummary[]>('list_meetings'),
  open: (meetingId: number) => invoke<MeetingDetail>('open_meeting', { meetingId }),
  rename: (meetingId: number, title: string) =>
    invoke<void>('rename_meeting', { meetingId, title }),
  remove: (meetingId: number) => invoke<void>('delete_meeting', { meetingId }),
  /** 匯出成果 HTML，回傳寫出的檔案路徑。 */
  exportDocument: (meetingId: number, runId: number) =>
    invoke<string>('export_document', { meetingId, runId }),
  /** 從事件日誌重建投影（§5.4.1）。災難復原用，正在錄的會議不允許。 */
  rebuild: (meetingId: number) => invoke<void>('rebuild_projections', { meetingId }),
};

/* ── 設定 ─────────────────────────────────────────────────────── */

export type ProviderKind = 'stt' | 'llm';
export type FieldSource = 'environment' | 'settings' | 'default';
export type SecretPresence = 'environment' | 'keychain' | 'missing' | 'unavailable';

export interface ResolvedField {
  value: string;
  source: FieldSource;
}

/** 注意沒有密鑰欄位。密鑰不離開 Rust 端（§14）。 */
export interface ResolvedProvider {
  kind: ProviderKind;
  provider: ResolvedField;
  model: ResolvedField;
  baseUrl: ResolvedField;
  secret: SecretPresence;
}

export const settings = {
  get: () => invoke<ResolvedProvider[]>('get_settings'),
  saveProvider: (kind: ProviderKind, provider: string, model: string, baseUrl: string) =>
    invoke<void>('save_provider', { kind, provider, model, baseUrl }),
  saveSecret: (kind: ProviderKind, value: string) =>
    invoke<void>('save_secret', { kind, value }),
  clearSecret: (kind: ProviderKind) => invoke<void>('clear_secret', { kind }),
};

export function subscribe(onBatch: (b: SessionEventBatch) => void): Promise<UnlistenFn> {
  return listen<SessionEventBatch>('session://events', (e) => onBatch(e.payload));
}

export const mmss = (ms: number): string => {
  const s = Math.max(0, Math.floor(ms / 1000));
  return `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`;
};

export const hhmmss = (ms: number): string => {
  const s = Math.max(0, Math.floor(ms / 1000));
  return [Math.floor(s / 3600), Math.floor((s % 3600) / 60), s % 60]
    .map((n) => String(n).padStart(2, '0'))
    .join(':');
};
