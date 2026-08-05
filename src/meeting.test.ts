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


const snapshot = (
  seq: number,
  version: number,
  throughEventSeq = 10,
  prompt = '',
): SessionEvent => ({
  kind: 'snapshotCreated',
  seq,
  version,
  prompt,
  throughEventSeq,
  meetingTimeMs: 900,
});

const completed = (seq: number, version: number): SessionEvent => ({
  kind: 'generationCompleted',
  seq,
  version,
});

const failed = (seq: number, version: number, reason: string): SessionEvent => ({
  kind: 'generationFailed',
  seq,
  version,
  reason,
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
      speakers: [
        {
          speakerId: 's1',
          ordinal: 1,
          proposedName: null,
          confirmedName: '小明',
          track: 'system',
        },
      ],
      notes: [{ noteId: 1, text: '筆記', meetingTimeMs: 800, capturedAudioMs: 800 }],
      snapshots: [
        { version: 1, throughEventSeq: 4, meetingTimeMs: 3000, state: 'completed', prompt: '' },
      ],
      pauses: [{ fromMs: 1000, toMs: 2000 }],
    };
    m = fromProjection(m, projection);
    expect(m.desynced).toBe(false);
    expect(m.appliedSeq).toBe(9);
    expect(m.segments[0].text).toBe('重新同步後的內容');
    expect(m.activeVersion).toBe(1);
    expect(m.speakers[0].confirmedName).toBe('小明');
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

describe('speakers', () => {
  const proposed = (seq: number, id: string, ordinal: number, track: 'mic' | 'system'): SessionEvent => ({
    kind: 'speakerProposed',
    seq,
    speakerId: id,
    ordinal,
    proposedName: null,
    track,
  });

  test('a_speaker_appears_once_however_often_the_batch_is_replayed', () => {
    let m = applyBatch(emptyMeeting, batchOf([proposed(1, 's1', 1, 'system')], 0));
    m = applyBatch(m, batchOf([proposed(1, 's1', 1, 'system')], 0));
    expect(m.speakers).toHaveLength(1);
  });

  test('confirming_a_name_updates_the_speaker_rather_than_a_side_table', () => {
    let m = applyBatch(emptyMeeting, batchOf([proposed(1, 's1', 1, 'system')], 0));
    m = applyBatch(
      m,
      batchOf([{ kind: 'speakerConfirmed', seq: 2, speakerId: 's1', name: '小明' }], 1),
    );
    expect(m.speakers[0].confirmedName).toBe('小明');
  });

  test('speakers_stay_ordered_by_first_appearance', () => {
    let m = applyBatch(emptyMeeting, batchOf([proposed(1, 'b', 2, 'system')], 0));
    m = applyBatch(m, batchOf([proposed(2, 'a', 1, 'mic')], 1));
    expect(m.speakers.map((s) => s.id)).toEqual(['a', 'b']);
  });
});

describe('precedence is identical across all three layers', () => {
  test('a_higher_provider_revision_still_loses_to_a_user_edit', () => {
    let m = applyBatch(emptyMeeting, batchOf([finalized(1, 10, '原始')], 0));
    m = applyBatch(m, batchOf([edited(2, 10, '使用者改過', 2)], 1));
    // Provider 的 r3。版本號較大不代表它有權覆蓋（與 Rust 兩層同一條規則）
    m = applyBatch(m, batchOf([finalized(3, 10, 'Provider 的 r3', 3)], 2));
    expect(m.segments[0].text).toBe('使用者改過');
    expect(m.segments[0].revision).toBe(2);
  });

  test('a_later_user_edit_still_wins_over_an_earlier_one', () => {
    let m = applyBatch(emptyMeeting, batchOf([finalized(1, 10, '原始')], 0));
    m = applyBatch(m, batchOf([edited(2, 10, '第一次改', 2)], 1));
    m = applyBatch(m, batchOf([edited(3, 10, '第二次改', 3)], 2));
    expect(m.segments[0].text).toBe('第二次改');
  });

  test('an_equal_revision_between_two_provider_results_changes_nothing', () => {
    // Rust 兩層有同名測試。同版本互蓋只會改寫時間戳，但三層必須同時擋。
    let m = applyBatch(emptyMeeting, batchOf([finalized(1, 10, '第一次')], 0));
    m = applyBatch(m, batchOf([finalized(2, 10, '同版本重送', 1)], 1));
    expect(m.segments[0].text).toBe('第一次');
  });

  test('an_equal_revision_from_a_provider_does_not_displace_a_user_edit', () => {
    // 等版本號是三層規則裡最容易寫歪的一格，Rust 兩層各有一個同名測試
    let m = applyBatch(emptyMeeting, batchOf([finalized(1, 10, '原始')], 0));
    m = applyBatch(m, batchOf([edited(2, 10, '使用者改過', 2)], 1));
    m = applyBatch(m, batchOf([finalized(3, 10, 'Provider 的同版本', 2)], 2));
    expect(m.segments[0].text).toBe('使用者改過');
  });
});

describe('生成失敗', () => {
  // 使用者最常遇到的錯誤路徑：CLI 沒登入、額度用盡、模型回了解不開的東西。
  // 這些都必須在畫面上看得見並且可以重試，靜默失敗會讓人以為摘要還在跑。

  test('a_snapshot_remembers_what_this_round_asked_for', () => {
    // 沒有這個欄位，多個版本在畫面上看起來一模一樣，
    // 使用者無從知道哪一版問的是什麼，也無從沿用它重試
    const m = applyBatch(
      emptyMeeting,
      batchOf([snapshot(1, 1, 10, '整理成決議清單')], 0),
    );
    expect(m.snapshots[0].prompt).toBe('整理成決議清單');
  });

  test('a_failed_generation_keeps_the_snapshot_and_shows_why', () => {
    let m = applyBatch(emptyMeeting, batchOf([snapshot(1, 1)], 0));
    m = applyBatch(m, batchOf([failed(2, 1, 'CLI 尚未登入')], 1));

    expect(m.snapshots).toHaveLength(1);
    expect(m.snapshots[0].state).toBe('failed');
    expect(m.snapshots[0].reason).toBe('CLI 尚未登入');
    // 快照本身不該消失：使用者要能重試同一個涵蓋範圍
    expect(m.snapshots[0].version).toBe(1);
  });

  test('a_failure_without_a_prior_snapshot_still_surfaces', () => {
    // 生成可能在快照事件送達 UI 之前就失敗（背景工作比事件泵快）
    const m = applyBatch(emptyMeeting, batchOf([failed(1, 3, '生成逾時')], 0));
    expect(m.snapshots).toHaveLength(1);
    expect(m.snapshots[0].state).toBe('failed');
    expect(m.snapshots[0].reason).toBe('生成逾時');
  });

  test('a_failed_version_never_becomes_the_active_one', () => {
    // 失敗的版本被設成 active 的話，畫面會顯示一份不存在的成果
    let m = applyBatch(emptyMeeting, batchOf([snapshot(1, 1), completed(2, 1)], 0));
    expect(m.activeVersion).toBe(1);

    m = applyBatch(m, batchOf([snapshot(3, 2), failed(4, 2, '模型回了散文')], 2));
    expect(m.activeVersion).toBe(1);
    expect(m.snapshots.find((s) => s.version === 2)?.state).toBe('failed');
  });

  test('a_replayed_failure_does_not_duplicate_the_snapshot', () => {
    // 重連之後同一個事件會再送一次，重播必須是冪等的
    let m = applyBatch(emptyMeeting, batchOf([snapshot(1, 1), failed(2, 1, '逾時')], 0));
    m = applyBatch(m, batchOf([snapshot(1, 1), failed(2, 1, '逾時')], 0));
    expect(m.snapshots).toHaveLength(1);
  });

  test('a_retry_after_failure_can_reach_completed', () => {
    // 失敗不是終局：重試成功之後狀態要能翻回來
    let m = applyBatch(emptyMeeting, batchOf([snapshot(1, 1), failed(2, 1, '逾時')], 0));
    m = applyBatch(m, batchOf([completed(3, 1)], 2));

    expect(m.snapshots[0].state).toBe('completed');
    expect(m.activeVersion).toBe(1);
  });
});
