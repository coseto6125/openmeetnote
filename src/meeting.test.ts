import { describe, expect, test } from 'vitest';
import {
  applyBatch,
  emptyMeeting,
  fromProjection,
  speakerDisplayName,
  UNKNOWN_REMOTE,
  type MeetingModel,
} from './meeting';
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

const audioStored = (seq: number, track: 'mic' | 'system' = 'mic'): SessionEvent => ({
  kind: 'audioSegmentStored',
  seq,
  track,
  capturedStartMs: 0,
  capturedEndMs: 60_000,
});

const progress = (version: number, text: string): SessionEvent => ({
  kind: 'generationProgress',
  version,
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
      speakers: [
        {
          speakerId: 's1',
          ordinal: 1,
          proposedName: null,
          confirmedName: '小明',
          track: 'system',
          mergedInto: null,
        },
      ],
      notes: [{ noteId: 1, text: '筆記', meetingTimeMs: 800, capturedAudioMs: 800 }],
      snapshots: [
        { version: 1, throughEventSeq: 4, meetingTimeMs: 3000, state: 'completed', prompt: '' },
      ],
      pauses: [{ fromMs: 1000, toMs: 2000 }],
      audioSegments: 0,
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

  // 那一列留在陣列裡，只是不再是一個人。移掉的話片段上帶著的舊 id 就查
  // 不到人，畫面上那幾句話變成沒有名字。
  test('a_merged_speaker_stays_in_the_list_and_records_where_it_went', () => {
    let m = applyBatch(emptyMeeting, batchOf([proposed(1, 's1', 1, 'system')], 0));
    m = applyBatch(m, batchOf([proposed(2, 's2', 2, 'system')], 1));
    m = applyBatch(
      m,
      batchOf([{ kind: 'speakerMerged', seq: 3, fromSpeakerId: 's1', intoSpeakerId: 's2' }], 2),
    );
    expect(m.speakers).toHaveLength(2);
    expect(m.speakers.find((x) => x.id === 's1')?.mergedInto).toBe('s2');
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

  test('progress_shows_on_the_running_version_and_the_latest_line_wins', () => {
    // 一輪最多十分鐘，只顯示「生成中」使用者分不出在跑還是卡死
    let m = applyBatch(emptyMeeting, batchOf([snapshot(1, 1)], 0));
    m = applyBatch(m, batchOf([progress(1, '讀取證據')], 1));
    expect(m.snapshots[0].progress).toBe('讀取證據');

    m = applyBatch(m, batchOf([progress(1, 'tokens used 5,580')], 1));
    expect(m.snapshots[0].progress).toBe('tokens used 5,580');
    // 進度是暫態的，不帶 seq，不該推進已套用的事件序列
    expect(m.appliedSeq).toBe(1);
  });

  test('progress_for_an_unknown_version_does_not_invent_a_snapshot', () => {
    // 建立版本的是 snapshotCreated。進度先到就長出一個空節點的話，
    // 畫面上會多一個沒有涵蓋範圍、點不開的版本
    const m = applyBatch(emptyMeeting, batchOf([progress(9, '讀取證據')], 0));
    expect(m.snapshots).toHaveLength(0);
  });

  test('progress_is_cleared_once_the_round_settles', () => {
    // 完成或失敗之後那行過期的進度只會誤導
    let m = applyBatch(emptyMeeting, batchOf([snapshot(1, 1)], 0));
    m = applyBatch(m, batchOf([progress(1, '正在生成區塊')], 1));
    m = applyBatch(m, batchOf([completed(2, 1)], 1));
    expect(m.snapshots[0].progress).toBeUndefined();

    let n = applyBatch(emptyMeeting, batchOf([snapshot(1, 2)], 0));
    n = applyBatch(n, batchOf([progress(2, '正在生成區塊')], 1));
    n = applyBatch(n, batchOf([failed(2, 2, '生成逾時（600 秒）')], 1));
    expect(n.snapshots[0].progress).toBeUndefined();
    expect(n.snapshots[0].reason).toBe('生成逾時（600 秒）');
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

describe('原音保存', () => {
  test('a_stored_audio_segment_counts_without_breaking_seq_continuity', () => {
    // 音訊落地是決定性事件、佔一個 seq。不送到 UI 的話前端會把那個 seq
    // 當成缺號而要求重新同步，畫面上就是無故閃一次「內容不可信」。
    let m = applyBatch(emptyMeeting, batchOf([audioStored(1)], 0));
    expect(m.audioSegments).toBe(1);
    expect(m.desynced).toBe(false);
    expect(m.appliedSeq).toBe(1);

    m = applyBatch(m, batchOf([audioStored(2, 'system')], 1));
    expect(m.audioSegments).toBe(2);
    expect(m.desynced).toBe(false);
  });

  test('resync_corrects_the_audio_count_it_does_not_inherit_a_stale_one', () => {
    // 缺號後重新同步的目的就是修正計數。沿用舊值的話，被漏掉的那幾段
    // 永遠補不回來，而畫面會宣稱原音比實際少。
    let m = applyBatch(emptyMeeting, batchOf([audioStored(1)], 0));
    expect(m.audioSegments).toBe(1);
    const p: SessionProjection = {
      state: 'recording',
      seq: 9,
      meetingTimeMs: 5000,
      capturedAudioMs: 5000,
      segments: [],
      speakers: [],
      notes: [],
      snapshots: [],
      pauses: [],
      audioSegments: 4,
    };
    m = fromProjection(m, p);
    expect(m.audioSegments).toBe(4);
  });

  test('no_audio_events_means_the_transcript_cannot_be_verified_later', () => {
    // 0 是有意義的狀態，不是「還沒收到」：關掉保存或寫入失敗時，
    // 逐字稿就是唯一紀錄
    const m = applyBatch(emptyMeeting, batchOf([finalized(1, 1, '開始討論')], 0));
    expect(m.audioSegments).toBe(0);
  });
});

describe('speaker display name', () => {
  const s = (
    ordinal: number,
    track: 'mic' | 'system',
    proposed = null,
    confirmed = null,
    id = `s${ordinal}`,
  ) => ({
    id,
    ordinal,
    track,
    proposedName: proposed as string | null,
    confirmedName: confirmed as string | null,
    mergedInto: null as string | null,
  });

  test('confirmed_name_wins_over_proposed', () => {
    const all = [s(1, 'system', '王小明' as never, '陳大文' as never)];
    expect(speakerDisplayName(all[0], all)).toBe('陳大文');
  });

  test('proposed_name_is_used_when_nothing_is_confirmed', () => {
    const all = [s(1, 'system', '王小明' as never)];
    expect(speakerDisplayName(all[0], all)).toBe('王小明');
  });

  test('the_mic_track_is_always_me', () => {
    const all = [s(1, 'mic')];
    expect(speakerDisplayName(all[0], all)).toBe('我');
  });

  // ordinal 是全域出現序，麥克風軌佔掉第一格。照它編號的話第一位遠端語者
  // 會叫「語者 2」，看起來像有一個人不見了。
  test('default_numbering_skips_the_mic_track', () => {
    const all = [s(1, 'mic'), s(2, 'system'), s(3, 'system')];
    expect(all.map((x) => speakerDisplayName(x, all))).toEqual(['我', '語者 1', '語者 2']);
  });

  // 聲紋分不出是誰的那些話掛在 UNKNOWN_REMOTE 底下。叫它「語者 1」的話它會
  // 看起來像一個人，然後被命名成某人 —— 話就被安到那個人頭上了。
  test('the_unidentified_remote_speaker_is_not_numbered', () => {
    const all = [s(1, 'mic'), s(2, 'system', null, null, UNKNOWN_REMOTE), s(3, 'system')];
    expect(all.map((x) => speakerDisplayName(x, all))).toEqual(['我', '遠端', '語者 1']);
  });

  // 歷史分頁曾經直接印內部 id，同一句話在錄音分頁是「沈立群」、在歷史分頁是
  // 「s2」。兩邊現在共用這個函式，這條測試守的是那個規則本身。
  // 後端已經拒絕命名這個識別碼，這條守的是既有資料：舊版本存進去的名字
  // 不能在畫面上生效。那底下是好幾位分不出來的人，讓其中一個名字蓋住全部
  // 就是把他們併成一位。
  test('a_name_stored_on_the_unidentified_remote_speaker_does_not_take_effect', () => {
    const all = [s(1, 'system', null, 'Alice' as never, UNKNOWN_REMOTE), s(2, 'system')];
    expect(all.map((x) => speakerDisplayName(x, all))).toEqual(['遠端', '語者 1']);
  });

  // 聲紋比對寧可錯拆也不錯併（§8.1），所以同一個人會多出一列。合併把它改
  // 回來，但不改寫片段：片段上留著的仍是當時聽到的 id，查不到名字的話那
  // 幾句話會變成「未指派」。
  test('a_merged_speaker_answers_with_the_name_of_whoever_absorbed_it', () => {
    const all = [s(1, 'system'), s(2, 'system', null, '沈立群' as never)];
    all[0] = { ...all[0], mergedInto: 's2' };
    expect(all.map((x) => speakerDisplayName(x, all))).toEqual(['沈立群', '沈立群']);
  });

  // 佔了編號的話，同一場會議看起來就多一個人
  test('a_merged_speaker_does_not_consume_a_number', () => {
    const all = [s(1, 'system'), s(2, 'system'), s(3, 'system')];
    all[0] = { ...all[0], mergedInto: 's2' };
    expect(all.map((x) => speakerDisplayName(x, all))).toEqual(['語者 1', '語者 1', '語者 2']);
  });

  // A 併進 B 再 B 併進 A 只要兩次點擊。後端擋掉了，但畫面吃得到別的來源
  // 寫進來的資料 —— 這裡吊死的話整份逐字稿都畫不出來。
  test('a_cycle_between_two_aliases_does_not_hang_the_transcript', () => {
    const all = [s(1, 'system'), s(2, 'system')];
    all[0] = { ...all[0], mergedInto: 's2' };
    all[1] = { ...all[1], mergedInto: 's1' };
    expect(() => all.map((x) => speakerDisplayName(x, all))).not.toThrow();
  });

  // 別名指向名單上沒有的人時，那一列照常有自己的名字。整份逐字稿因為一個
  // 走不到的別名而變成「未指派」，比名字不理想糟得多。
  test('an_alias_pointing_nowhere_leaves_the_row_with_its_own_name', () => {
    const all = [s(1, 'system'), s(2, 'system')];
    all[0] = { ...all[0], mergedInto: '不存在的人' };
    expect(all.map((x) => speakerDisplayName(x, all))).toEqual(['語者 1', '語者 2']);
  });

  test('numbering_is_stable_when_a_remote_speaker_gets_a_name', () => {
    const all = [s(1, 'mic'), s(2, 'system', null, '沈立群' as never), s(3, 'system')];
    expect(all.map((x) => speakerDisplayName(x, all))).toEqual(['我', '沈立群', '語者 2']);
  });
});
