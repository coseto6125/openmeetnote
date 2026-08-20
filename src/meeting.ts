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
  /** 被併進了誰。合併不改寫片段，片段上留著的仍是當時聽到的 id。 */
  mergedInto: string | null;
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
  /** 生成期間 Provider 的最新一行輸出。只在 running 有意義，完成或失敗後就沒用了。 */
  progress?: string;
  /** 本輪要求。空字串代表使用者沒填，交給 AI 自行規劃。 */
  prompt: string;
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
  /** 這場已經落地幾段原音。0 代表沒有保存，事後無法驗證逐字稿。 */
  audioSegments: number;
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
  audioSegments: 0,
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
                mergedInto: null,
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

    // 那一列留在陣列裡，只是不再是一個人。片段上還帶著它的 id，移掉的話
    // 那幾句話會查不到是誰講的。
    case 'speakerMerged':
      return {
        ...m,
        speakers: m.speakers.map((s) =>
          s.id === ev.fromSpeakerId ? { ...s, mergedInto: ev.intoSpeakerId } : s,
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
                prompt: ev.prompt,
              },
            ],
          };

    case 'audioSegmentStored':
      // 只記段數。畫面要回答的是「原音有在存嗎」，不是「存了哪些檔案」。
      return { ...m, audioSegments: m.audioSegments + 1 };

    case 'generationProgress':
      // 只更新已知的那一版。進度是暫態的，不該讓它自己長出一個版本節點：
      // 真正建立版本的是 snapshotCreated。
      return m.snapshots.some((s) => s.version === ev.version)
        ? {
            ...m,
            snapshots: m.snapshots.map((s) =>
              s.version === ev.version ? { ...s, progress: ev.text } : s,
            ),
          }
        : m;

    case 'generationCompleted':
      return {
        ...m,
        activeVersion: ev.version,
        snapshots: m.snapshots.map((s) =>
          // 進度連同狀態一起收掉：完成之後那行字只會誤導
          s.version === ev.version ? { ...s, state: 'completed', progress: undefined } : s,
        ),
      };

    case 'generationFailed': {
      const exists = m.snapshots.some((s) => s.version === ev.version);
      return {
        ...m,
        snapshots: exists
          ? m.snapshots.map((s) =>
              s.version === ev.version
                ? { ...s, state: 'failed', reason: ev.reason, progress: undefined }
                : s,
            )
          : [
              ...m.snapshots,
              {
                version: ev.version,
                // 沒收到 snapshotCreated 就先收到失敗，這是重新同步時可能
                // 出現的順序。要求與游標都補不出來，留白比編一個好
                prompt: '',
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
/**
 * 聲紋分不出是誰時，遠端發言掛的識別碼（後端 `live.rs` 的 `UNKNOWN_REMOTE`）。
 *
 * 它不是一個人，是「遠端，但不確定是誰」，所以不參與「語者 N」的編號。
 */
export const UNKNOWN_REMOTE = 'remote';

/** `speakerDisplayName` 需要的欄位。錄音分頁與歷史分頁的語者形狀不同，取交集。 */
export interface NamedSpeaker {
  id: string;
  ordinal: number;
  track: 'mic' | 'system';
  proposedName: string | null;
  confirmedName: string | null;
  mergedInto: string | null;
}

/**
 * 順著 `mergedInto` 走到還是自己的那一位（後端 `store::resolve_merge` 的同一條
 * 規則）。合併只寫在語者那一列上，片段留著當時聽到的 id，所以每一條顯示路徑
 * 都要自己走這一步。
 *
 * 指向名單上沒有的人就停在原地：走不到的別名等於沒有別名，讓那一列照常有
 * 自己的名字，比讓它底下那幾句話變成「未指派」好。
 */
export function resolveMerge(all: NamedSpeaker[], id: string): string {
  let cur = id;
  // 圈數上限就是防呆。合併是使用者操作，A 併進 B 再 B 併進 A 只要兩次點擊；
  // 後端已經擋掉，這裡吊死的話整份逐字稿都畫不出來。
  for (let i = 0; i < all.length; i++) {
    const next = all.find((s) => s.id === cur)?.mergedInto;
    if (!next || !all.some((s) => s.id === next)) return cur;
    cur = next;
  }
  return cur;
}

/**
 * §8.2 的名稱優先順序：確認名 > 暫定名 > 依軌道與出現序的預設稱呼。
 *
 * 預設稱呼裡的編號只數遠端語者。ordinal 是全域的出現序，麥克風軌會佔掉一格，
 * 直接拿來顯示的話「我」之後的第一位遠端語者會叫「語者 3」，看起來像少了一個人。
 *
 * 放在這裡而不是各分頁自己一份：錄音分頁與歷史分頁顯示的是同一場會議的同一個
 * 人，兩份實作之間沒有邊界可以正當化差異。實際發生過的後果是歷史分頁顯示
 * 內部 id（`s2`），而同一句話在錄音分頁顯示「沈立群」。
 */
export function speakerDisplayName(s: NamedSpeaker, all: NamedSpeaker[]): string {
  // 排在確認名之前，不是之後。這個識別碼底下可能有好幾個人，讓其中一個名字
  // 蓋住全部就是把他們併成一位；後端已經擋掉命名，這裡擋的是既有資料裡
  // 已經被命名過的那些。
  // 被併掉的那一列不自己取名字，也不佔編號：它底下的片段從此算在合併
  // 對象頭上，稱呼由對象決定
  const root = resolveMerge(all, s.id);
  if (root !== s.id) {
    const into = all.find((x) => x.id === root);
    if (into) return speakerDisplayName(into, all);
  }
  if (s.id === UNKNOWN_REMOTE) return '遠端';
  if (s.confirmedName) return s.confirmedName;
  if (s.proposedName) return s.proposedName;
  // 軌道先驗只到「本機 vs 遠端」，不能再往「遠端只有一人」推（§8.1）
  if (s.track === 'mic') return '我';
  const nth = all.filter(
    (x) =>
      x.track === 'system' &&
      x.id !== UNKNOWN_REMOTE &&
      resolveMerge(all, x.id) === x.id &&
      x.ordinal <= s.ordinal,
  ).length;
  return `語者 ${nth}`;
}

export function fromProjection(m: MeetingModel, p: SessionProjection): MeetingModel {
  return {
    ...m,
    // 每個列表欄位都從投影重建，計數也一樣：留著舊值等於 resync 修不了它
    audioSegments: p.audioSegments,
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
      mergedInto: s.mergedInto,
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
      prompt: s.prompt,
    })),
    pauses: p.pauses.map((x) => ({ fromMs: x.fromMs, toMs: x.toMs })),
    activeVersion:
      p.snapshots.filter((s) => s.state === 'completed').pop()?.version ?? m.activeVersion,
    appliedSeq: p.seq,
    desynced: false,
  };
}
