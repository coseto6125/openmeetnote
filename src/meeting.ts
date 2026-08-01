/**
 * 會議投影：把事件批次摺疊成畫面狀態。
 *
 * 這裡是純函式，沒有 React 也沒有 Tauri，因此優先權規則可以被直接測試。
 * 規則本身在 Rust 端也有一份，兩邊都擋是刻意的：
 * Rust 保護事件序列，前端保護畫面，任何一邊單獨失效都不會讓錯誤內容顯示出來。
 */
import type {
  Health,
  MeetingState,
  Origin,
  SessionEvent,
  SessionEventBatch,
  SessionProjection,
} from './session';

export interface Segment {
  id: number;
  speakerId: string;
  text: string;
  meetingTimeMs: number;
  revision: number;
  origin: Origin;
  stability: 'partial' | 'final';
}

/**
 * 一位語者。
 *
 * 名稱優先順序（§8.4）：使用者確認的名稱勝過暫定名稱，暫定名稱勝過
 * 依軌道與出現序給的預設稱呼。這裡不從逐字稿內容推定任何人的身份。
 */
export interface Speaker {
  id: string;
  ordinal: number;
  track: 'mic' | 'system';
  proposedName: string | null;
  confirmedName: string | null;
}

export interface Note {
  id: number;
  text: string;
  meetingTimeMs: number;
  capturedAudioMs: number;
}

export interface Snapshot {
  version: number;
  throughEventSeq: number;
  meetingTimeMs: number;
  state: 'queued' | 'running' | 'completed' | 'failed';
  reason?: string;
}

export interface Pause {
  fromMs: number;
  toMs: number | null;
}

export interface Degrade {
  title: string;
  body: string;
  tone: 'warn' | 'bad';
}

export interface MeetingModel {
  state: MeetingState;
  meetingTimeMs: number;
  capturedAudioMs: number;
  segments: Segment[];
  notes: Note[];
  snapshots: Snapshot[];
  pauses: Pause[];
  activeVersion: number | null;
  levels: { mic: number; system: number };
  health: { mic: Health; system: Health; stt: Health };
  speakers: Speaker[];
  /** 已套用的最大 seq。收到不銜接的批次就代表中間漏了事件 */
  appliedSeq: number;
  /** 偵測到缺號，畫面內容不可信，必須 resync */
  desynced: boolean;
  /** 本機資料庫寫不進去。畫面上的內容不保證有被保存。 */
  journalError: string | null;
  degrade: Degrade | null;
}

export const emptyMeeting: MeetingModel = {
  state: 'idle',
  meetingTimeMs: 0,
  capturedAudioMs: 0,
  segments: [],
  notes: [],
  snapshots: [],
  pauses: [],
  activeVersion: null,
  levels: { mic: 0, system: 0 },
  health: { mic: 'ok', system: 'ok', stt: 'ok' },
  speakers: [],
  appliedSeq: 0,
  desynced: false,
  journalError: null,
  degrade: null,
};

/**
 * 能否覆寫既有內容。
 *
 * 這條規則在 Session、Store 與這裡各有一份。三份存在的理由是它們隔著不同的
 * 邊界（Session 決定要不要產生事件、Store 守投影、這裡守畫面），但它們必須
 * 判出完全相同的結果 —— 曾經有一版本規則不等價，Provider 的 r3 可以蓋掉
 * 使用者的 r2，而下層看起來像是擋得住。
 *
 * 順序是刻意的：先看來源再看版本。Provider 的結果永遠不覆蓋使用者修訂，
 * 版本號再高也一樣。
 */
function supersedes(
  incoming: { revision: number; origin: Origin },
  current: { revision: number; origin: Origin },
): boolean {
  if (current.origin === 'user' && incoming.origin === 'provider') return false;
  if (incoming.revision > current.revision) return true;
  if (incoming.revision < current.revision) return false;
  return incoming.origin === 'user' && current.origin === 'provider';
}

function upsertSegment(segments: Segment[], next: Segment): Segment[] {
  const i = segments.findIndex((s) => s.id === next.id);
  if (i === -1) {
    const out = [...segments, next];
    out.sort((a, b) => a.meetingTimeMs - b.meetingTimeMs || a.id - b.id);
    return out;
  }
  const cur = segments[i];
  // 已定稿的片段不接受 partial：reconnect 之後的 late partial 會讓內容倒退
  if (next.stability === 'partial' && cur.stability === 'final') return segments;
  if (next.stability === 'final' && cur.stability === 'final' && !supersedes(next, cur)) {
    return segments;
  }
  const out = segments.slice();
  out[i] = { ...cur, ...next };
  return out;
}

function applyEvent(m: MeetingModel, ev: SessionEvent, batch: SessionEventBatch): MeetingModel {
  switch (ev.kind) {
    case 'transcriptPartial':
      return {
        ...m,
        segments: upsertSegment(m.segments, {
          id: ev.segmentId,
          speakerId: ev.speakerId,
          text: ev.text,
          meetingTimeMs: ev.meetingTimeMs,
          revision: 0,
          origin: 'provider',
          stability: 'partial',
        }),
      };

    case 'transcriptFinalized':
      return {
        ...m,
        segments: upsertSegment(m.segments, {
          id: ev.segmentId,
          speakerId: ev.speakerId,
          text: ev.text,
          meetingTimeMs: ev.meetingTimeMs,
          revision: ev.revision,
          origin: ev.origin,
          stability: 'final',
        }),
      };

    case 'transcriptEdited': {
      const i = m.segments.findIndex((s) => s.id === ev.segmentId);
      if (i === -1) return m;
      const cur = m.segments[i];
      if (!supersedes({ revision: ev.revision, origin: ev.origin }, cur)) return m;
      const segments = m.segments.slice();
      segments[i] = { ...cur, text: ev.text, revision: ev.revision, origin: ev.origin };
      return { ...m, segments };
    }

    // 決定性事件一律以 id 去重。重連或批次重送時不能長出第二筆。
    case 'noteAdded':
      return m.notes.some((n) => n.id === ev.noteId)
        ? m
        : {
            ...m,
            notes: [
              ...m.notes,
              {
                id: ev.noteId,
                text: ev.text,
                meetingTimeMs: ev.meetingTimeMs,
                capturedAudioMs: ev.capturedAudioMs,
              },
            ],
          };

    case 'speakerProposed':
      // 決定性事件一律去重：重連或批次重送不能長出第二位語者
      return m.speakers.some((s) => s.id === ev.speakerId)
        ? m
        : {
            ...m,
            speakers: [
              ...m.speakers,
              {
                id: ev.speakerId,
                ordinal: ev.ordinal,
                track: ev.track,
                proposedName: ev.proposedName,
                confirmedName: null,
              },
            ].sort((a, b) => a.ordinal - b.ordinal),
          };

    case 'speakerConfirmed':
      return {
        ...m,
        speakers: m.speakers.map((s) =>
          s.id === ev.speakerId ? { ...s, confirmedName: ev.name } : s,
        ),
      };

    case 'snapshotCreated':
      return m.snapshots.some((s) => s.version === ev.version)
        ? m
        : {
            ...m,
            snapshots: [
              ...m.snapshots,
              {
                version: ev.version,
                throughEventSeq: ev.throughEventSeq,
                meetingTimeMs: ev.meetingTimeMs,
                state: 'running',
              },
            ],
          };

    case 'generationCompleted':
      return {
        ...m,
        activeVersion: ev.version,
        snapshots: m.snapshots.map((s) =>
          s.version === ev.version ? { ...s, state: 'completed' } : s,
        ),
      };

    case 'generationFailed': {
      const exists = m.snapshots.some((s) => s.version === ev.version);
      return {
        ...m,
        snapshots: exists
          ? m.snapshots.map((s) =>
              s.version === ev.version ? { ...s, state: 'failed', reason: ev.reason } : s,
            )
          : [
              ...m.snapshots,
              {
                version: ev.version,
                throughEventSeq: 0,
                meetingTimeMs: batch.meetingTimeMs,
                state: 'failed',
                reason: ev.reason,
              },
            ],
        degrade: {
          title: `摘要 v${ev.version} 生成失敗`,
          body: `${ev.reason}。快照已保留，可以直接重試或改用其他 Provider，錄音與逐字稿不受影響。`,
          tone: 'warn',
        },
      };
    }

    case 'meetingStateChanged': {
      // 用事件自己的時間戳，不用批次邊界，暫停區間才不會被批次粒度抹平
      let pauses = m.pauses;
      if (ev.state === 'paused') {
        pauses = [...pauses, { fromMs: ev.meetingTimeMs, toMs: null }];
      } else if (pauses.length && pauses[pauses.length - 1].toMs === null) {
        pauses = pauses.slice();
        pauses[pauses.length - 1] = { ...pauses[pauses.length - 1], toMs: ev.meetingTimeMs };
      }
      return { ...m, state: ev.state, pauses };
    }

    case 'trackActivity':
      return {
        ...m,
        levels: { mic: ev.micLevel, system: ev.systemLevel },
        health: { mic: ev.micHealth, system: ev.systemHealth, stt: ev.sttHealth },
      };
  }
}

/**
 * 套用一個批次。
 *
 * 批次不銜接就代表中間有事件沒送到，這時停止套用增量並標記 desynced，
 * 由呼叫端改走 resync。帶著破洞繼續累加，畫面會安靜地變成錯的。
 */
export function applyBatch(m: MeetingModel, batch: SessionEventBatch): MeetingModel {
  if (batch.firstSeq !== null && batch.prevHighSeq !== m.appliedSeq) {
    // 重送已套用過的批次可以安全略過；真正的缺號才需要重新同步
    if (batch.lastSeq !== null && batch.lastSeq <= m.appliedSeq) return m;
    // 缺號與寫入失敗可以同時發生，後者不能被前者吃掉
    return { ...m, desynced: true, journalError: batch.journalError };
  }

  let next: MeetingModel = {
    ...m,
    journalError: batch.journalError,
    state: batch.state,
    meetingTimeMs: batch.meetingTimeMs,
    capturedAudioMs: batch.capturedAudioMs,
  };
  for (const ev of batch.events) {
    next = applyEvent(next, ev, batch);
  }
  if (batch.lastSeq !== null) next.appliedSeq = batch.lastSeq;
  return next;
}

/** 用完整投影取代目前狀態，清掉 desynced 旗標。 */
export function fromProjection(m: MeetingModel, p: SessionProjection): MeetingModel {
  return {
    ...m,
    state: p.state,
    meetingTimeMs: p.meetingTimeMs,
    capturedAudioMs: p.capturedAudioMs,
    segments: p.segments.map((s) => ({
      id: s.segmentId,
      speakerId: s.speakerId,
      text: s.text,
      meetingTimeMs: s.meetingTimeMs,
      revision: s.revision,
      origin: s.origin,
      stability: 'final' as const,
    })),
    speakers: p.speakers.map((s) => ({
      id: s.speakerId,
      ordinal: s.ordinal,
      track: s.track,
      proposedName: s.proposedName,
      confirmedName: s.confirmedName,
    })),
    notes: p.notes.map((n) => ({
      id: n.noteId,
      text: n.text,
      meetingTimeMs: n.meetingTimeMs,
      capturedAudioMs: n.capturedAudioMs,
    })),
    snapshots: p.snapshots.map((s) => ({
      version: s.version,
      throughEventSeq: s.throughEventSeq,
      meetingTimeMs: s.meetingTimeMs,
      state: s.state,
    })),
    pauses: p.pauses.map((x) => ({ fromMs: x.fromMs, toMs: x.toMs })),
    activeVersion:
      p.snapshots.filter((s) => s.state === 'completed').pop()?.version ?? m.activeVersion,
    appliedSeq: p.seq,
    desynced: false,
  };
}
