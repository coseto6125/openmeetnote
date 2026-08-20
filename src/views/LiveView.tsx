/**
 * 錄音中的畫面。
 *
 * 只讀 model，不直接訂閱事件：訂閱在 App，切到別的分頁時錄音與事件累積
 * 都不能中斷。這個元件被卸載也只是畫面消失，會議照常進行。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  activeMeeting,
  commands,
  hhmmss,
  mmss,
  snapshotDocument,
  type DocumentBlock,
} from '../session';
import { resolveMerge, speakerDisplayName, UNKNOWN_REMOTE } from '../meeting';
import type { Degrade, MeetingModel, Snapshot, Speaker } from '../meeting';
import { Spine, type SpinePause } from '../components/Spine';
import { DocumentView, type CitedSegment } from '../components/DocumentView';

/**
 * 語者的顯示樣式。
 *
 * 這裡只放顏色與稱呼規則，人是誰由後端的 speakers 決定。
 * 先前這是一個寫死的常數陣列，於是後端有一整張 speakers 表卻沒人填，
 * 語者確認也就無聲消失。
 */
const SPEAKER_COLORS = ['var(--violet)', '#3f7fa6', '#8a6d3b', '#4f7a5e', '#7a4f6d'];

const displayName = speakerDisplayName;

const colorOf = (s: Speaker) => SPEAKER_COLORS[(s.ordinal - 1) % SPEAKER_COLORS.length];

export interface LiveViewProps {
  model: MeetingModel;
  setModel: React.Dispatch<React.SetStateAction<MeetingModel>>;
  localDegrade: Degrade | null;
  setLocalDegrade: (d: Degrade | null) => void;
}

export function LiveView({ model, setModel, localDegrade, setLocalDegrade }: LiveViewProps) {
  const [rejectedSpeakers, setRejectedSpeakers] = useState<string[]>([]);
  const [noteDraft, setNoteDraft] = useState('');
  // 本輪 Prompt 決定這一版文件的方向（§17.6）。送出後清掉：下一版通常
  // 要問的是別的事，留著上一輪的要求會讓人以為它還在生效。
  const [promptDraft, setPromptDraft] = useState('');
  const [blocks, setBlocks] = useState<DocumentBlock[]>([]);
  // 逐字稿與摘要共用主欄。摘要值得整個欄寬，而兩者同時看得到並沒有用處：
  // 讀摘要的時候要的是引用能跳回去，那由引用自己負責切換。
  const [pane, setPane] = useState<'transcript' | 'document'>('transcript');
  const [naming, setNaming] = useState<string | null>(null);
  const [merging, setMerging] = useState<string | null>(null);
  // 受控輸入。非受控的話 blur 讀到的值取決於瀏覽器何時同步 DOM，
  // 而 HistoryView 的改名本來就是受控的，兩處不該有兩種寫法。
  const [nameDraft, setNameDraft] = useState('');
  const streamRef = useRef<HTMLDivElement>(null);
  const stickRef = useRef(true);

  /* ── 捲動只在使用者已在底部時跟隨 ───────────────────────── */

  useEffect(() => {
    // 逐字稿被 hidden 起來時 scrollHeight 是 0，這時候貼底等於捲到最上面。
    // 把 pane 放進相依項，切回來的那一次繪製會重新貼底。
    if (pane !== 'transcript') return;
    const el = streamRef.current;
    if (el && stickRef.current) el.scrollTop = el.scrollHeight;
  }, [model.segments, pane]);

  const onStreamScroll = () => {
    const el = streamRef.current;
    if (!el) return;
    stickRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  };

  /* ── 衍生資料 ─────────────────────────────────────────── */

  // 併掉的 id 也要問得到人。合併不改寫片段，片段上帶著的仍是當時聽到的那個
  // id，直接比對就查不到，畫面上的顏色與軌道會退回預設值。
  const speakerOf = useCallback(
    (id: string) => {
      const root = resolveMerge(model.speakers, id);
      return model.speakers.find((s) => s.id === root);
    },
    [model.speakers],
  );
  /** 還是一個人的那幾列。被併掉的留在 `model.speakers` 裡供查詢，但不上名單。 */
  const roster = useMemo(
    () => model.speakers.filter((s) => resolveMerge(model.speakers, s.id) === s.id),
    [model.speakers],
  );
  const nameOf = useCallback(
    (id: string) => {
      const s = speakerOf(id);
      return s ? displayName(s, model.speakers) : id;
    },
    [speakerOf, model.speakers],
  );

  const finalCount = model.segments.filter((s) => s.stability === 'final').length;
  const lastCovering = [...model.snapshots].reverse().find((s) => s.state !== 'failed') ?? null;
  const lagMs = Math.max(0, model.meetingTimeMs - (lastCovering?.meetingTimeMs ?? 0));
  const active = model.snapshots.find(
    (s) => s.version === model.activeVersion && s.state === 'completed',
  );
  // 新版本以最後一個成功的版本為基礎（§5.5）。使用者要知道自己是在改哪一版，
  // 否則「補上行動項目」這種要求看起來像是要重新生一份。
  const baseVersion = [...model.snapshots].reverse().find((s) => s.state === 'completed');
  const degrade = localDegrade ?? model.degrade;
  const live = model.state === 'recording' || model.state === 'paused';
  // 誰是誰通常散會後才確定得下來，所以命名的窗口比錄音本身長
  const canName = model.state !== 'idle' && model.state !== 'stopping';
  /// 結束之後仍可建立摘要：最常見的流程就是開完會才要，那時內容才完整。
  const canSnapshot = live || model.state === 'completed';

  // 暫定名稱來自 §8.3 的自我介紹推定，目前沒有生產者，因此這個清單通常是空的。
  // 保留這條路徑是為了讓 M3 接上時不必重寫確認流程。
  const pending = roster.filter(
    (s) => s.proposedName && !s.confirmedName && !rejectedSpeakers.includes(s.id),
  );

  const spineSegments = useMemo(
    () =>
      model.segments.map((s) => ({
        meetingTimeMs: s.meetingTimeMs,
        track: speakerOf(s.speakerId)?.track ?? 'system',
        final: s.stability === 'final',
      })),
    [model.segments, speakerOf],
  );

  const spinePauses: SpinePause[] = useMemo(
    () =>
      model.pauses.map((p) => ({ fromMs: p.fromMs, toMs: p.toMs ?? model.meetingTimeMs })),
    [model.pauses, model.meetingTimeMs],
  );

  const streamRows = useMemo(() => {
    const rows: Array<
      | { type: 'utt'; seg: (typeof model.segments)[number] }
      | { type: 'coverage'; snap: Snapshot }
      | { type: 'gap'; pause: SpinePause }
    > = [];
    const marks = [
      ...model.snapshots
        .filter((s) => s.state !== 'failed')
        .map((snap) => ({ at: snap.meetingTimeMs, node: { type: 'coverage' as const, snap } })),
      ...spinePauses.map((pause) => ({ at: pause.fromMs, node: { type: 'gap' as const, pause } })),
    ].sort((a, b) => a.at - b.at);

    let mi = 0;
    for (const seg of model.segments) {
      while (mi < marks.length && marks[mi].at <= seg.meetingTimeMs) {
        rows.push(marks[mi].node);
        mi += 1;
      }
      rows.push({ type: 'utt', seg });
    }
    while (mi < marks.length) {
      rows.push(marks[mi].node);
      mi += 1;
    }
    return rows;
  }, [model.segments, model.snapshots, spinePauses]);

  /* ── 操作 ─────────────────────────────────────────────── */

  /* ── 顯示的是真實生成結果，不是佔位文字 ─────────────────── */

  useEffect(() => {
    if (model.activeVersion === null) {
      setBlocks([]);
      return;
    }
    let alive = true;
    const version = model.activeVersion;
    activeMeeting()
      .then((id) => (id === null ? [] : snapshotDocument(id, version)))
      .then((b) => alive && setBlocks(b))
      .catch(() => alive && setBlocks([]));
    return () => {
      alive = false;
    };
  }, [model.activeVersion]);

  const submitSnapshot = () => {
    void commands.createSnapshot(promptDraft);
    setPromptDraft('');
  };

  /** 失敗的版本用同一段要求再跑一次。
   *
   * 這是新的一版而不是把那一版救回來：版本是一條線性歷史，重跑一次改寫
   * 舊版本會讓「v2 是什麼」有兩個答案。使用者要的其實是不必重打要求。
   */
  const retry = (prompt: string) => {
    void commands.createSnapshot(prompt);
  };

  const submitNote = async () => {
    const text = noteDraft.trim();
    if (!text) return;
    const r = await commands.addNote(text);
    if (r.accepted) setNoteDraft('');
    else if (r.note) setLocalDegrade({ title: '筆記未送出', body: r.note, tone: 'warn' });
  };

  /** 名單上有幾個人可以當合併對象。少於一個就沒有「併入」這個動作。 */
  const mergeTargets = roster.filter((s) => s.id !== UNKNOWN_REMOTE).length - 1;

  const submitMerge = async (from: string, into: string) => {
    setMerging(null);
    if (!into) return;
    const r = await commands.mergeSpeaker(from, into);
    if (!r.accepted && r.note) {
      setLocalDegrade({ title: '語者未合併', body: r.note, tone: 'warn' });
    }
  };

  const submitName = async (speakerId: string, raw: string) => {
    setNaming(null);
    const name = raw.trim();
    if (!name) return;
    const r = await commands.confirmSpeaker(speakerId, name);
    if (!r.accepted && r.note) {
      setLocalDegrade({ title: '語者名稱未儲存', body: r.note, tone: 'warn' });
    }
  };

  const scrollToSegment = (id: string) => {
    stickRef.current = false;
    // 切換分頁之後 DOM 還沒換，捲動要等這一輪繪製完
    requestAnimationFrame(() =>
      document.getElementById(`seg-${id}`)?.scrollIntoView({ block: 'center', behavior: 'smooth' }),
    );
  };

  const seek = (ms: number) => {
    const target =
      model.segments.find((s) => s.meetingTimeMs >= ms) ??
      model.segments[model.segments.length - 1];
    if (!target) return;
    setPane('transcript');
    scrollToSegment(String(target.id));
  };

  /** 點引用就跳回被引用的那一段逐字稿，這是引用唯一的用途。 */
  const followCite = (sourceId: string) => {
    setPane('transcript');
    scrollToSegment(sourceId);
  };

  const citedSegments: CitedSegment[] = useMemo(
    () =>
      model.segments.map((s) => ({
        id: String(s.id),
        meetingTimeMs: s.meetingTimeMs,
        revision: s.revision,
      })),
    [model.segments],
  );

  return (
    <>
      <div className="shell">
        <nav className="spine" aria-label="錄音時間軸">
          <div className="spine-cap">時間軸</div>
          <Spine
            meetingTimeMs={model.meetingTimeMs}
            segments={spineSegments}
            snapshots={model.snapshots}
            notes={model.notes}
            pauses={spinePauses}
            activeVersion={model.activeVersion}
            onSeek={seek}
          />
          <div className="spine-foot">
            <span><i style={{ background: 'var(--live-soft)' }} />已進摘要</span>
            <span><i className="hatch" />未涵蓋 <b className="num">{mmss(lagMs)}</b></span>
          </div>
        </nav>

        <main className="stream-wrap">
          <div className="stream-head">
            <div className="pane-switch" role="tablist" aria-label="主欄內容">
              <button
                role="tab"
                aria-selected={pane === 'transcript'}
                onClick={() => setPane('transcript')}
              >
                逐字稿
                <span className="count num">{finalCount}</span>
              </button>
              <button
                role="tab"
                aria-selected={pane === 'document'}
                onClick={() => setPane('document')}
                disabled={!active}
                title={active ? undefined : '還沒有完成的摘要版本'}
              >
                摘要
                {active && <span className="count num">v{active.version}</span>}
              </button>
            </div>
            <span className="spacer" />
            <span className="lag">
              錄音 <span className="num">{mmss(model.capturedAudioMs)}</span>
              {model.meetingTimeMs - model.capturedAudioMs > 1500 && (
                <> ・暫停 <span className="num">{mmss(model.meetingTimeMs - model.capturedAudioMs)}</span></>
              )}
            </span>
          </div>

          {pane === 'document' && (
            <div className="stream doc-pane" tabIndex={0}>
              {active && (
                <p className="doc-meta num">
                  v{active.version}・涵蓋至 seq {active.throughEventSeq}・
                  {mmss(active.meetingTimeMs)}
                  {lagMs > 1500 && <>・之後還有 {mmss(lagMs)} 未涵蓋</>}
                </p>
              )}
              {blocks.length === 0 ? (
                <p className="empty">這個版本沒有通過驗證的區塊。</p>
              ) : (
                <DocumentView blocks={blocks} segments={citedSegments} onCite={followCite} />
              )}
            </div>
          )}

          <div
            className="stream"
            ref={streamRef}
            onScroll={onStreamScroll}
            tabIndex={0}
            hidden={pane !== 'transcript'}
          >
            {streamRows.map((row, i) => {
              if (row.type === 'coverage') {
                return (
                  <div className="coverage-rule" key={`c${row.snap.version}`}>
                    <span className="coverage-chip num">
                      v{row.snap.version} 涵蓋至 seq {row.snap.throughEventSeq}
                    </span>
                  </div>
                );
              }
              if (row.type === 'gap') {
                return (
                  <div className="gap-mark" key={`g${i}`}>
                    暫停 {mmss(row.pause.toMs - row.pause.fromMs)}，此區間沒有錄音
                  </div>
                );
              }
              const seg = row.seg;
              const meta = speakerOf(seg.speakerId);
              return (
                <article
                  className="utt"
                  key={seg.id}
                  id={`seg-${seg.id}`}
                  data-stability={seg.stability}
                  data-edited={seg.origin === 'user' ? '1' : '0'}
                >
                  <span className="utt-time num">{mmss(seg.meetingTimeMs)}</span>
                  <div className="utt-body">
                    <span className="utt-who">
                      <span
                        className="who-swatch"
                        style={{ background: meta ? colorOf(meta) : 'var(--muted)' }}
                      />
                      {nameOf(seg.speakerId)}
                    </span>
                    <span className="utt-text">{seg.text}</span>
                    {seg.origin === 'user' && (
                      <span className="edited-tag">已修訂 r{seg.revision}</span>
                    )}
                  </div>
                </article>
              );
            })}
            {model.segments.length === 0 && (
              <p className="empty">等待第一段語音。開始說話之後，逐字稿會出現在這裡。</p>
            )}
          </div>

          <div className="dock">
            <label className="note-field">
              <span className="sr-only">新增時間戳筆記</span>
              <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="var(--muted)" strokeWidth="1.5" strokeLinecap="round" aria-hidden="true">
                <path d="M2.5 13.5h11M4 11l7.2-7.2a1.6 1.6 0 0 1 2.3 2.3L6.3 13.3l-3 .7z" />
              </svg>
              <input
                value={noteDraft}
                onChange={(e) => setNoteDraft(e.target.value)}
                onKeyDown={(e) => e.key === 'Enter' && submitNote()}
                placeholder="記一筆，Enter 送出"
                autoComplete="off"
                disabled={!live}
              />
              <span className="note-stamp num">將標記於 {hhmmss(model.meetingTimeMs)}</span>
            </label>
            <button className="btn" onClick={submitNote} disabled={!live || !noteDraft.trim()}>
              記下
            </button>
            <label className="note-field prompt-field">
              {/* placeholder 不是名稱：螢幕閱讀器唸不到它，而這個欄位在
                  accessibility tree 上會變成一個沒有標籤的「編輯」。 */}
              <span className="sr-only">本輪要求</span>
              <input
                aria-label="本輪要求"
                value={promptDraft}
                onChange={(e) => setPromptDraft(e.target.value)}
                onKeyDown={(e) => e.key === 'Enter' && submitSnapshot()}
                placeholder={
                  baseVersion
                    ? `要改什麼？會在 v${baseVersion.version} 的基礎上修訂`
                    : '這一版想要什麼？留空由 AI 自行規劃'
                }
                autoComplete="off"
                disabled={!canSnapshot}
              />
            </label>
            {/* 結束之後仍可建立：最常見的流程就是開完會才要摘要 */}
            <button className="btn btn-primary" onClick={submitSnapshot} disabled={!canSnapshot}>
              {baseVersion ? `修訂為 v${baseVersion.version + 1}` : '建立摘要快照'}
            </button>
          </div>
        </main>

        <aside className="aside">
          {model.journalError && (
            <section className="card">
              <div className="banner" data-tone="bad" role="alert">
                <span>
                  <b>內容無法寫入本機資料庫</b>
                  <p>
                    {model.journalError}
                    。畫面上的內容不保證有被保存，請先停止錄音並確認磁碟空間，
                    重新啟動應用程式之後才會恢復寫入。
                  </p>
                </span>
              </div>
            </section>
          )}

{model.desynced && (
            <section className="card">
              <div className="banner" data-tone="bad" role="status">
                <span>
                  <b>正在與核心重新同步</b>
                  <p>偵測到事件缺號，畫面暫時可能不完整。錄音與逐字稿仍在本機寫入。</p>
                </span>
              </div>
            </section>
          )}

          {degrade && (
            <section className="card">
              <div className="banner" data-tone={degrade.tone} role="status">
                <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
                  <path d="M8 5.5v3.2M8 11.2v.1" strokeLinecap="round" />
                  <circle cx="8" cy="8" r="6.2" />
                </svg>
                <span>
                  <b>{degrade.title}</b>
                  <p>{degrade.body}</p>
                </span>
                <button
                  className="btn-ghost close"
                  onClick={() => {
                    setLocalDegrade(null);
                    setModel((m) => ({ ...m, degrade: null }));
                  }}
                  aria-label="關閉提示"
                >
                  ×
                </button>
              </div>
            </section>
          )}

          <section className="card">
            <div className="card-head">
              <span className="card-title">語者</span>
              <span className="count num">{roster.length}</span>
              {pending.length > 0 && <span className="spk-pending">{pending.length} 待確認</span>}
            </div>
            {roster.length === 0 && (
              <p className="hint">還沒有人發言。語者會在第一次聽到聲音時出現。</p>
            )}
            {roster.map((s) => {
              const isPending = pending.some((p) => p.id === s.id);
              return (
                <div className="spk-row" key={s.id}>
                  <span className="spk-name">
                    <span className="who-swatch" style={{ background: colorOf(s) }} />
                    {isPending ? s.proposedName : displayName(s, model.speakers)}
                    {isPending ? (
                      <span className="spk-pending">待確認</span>
                    ) : (
                      <em>{s.track === 'mic' ? '麥克風軌' : '系統音訊軌'}</em>
                    )}
                  </span>
                  {isPending && (
                    <>
                      <button
                        className="mini mini-yes"
                        onClick={() => void commands.confirmSpeaker(s.id, s.proposedName!)}
                      >
                        是
                      </button>
                      <button
                        className="mini"
                        onClick={() => setRejectedSpeakers((prev) => [...prev, s.id])}
                      >
                        不是
                      </button>
                    </>
                  )}
                  {/* 沒有暫定名稱時的手動命名。§8.3 的自我介紹推定還沒接上，
                      在那之前這是使用者唯一能給語者名字的地方。 */}
                  {!isPending &&
                    (naming === s.id ? (
                      <input
                        className="spk-input"
                        autoFocus
                        value={nameDraft}
                        placeholder="輸入名稱"
                        onChange={(e) => setNameDraft(e.target.value)}
                        onBlur={() => void submitName(s.id, nameDraft)}
                        onKeyDown={(e) => {
                          if (e.key === 'Enter') void submitName(s.id, nameDraft);
                          if (e.key === 'Escape') setNaming(null);
                        }}
                      />
                    ) : (
                      <button
                        className="mini"
                        disabled={!canName || s.id === UNKNOWN_REMOTE}
                        onClick={() => {
                          setNameDraft(s.confirmedName ?? '');
                          setNaming(s.id);
                        }}
                      >
                        {s.confirmedName ? '改名' : '命名'}
                      </button>
                    ))}
{/* 聲紋比對寧可錯拆也不錯併（§8.1），代價是同一個人可能多出一列。
                      這裡是把它改回來的地方；合併只是別名，片段的 id 不變。 */}
                  {!isPending &&
                    naming !== s.id &&
                    (merging === s.id ? (
                      <select
                        className="spk-input"
                        autoFocus
                        defaultValue=""
                        onBlur={() => setMerging(null)}
                        onChange={(e) => void submitMerge(s.id, e.target.value)}
                      >
                        <option value="">併入誰…</option>
                        {roster
                          .filter((x) => x.id !== s.id && x.id !== UNKNOWN_REMOTE)
                          .map((x) => (
                            <option key={x.id} value={x.id}>
                              {displayName(x, model.speakers)}
                            </option>
                          ))}
                      </select>
                    ) : (
                      <button
                        className="mini"
                        disabled={!canName || s.id === UNKNOWN_REMOTE || mergeTargets < 1}
                        title="這一列其實是名單上的另一個人"
                        onClick={() => setMerging(s.id)}
                      >
                        併入
                      </button>
                    ))}
                </div>
              );
            })}
          </section>

          <section className="card">
            <div className="card-head">
              <span className="card-title">摘要版本</span>
              <span className="spacer" />
              <span className="count">錄音不中斷</span>
            </div>
            {model.snapshots.length === 0 && (
              <p className="hint">尚未建立快照。建立後生成在背景進行，錄音與逐字稿不會停。</p>
            )}
            {model.snapshots.map((s) => (
              <button
                className="snap"
                data-s={s.state}
                key={s.version}
                aria-current={s.version === model.activeVersion}
                onClick={() => {
                  // 失敗的版本沒有內容可看，點它是想再跑一次
                  if (s.state === 'failed' && canSnapshot) {
                    retry(s.prompt);
                    return;
                  }
                  if (s.state !== 'completed') return;
                  setModel((m) => ({ ...m, activeVersion: s.version }));
                  // 點版本就是想看那一版，不必再點一次分頁
                  setPane('document');
                }}
                title={
                  s.state === 'failed' ? '再跑一次，沿用這一版的要求' : undefined
                }
              >
                <span className="snap-v num">v{s.version}</span>
                <span className="snap-meta">
                  {mmss(s.meetingTimeMs)}・涵蓋至 seq {s.throughEventSeq}
                </span>
                <span className="snap-state" data-s={s.state}>
                  {{ completed: '已完成', running: '生成中', queued: '排隊中', failed: '失敗' }[s.state]}
                </span>
                {/* 本輪要求。沒有它，多個版本在畫面上看起來一模一樣，
                    使用者無從知道哪一版問的是什麼 */}
                {s.prompt && <span className="snap-prompt">「{s.prompt}」</span>}
                {s.state === 'failed' && s.reason && (
                  <span className="snap-reason">{s.reason}</span>
                )}
                {/* 生成期間 Provider 的最新一行。一輪最多十分鐘，只顯示
                    「生成中」的話使用者分不出在跑還是卡死。 */}
                {s.state === 'running' && s.progress && (
                  <span className="snap-progress">{s.progress}</span>
                )}
              </button>
            ))}

            {/* 沒有「開啟」按鈕：點版本就會切到主欄，主欄上方的分頁也隨時
                回得去，再放一顆做同一件事的按鈕只是多一個要讀的東西。 */}
            {active && blocks.length === 0 && (
              <p className="hint">這個版本沒有通過驗證的區塊。</p>
            )}
          </section>

          <section className="card grow">
            <div className="card-head">
              <span className="card-title">人工筆記</span>
              <span className="count num">{model.notes.length}</span>
            </div>
            {model.notes.length === 0 && <p className="hint">會議中隨手記的內容會優先進入摘要。</p>}
            {[...model.notes].reverse().map((n) => (
              <div className="note-item" key={n.id}>
                <time className="num">{mmss(n.meetingTimeMs)}</time>
                <span>{n.text}</span>
              </div>
            ))}
          </section>
        </aside>
      </div>

      {/* 降級狀態的手動觸發。只在開發建置裡出現：它偽造的是「麥克風掉了」
          「生成失敗」這些狀態，在使用者手上等於一個會說謊的按鈕。 */}
      {import.meta.env.DEV && (
        <details className="harness">
          <summary>模擬事件</summary>
          <button
            onClick={() => {
              const last = [...model.segments]
                .reverse()
                .find((s) => s.stability === 'final' && s.origin === 'provider');
              if (!last) return;
              const next = last.text.endsWith('。')
                ? `${last.text.slice(0, -1)}，這點下次要再確認。`
                : `${last.text}（已修訂）`;
              commands.editTranscript(last.id, next);
            }}
          >
            修訂最後一段逐字稿
          </button>
          <button
            onClick={() => {
              commands.injectFault('micLost');
              setLocalDegrade({
                title: '麥克風已中斷',
                body: '系統音訊仍在錄，你的發言目前不會進入逐字稿。重新接上裝置後自動恢復，錄音沒有中斷。',
                tone: 'bad',
              });
            }}
          >
            麥克風中斷
          </button>
          <button
            onClick={() => {
              commands.injectFault('micRestored');
              setLocalDegrade(null);
            }}
          >
            麥克風恢復
          </button>
          <button
            onClick={() => {
              commands.injectFault('sttDown');
              setLocalDegrade({
                title: '逐字稿服務斷線',
                body: '音訊仍在寫入本機，斷線期間的內容會在恢復後補上。錄音沒有中斷。',
                tone: 'warn',
              });
              setTimeout(() => {
                commands.injectFault('sttUp');
                setLocalDegrade({
                  title: '逐字稿已恢復',
                  body: '斷線期間的音訊已排入補轉錄，完成後會插回原本的時間位置。',
                  tone: 'warn',
                });
              }, 3600);
            }}
          >
            STT 斷線並重連
          </button>
          <button onClick={() => commands.injectFault('generationFailed')}>生成失敗</button>
          <button
            onClick={() =>
              setLocalDegrade({
                title: '磁碟空間不足',
                body: '已寫完目前分段並準備安全停止。已完成的音訊、逐字稿與筆記都會保留，這不是崩潰。',
                tone: 'bad',
              })
            }
          >
            磁碟空間不足
          </button>
        </details>
      )}
    </>
  );
}
