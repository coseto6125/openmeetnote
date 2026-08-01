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
import type { Degrade, MeetingModel, Snapshot, Speaker } from '../meeting';
import { Spine, type SpinePause } from '../components/Spine';

/**
 * 語者的顯示樣式。
 *
 * 這裡只放顏色與稱呼規則，人是誰由後端的 speakers 決定。
 * 先前這是一個寫死的常數陣列，於是後端有一整張 speakers 表卻沒人填，
 * 語者確認也就無聲消失。
 */
const SPEAKER_COLORS = ['var(--violet)', '#3f7fa6', '#8a6d3b', '#4f7a5e', '#7a4f6d'];

/**
 * §8.4 的名稱優先順序：確認名 > 暫定名 > 依軌道與出現序的預設稱呼。
 *
 * 預設稱呼裡的編號只數遠端語者。ordinal 是全域的出現序，麥克風軌會佔掉一格，
 * 直接拿來顯示的話「我」之後的第一位遠端語者會叫「語者 3」，看起來像少了一個人。
 */
function displayName(s: Speaker, all: Speaker[]): string {
  if (s.confirmedName) return s.confirmedName;
  if (s.proposedName) return s.proposedName;
  // 軌道先驗只到「本機 vs 遠端」，不能再往「遠端只有一人」推（§8.1）
  if (s.track === 'mic') return '我';
  const nth = all.filter((x) => x.track === 'system' && x.ordinal <= s.ordinal).length;
  return `語者 ${nth}`;
}

const colorOf = (s: Speaker) => SPEAKER_COLORS[(s.ordinal - 1) % SPEAKER_COLORS.length];

export interface LiveViewProps {
  model: MeetingModel;
  setModel: React.Dispatch<React.SetStateAction<MeetingModel>>;
  localDegrade: Degrade | null;
  setLocalDegrade: (d: Degrade | null) => void;
}

/** 從已存的區塊取出可顯示的文字。content 是該 kind 的 JSON（§10）。 */
function plainText(b: DocumentBlock): string {
  try {
    const c = JSON.parse(b.content) as Record<string, unknown>;
    if (typeof c.text === 'string') return c.text;
    if (Array.isArray(c.items)) return (c.items as string[]).join('；');
    if (typeof c.title === 'string') return c.title;
    return b.content;
  } catch {
    // 解不開就顯示原文，總比顯示空白讓人以為沒東西好
    return b.content;
  }
}

export function LiveView({ model, setModel, localDegrade, setLocalDegrade }: LiveViewProps) {
  const [rejectedSpeakers, setRejectedSpeakers] = useState<string[]>([]);
  const [noteDraft, setNoteDraft] = useState('');
  const [blocks, setBlocks] = useState<DocumentBlock[]>([]);
  const [naming, setNaming] = useState<string | null>(null);
  // 受控輸入。非受控的話 blur 讀到的值取決於瀏覽器何時同步 DOM，
  // 而 HistoryView 的改名本來就是受控的，兩處不該有兩種寫法。
  const [nameDraft, setNameDraft] = useState('');
  const streamRef = useRef<HTMLDivElement>(null);
  const stickRef = useRef(true);

  /* ── 捲動只在使用者已在底部時跟隨 ───────────────────────── */

  useEffect(() => {
    const el = streamRef.current;
    if (el && stickRef.current) el.scrollTop = el.scrollHeight;
  }, [model.segments]);

  const onStreamScroll = () => {
    const el = streamRef.current;
    if (!el) return;
    stickRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  };

  /* ── 衍生資料 ─────────────────────────────────────────── */

  const speakerOf = useCallback(
    (id: string) => model.speakers.find((s) => s.id === id),
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
  const degrade = localDegrade ?? model.degrade;
  const live = model.state === 'recording' || model.state === 'paused';

  // 暫定名稱來自 §8.3 的自我介紹推定，目前沒有生產者，因此這個清單通常是空的。
  // 保留這條路徑是為了讓 M3 接上時不必重寫確認流程。
  const pending = model.speakers.filter(
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

  const submitNote = async () => {
    const text = noteDraft.trim();
    if (!text) return;
    const r = await commands.addNote(text);
    if (r.accepted) setNoteDraft('');
    else if (r.note) setLocalDegrade({ title: '筆記未送出', body: r.note, tone: 'warn' });
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

  const seek = (ms: number) => {
    const target =
      model.segments.find((s) => s.meetingTimeMs >= ms) ??
      model.segments[model.segments.length - 1];
    if (!target) return;
    stickRef.current = false;
    document.getElementById(`seg-${target.id}`)?.scrollIntoView({ block: 'center', behavior: 'smooth' });
  };

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
            <span className="seg-label">逐字稿</span>
            <span className="count num">{finalCount} 段已定稿</span>
            <span className="spacer" />
            <span className="lag">
              錄音 <span className="num">{mmss(model.capturedAudioMs)}</span>
              {model.meetingTimeMs - model.capturedAudioMs > 1500 && (
                <> ・暫停 <span className="num">{mmss(model.meetingTimeMs - model.capturedAudioMs)}</span></>
              )}
            </span>
          </div>

          <div className="stream" ref={streamRef} onScroll={onStreamScroll} tabIndex={0}>
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
            <button className="btn btn-primary" onClick={() => commands.createSnapshot()} disabled={!live}>
              建立摘要快照
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
              <span className="count num">{model.speakers.length}</span>
              {pending.length > 0 && <span className="spk-pending">{pending.length} 待確認</span>}
            </div>
            {model.speakers.length === 0 && (
              <p className="hint">還沒有人發言。語者會在第一次聽到聲音時出現。</p>
            )}
            {model.speakers.map((s) => {
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
                        disabled={!live}
                        onClick={() => {
                          setNameDraft(s.confirmedName ?? '');
                          setNaming(s.id);
                        }}
                      >
                        {s.confirmedName ? '改名' : '命名'}
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
                key={s.version}
                aria-current={s.version === model.activeVersion}
                onClick={() =>
                  s.state === 'completed' && setModel((m) => ({ ...m, activeVersion: s.version }))
                }
              >
                <span className="snap-v num">v{s.version}</span>
                <span className="snap-meta">
                  {mmss(s.meetingTimeMs)}・涵蓋至 seq {s.throughEventSeq}
                </span>
                <span className="snap-state" data-s={s.state}>
                  {{ completed: '已完成', running: '生成中', queued: '排隊中', failed: '失敗' }[s.state]}
                </span>
              </button>
            ))}

            {active && blocks.length === 0 && (
              <p className="hint">這個版本沒有通過驗證的區塊。</p>
            )}
{active && blocks.length > 0 && (
              <div className="claims">
                {blocks.map((b) => (
                  <div className="claim" key={b.position} data-kind={b.claimKind}>
                    <span className="claim-kind">{b.claimKind}</span>
                    {plainText(b)}
                    {b.sourceRefs.map((r, i) => {
                      // 這個片段在生成之後又被改過，引用指向的內容已不是當時看到的
                      const stale =
                        r.sourceKind === 'transcript_segment' &&
                        model.segments.some(
                          (seg) => String(seg.id) === r.sourceId && seg.revision > r.sourceRevision,
                        );
                      return (
                        <span
                          className="cite num"
                          key={i}
                          data-stale={stale ? '1' : '0'}
                          title={
                            stale
                              ? '此引用依據的片段已被修訂，內容可能與生成當時不同'
                              : '來源可回溯，不代表此陳述已被驗證為真'
                          }
                        >
                          seg {r.sourceId} r{r.sourceRevision}
                        </span>
                      );
                    })}
                  </div>
                ))}
              </div>
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
    </>
  );
}
