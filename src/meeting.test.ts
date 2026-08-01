import { describe, expect, test } from 'vitest';
import { applyBatch, emptyMeeting, fromProjection, type MeetingModel } from './meeting';
import type { SessionEvent, SessionEventBatch, SessionProjection } from './session';

/** 依序組批次，prevHighSeq 自動銜接，模擬正常的事件流。 */
function batchOf(events: SessionEvent[], prevHighSeq: number, meetingTimeMs = 1000): SessionEventBatch {
  const seqs = events
    .map((e) => ('seq' in e ? e.seq : null))
    .filter((s): s is number => s !== null);
  return {
    firstSeq: seqs[0] ?? null,
    lastSeq: seqs[seqs.length - 1] ?? null,
    prevHighSeq,
    emittedAtMs: 0,
    meetingTimeMs,
    capturedAudioMs: meetingTimeMs,
    state: 'recording',
    journalError: null,
    events,
  };
}

const finalized = (seq: number, id: number, text: string, revision = 1): SessionEvent => ({
  kind: 'transcriptFinalized',
  seq,
  segmentId: id,
  revision,
  origin: 'provider',
  speakerId: 's1',
  text,
  meetingTimeMs: 500,
  capturedAudioMs: 500,
});

const partial = (id: number, text: string): SessionEvent => ({
  kind: 'transcriptPartial',
  segmentId: id,
  speakerId: 's1',
  text,
  meetingTimeMs: 500,
});

const edited = (seq: number, id: number, text: string, revision: number): SessionEvent => ({
  kind: 'transcriptEdited',
  seq,
  segmentId: id,
  revision,
  origin: 'user',
  text,
});

const note = (seq: number, noteId: number, text: string): SessionEvent => ({
  kind: 'noteAdded',
  seq,
  noteId,
  text,
  meetingTimeMs: 800,
  capturedAudioMs: 800,
});

describe('transcript precedence', () => {
  test('late_partial_cannot_rewrite_a_finalized_segment', () => {
    let m = applyBatch(emptyMeeting, batchOf([finalized(1, 10, '定稿內容')], 0));
    m = applyBatch(m, batchOf([partial(10, '遲到的 partial')], 1));
    expect(m.segments[0].text).toBe('定稿內容');
    expect(m.segments[0].stability).toBe('final');
  });

  test('provider_final_cannot_overwrite_a_user_edit', () => {
    let m = applyBatch(emptyMeeting, batchOf([finalized(1, 10, '原始內容')], 0));
    m = applyBatch(m, batchOf([edited(2, 10, '使用者改過', 2)], 1));
    // Provider 重連後重送 r1，不得蓋掉 r2
    m = applyBatch(m, batchOf([finalized(3, 10, '重連後的結果')], 2));
    expect(m.segments[0].text).toBe('使用者改過');
    expect(m.segments[0].origin).toBe('user');
    expect(m.segments[0].revision).toBe(2);
  });

  test('lower_revision_edit_is_ignored', () => {
    let m = applyBatch(emptyMeeting, batchOf([finalized(1, 10, '原始')], 0));
    m = applyBatch(m, batchOf([edited(2, 10, '第二版', 2)], 1));
    m = applyBatch(m, batchOf([edited(3, 10, '倒退的第一版', 1)], 2));
    expect(m.segments[0].text).toBe('第二版');
  });

  test('edit_for_unknown_segment_is_dropped', () => {
    const m = applyBatch(emptyMeeting, batchOf([edited(1, 999, '不存在', 2)], 0));
    expect(m.segments).toHaveLength(0);
  });
});

describe('decisive event dedup', () => {
  test('replayed_note_does_not_create_a_second_entry', () => {
    let m = applyBatch(emptyMeeting, batchOf([note(1, 1, '一筆')], 0));
    // 同一批被重送：prevHighSeq 相同，lastSeq 不超過已套用的 seq
    m = applyBatch(m, batchOf([note(1, 1, '一筆')], 0));
    expect(m.notes).toHaveLength(1);
  });
});

describe('sequence gap detection', () => {
  test('gap_marks_the_model_desynced_instead_of_applying', () => {
    let m = applyBatch(emptyMeeting, batchOf([note(1, 1, '第一筆')], 0));
    expect(m.desynced).toBe(false);
    // seq 2 沒送到，直接跳到 3
    m = applyBatch(m, batchOf([note(3, 2, '第三筆')], 2));
    expect(m.desynced).toBe(true);
    expect(m.notes).toHaveLength(1);
  });

  test('resync_from_projection_clears_the_flag', () => {
    let m: MeetingModel = { ...emptyMeeting, desynced: true };
    const projection: SessionProjection = {
      state: 'recording',
      seq: 9,
      meetingTimeMs: 5000,
      capturedAudioMs: 4000,
      segments: [
        {
          segmentId: 10,
          revision: 2,
          origin: 'user',
          speakerId: 's1',
          text: '重新同步後的內容',
          meetingTimeMs: 500,
        },
      ],
      notes: [{ noteId: 1, text: '筆記', meetingTimeMs: 800, capturedAudioMs: 800 }],
      snapshots: [
        { version: 1, throughEventSeq: 4, meetingTimeMs: 3000, state: 'completed' },
      ],
      pauses: [{ fromMs: 1000, toMs: 2000 }],
    };
    m = fromProjection(m, projection);
    expect(m.desynced).toBe(false);
    expect(m.appliedSeq).toBe(9);
    expect(m.segments[0].text).toBe('重新同步後的內容');
    expect(m.activeVersion).toBe(1);
  });
});

describe('pause intervals', () => {
  test('pause_uses_event_timestamps_not_batch_boundaries', () => {
    const pauseAt: SessionEvent = {
      kind: 'meetingStateChanged',
      seq: 1,
      state: 'paused',
      meetingTimeMs: 1234,
      capturedAudioMs: 1234,
    };
    const resumeAt: SessionEvent = {
      kind: 'meetingStateChanged',
      seq: 2,
      state: 'recording',
      meetingTimeMs: 5678,
      capturedAudioMs: 1234,
    };
    let m = applyBatch(emptyMeeting, batchOf([pauseAt], 0, 9999));
    m = applyBatch(m, batchOf([resumeAt], 1, 9999));
    expect(m.pauses).toEqual([{ fromMs: 1234, toMs: 5678 }]);
  });

  test('stopping_while_paused_closes_the_interval', () => {
    const pauseAt: SessionEvent = {
      kind: 'meetingStateChanged',
      seq: 1,
      state: 'paused',
      meetingTimeMs: 100,
      capturedAudioMs: 100,
    };
    const stopAt: SessionEvent = {
      kind: 'meetingStateChanged',
      seq: 2,
      state: 'stopping',
      meetingTimeMs: 400,
      capturedAudioMs: 100,
    };
    let m = applyBatch(emptyMeeting, batchOf([pauseAt], 0));
    m = applyBatch(m, batchOf([stopAt], 1));
    expect(m.pauses[0].toMs).toBe(400);
  });
});

describe('local write failures', () => {
  test('a_journal_error_reaches_the_model_even_with_no_events', () => {
    const batch = { ...batchOf([], 0), journalError: '磁碟已滿' };
    expect(applyBatch(emptyMeeting, batch).journalError).toBe('磁碟已滿');
  });

  test('a_journal_error_survives_a_desync', () => {
    let m = applyBatch(emptyMeeting, batchOf([note(1, 1, '一')], 0));
    m = applyBatch(m, { ...batchOf([note(3, 2, '三')], 2), journalError: '磁碟已滿' });
    expect(m.desynced).toBe(true);
    expect(m.journalError).toBe('磁碟已滿');
  });
});
