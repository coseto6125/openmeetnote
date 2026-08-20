/**
 * 歷史會議：列表加上重新開啟。
 *
 * 這裡讀的是 SQLite 投影，不是 Session 的記憶體狀態。正在錄的那場會議
 * 也會出現在列表上，但打開它看到的是「已經落地的部分」，不是即時畫面。
 * 這個區別必須在畫面上講出來，否則使用者會以為歷史頁沒有更新。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  history,
  hhmmss,
  mmss,
  snapshotDocument,
  type DocumentBlock,
  type MeetingDetail,
  type MeetingHit,
  type MeetingSummary,
} from '../session';
import { speakerDisplayName, type NamedSpeaker } from '../meeting';
import { DocumentView, type CitedSegment } from '../components/DocumentView';

/**
 * 語者 id 對顯示名稱。
 *
 * 軌道不在 speakers 投影裡（`speakers` 表沒有這一欄），但每一段逐字稿都帶著
 * 自己的 track，所以從段落回推。沒有任何段落的語者不會出現在逐字稿上，
 * 推不出軌道也就沒有影響。
 */
function speakerNames(detail: MeetingDetail): Map<string, string> {
  const trackOf = new Map<string, 'mic' | 'system'>();
  for (const s of detail.segments) {
    if (s.speakerId && !trackOf.has(s.speakerId)) trackOf.set(s.speakerId, s.track);
  }
  const named: (NamedSpeaker & { speakerId: string })[] = detail.speakers.map((s) => ({
    speakerId: s.speakerId,
    id: s.speakerId,
    ordinal: s.ordinal,
    track: trackOf.get(s.speakerId) ?? 'system',
    proposedName: s.proposedName,
    confirmedName: s.confirmedName,
    mergedInto: s.mergedInto,
  }));
  return new Map(named.map((s) => [s.speakerId, speakerDisplayName(s, named)]));
}

const STATE_LABEL: Record<string, string> = {
  idle: '未開始',
  recording: '錄音中',
  paused: '已暫停',
  stopping: '收尾中',
  finalizing: '結算中',
  completed: '已結束',
  // 與「已結束」分開講：這場會議的逐字稿可能斷在半句話，使用者需要知道
  failed: '異常結束',
};

/** `2026-08-01T09:30:00.000Z` → `08/01 09:30`。留空時回破折線以外的佔位。 */
function stamp(iso: string | null): string {
  if (!iso) return '未開始';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const p = (n: number) => String(n).padStart(2, '0');
  return `${p(d.getMonth() + 1)}/${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/**
 * 把命中的字標出來。
 *
 * 用切字串而不是 `dangerouslySetInnerHTML`：逐字稿是不受信任的內容（§9.4），
 * 而這裡是它離「被當成標記解讀」最近的一次。回傳節點陣列，React 照樣轉義。
 */
function highlight(text: string, term: string) {
  if (!term) return text;
  const parts: React.ReactNode[] = [];
  const lower = text.toLowerCase();
  const needle = term.toLowerCase();
  let at = 0;
  for (;;) {
    const found = lower.indexOf(needle, at);
    if (found < 0) break;
    if (found > at) parts.push(text.slice(at, found));
    parts.push(<mark key={found}>{text.slice(found, found + term.length)}</mark>);
    at = found + term.length;
  }
  parts.push(text.slice(at));
  return parts;
}

export interface HistoryViewProps {
  /** 目前正在錄的會議 id。列表上要標出來，也不允許刪除。 */
  activeMeetingId: number | null;
}

export function HistoryView({ activeMeetingId }: HistoryViewProps) {
  const [list, setList] = useState<MeetingSummary[] | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [detail, setDetail] = useState<MeetingDetail | null>(null);
  const names = useMemo(() => (detail ? speakerNames(detail) : new Map()), [detail]);
  const [error, setError] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<number | null>(null);
  const [draft, setDraft] = useState('');
  // 匯出完成後的提示：路徑加上那一版的版本號，開資料夾要用它
  const [notice, setNotice] = useState<{ path: string; versionNo: number } | null>(null);
  // 開著的摘要版本。歷史頁本來只能匯出 HTML 再用瀏覽器開，
  // 而「開完會回頭看摘要」正是這個頁面最主要的用途。
  const [openRun, setOpenRun] = useState<number | null>(null);
  const [blocks, setBlocks] = useState<DocumentBlock[] | null>(null);
  const [query, setQuery] = useState('');
  // null 代表沒有在搜尋，顯示完整清單。空陣列代表搜了但沒有命中，
  // 這兩件事在畫面上要講不同的話。
  const [hits, setHits] = useState<MeetingHit[] | null>(null);
  // 為這場已結束的會議建立摘要。生成要幾十秒，按鈕期間要看得出在跑，
  // 否則使用者會以為沒反應而重複按。
  const [summaryPrompt, setSummaryPrompt] = useState('');
  const [summarising, setSummarising] = useState(false);
  // 生成期間使用者可能換了會議。用 ref 而不是 state：非同步收尾時要讀的是
  // 「現在選的是誰」，而閉包裡的 state 停在按下按鈕的那一刻。
  const selectedRef = useRef<number | null>(null);
  selectedRef.current = selected;

  const reload = useCallback(async () => {
    try {
      const rows = await history.list();
      setList(rows);
      setError(null);
      // 選中的會議被刪掉之後不要留著空的詳情面板
      setSelected((cur) => (cur !== null && rows.some((r) => r.id === cur) ? cur : null));
    } catch (e) {
      setError(String(e));
      setList([]);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  /* ── 搜尋。每次按鍵都查會在長逐字稿上白掃很多次，等手停下來再查 ── */

  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setHits(null);
      return;
    }
    let alive = true;
    const timer = setTimeout(() => {
      history
        .search(q)
        .then((r) => alive && setHits(r))
        .catch((e) => alive && setError(String(e)));
    }, 200);
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [query]);

  useEffect(() => {
    setOpenRun(null);
    setBlocks(null);
    setSummaryPrompt('');
    if (selected === null) {
      setDetail(null);
      return;
    }
    let alive = true;
    history
      .open(selected)
      .then((d) => alive && setDetail(d))
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [selected]);

  const submitRename = async (id: number) => {
    const title = draft.trim();
    setRenaming(null);
    if (!title) return;
    try {
      await history.rename(id, title);
      await reload();
      if (selected === id) setDetail((d) => (d ? { ...d, summary: { ...d.summary, title } } : d));
    } catch (e) {
      setError(String(e));
    }
  };

  // 引用要指得回逐字稿。歷史頁的片段沒有「會議時間」欄位，
  // meetingStartMs 就是它。
  const citedSegments: CitedSegment[] = (detail?.segments ?? []).map((s) => ({
    id: String(s.segmentId),
    meetingTimeMs: s.meetingStartMs,
    revision: s.revision,
  }));

  // 一份清單走兩條來源：沒在搜尋時是全部，搜尋時是命中的那些加上命中原因。
  // 兩份 JSX 會慢慢長歪，命中的那份最後就會少掉改名或刪除。
  const rows: { m: MeetingSummary; hit?: MeetingHit }[] =
    hits === null
      ? (list ?? []).map((m) => ({ m }))
      : hits.map((h) => ({ m: h.summary, hit: h }));

  const summarize = async (meetingId: number) => {
    setSummarising(true);
    setError(null);
    // 生成要幾十秒，中途使用者可能已經去看別場了。那時候把結果套到畫面上，
    // 看到的會是別場會議配上這場的摘要。
    const stillHere = () => selectedRef.current === meetingId;
    try {
      const version = await history.summarize(meetingId, summaryPrompt);
      const d = await history.open(meetingId);
      const doc = await snapshotDocument(meetingId, version).catch(() => null);
      await reload();
      if (!stillHere()) return;
      setSummaryPrompt('');
      setDetail(d);
      // 生成完就直接打開它，那是按下按鈕的人想看的東西
      const run = d.runs.find((r) => r.versionNo === version);
      if (run && doc) {
        setOpenRun(run.runId);
        setBlocks(doc);
      }
    } catch (e) {
      // 失敗的版本也落地了，重新讀一次才看得到它與失敗原因
      const d = await history.open(meetingId).catch(() => null);
      if (!stillHere()) return;
      setError(String(e));
      if (d) setDetail(d);
    } finally {
      setSummarising(false);
    }
  };

  const remove = async (id: number) => {
    try {
      await history.remove(id);
      await reload();
    } catch (e) {
      setError(String(e));
    }
  };

  const removeAudio = async (id: number) => {
    try {
      const n = await history.removeAudio(id);
      // 說出刪了幾個，而不是靜默完成：刪除是不可逆的，使用者需要知道
      // 剛才到底發生了什麼。零個也要說 —— 那代表這場本來就沒有原音。
      setError(n > 0 ? `已刪除 ${n} 個音檔，逐字稿與摘要保留` : '這場沒有保存原音');
      await reload();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="page page-split">
      <section className="panel">
        <div className="panel-head">
          <span className="card-title">歷史會議</span>
          <span className="count num">
            {hits === null ? (list?.length ?? 0) : `${hits.length}/${list?.length ?? 0}`}
          </span>
          <span className="spacer" />
          <button className="btn btn-ghost" onClick={() => void reload()}>
            重新整理
          </button>
        </div>

        <label className="search-field">
          <span className="sr-only">搜尋會議</span>
          <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="var(--muted)" strokeWidth="1.6" aria-hidden="true">
            <circle cx="7" cy="7" r="4.6" />
            <path d="M10.4 10.4 14 14" strokeLinecap="round" />
          </svg>
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="搜尋標題、逐字稿與筆記"
            autoComplete="off"
            type="search"
          />
          {query && (
            <button className="btn-ghost close" onClick={() => setQuery('')} aria-label="清除搜尋">
              ×
            </button>
          )}
        </label>

        {error && (
          <div className="banner" data-tone="bad" role="status">
            <span>
              <b>讀取歷史時發生問題</b>
              <p>{error}</p>
            </span>
          </div>
        )}

        {list === null && <p className="hint">讀取中。</p>}
        {list !== null && list.length === 0 && !error && (
          <p className="empty">還沒有任何會議。到「錄音」分頁開始第一場。</p>
        )}
        {hits !== null && hits.length === 0 && (
          <p className="empty">沒有會議提到「{query.trim()}」。搜尋範圍是標題、逐字稿與人工筆記。</p>
        )}

        <div className="mlist">
          {rows.map(({ m, hit }) => (
            <div
              className="mrow"
              key={m.id}
              data-selected={m.id === selected ? '1' : '0'}
              data-live={m.id === activeMeetingId ? '1' : '0'}
            >
              <button className="mrow-main" onClick={() => setSelected(m.id)}>
                {renaming === m.id ? (
                  <input
                    className="mrow-rename"
                    value={draft}
                    autoFocus
                    onChange={(e) => setDraft(e.target.value)}
                    onBlur={() => void submitRename(m.id)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') void submitRename(m.id);
                      if (e.key === 'Escape') setRenaming(null);
                    }}
                    onClick={(e) => e.stopPropagation()}
                  />
                ) : (
                  <span className="mrow-title">{m.title}</span>
                )}
                <span className="mrow-meta num">
                  {stamp(m.startedAt)}・{hhmmss(m.meetingTimeMs)}・{m.segmentCount} 段・
                  {m.noteCount} 筆記
                </span>
              </button>
              <span className="mrow-state" data-s={m.state}>
                {m.id === activeMeetingId ? '進行中' : STATE_LABEL[m.state] ?? m.state}
              </span>
              <button
                className="mini"
                onClick={() => {
                  setRenaming(m.id);
                  setDraft(m.title);
                }}
              >
                改名
              </button>
              {/* 只刪音檔，留下逐字稿與摘要。音檔是最佔空間的東西，
                  而它的用途（驗證逐字稿）通常在看過之後就結束了。 */}
              <button
                className="mini"
                disabled={m.id === activeMeetingId}
                title={
                  m.id === activeMeetingId
                    ? '進行中的會議不能刪除音檔'
                    : '只刪原音，保留逐字稿與摘要'
                }
                onClick={() => void removeAudio(m.id)}
              >
                刪音檔
              </button>
              <button
                className="mini mini-danger"
                disabled={m.id === activeMeetingId}
                title={m.id === activeMeetingId ? '進行中的會議不能刪除' : '刪除這場會議'}
                onClick={() => void remove(m.id)}
              >
                刪除
              </button>

              {/* 命中原因。只有標題命中時 excerpts 是空的，標題就在上面，
                  再抄一次是雜訊。 */}
              {hit && hit.excerpts.length > 0 && (
                <div className="mhits">
                  {hit.excerpts.map((e, i) => (
                    <p key={i}>
                      <span className="num">{mmss(e.meetingTimeMs)}</span>
                      {e.kind === 'note' && <span className="mhit-kind">筆記</span>}
                      {highlight(e.text, query.trim())}
                    </p>
                  ))}
                  {hit.totalHits > hit.excerpts.length && (
                    <p className="hint">另有 {hit.totalHits - hit.excerpts.length} 處命中</p>
                  )}
                </div>
              )}
            </div>
          ))}
        </div>
      </section>

      <section className="panel">
        {!detail && <p className="empty">選一場會議查看內容。</p>}
        {detail && (
          <>
            <div className="panel-head">
              <span className="card-title">{detail.summary.title}</span>
              <span className="spacer" />
              <span className="count num">
                錄音 {hhmmss(detail.summary.capturedAudioMs)}・全程{' '}
                {hhmmss(detail.summary.meetingTimeMs)}
              </span>
            </div>

            {detail.summary.id === activeMeetingId && (
              <div className="banner" data-tone="warn" role="status">
                <span>
                  <b>這場會議正在進行</b>
                  <p>這裡顯示的是已經寫入本機資料庫的部分，不會即時更新。即時畫面在「錄音」分頁。</p>
                </span>
              </div>
            )}

            {notice && (
              <div className="banner" data-tone="warn" role="status">
                <span>
                  <b>已匯出</b>
                  <p>{notice.path}</p>
                </span>
                {/* 只給路徑等於要使用者自己去翻。開一次資料夾就到了。 */}
                <button
                  className="mini"
                  onClick={() =>
                    history
                      .revealExport(detail.summary.id, notice.versionNo)
                      .catch((e) => setError(String(e)))
                  }
                >
                  開啟資料夾
                </button>
                <button className="btn-ghost close" onClick={() => setNotice(null)} aria-label="關閉">
                  ×
                </button>
              </div>
            )}

            {/* 已結束的會議在這裡建立摘要。錄音中的那一場走「錄音」分頁，
                兩條路徑同時配發版本號會撞號。 */}
            {detail.summary.id !== activeMeetingId && (
              <div className="summarize-row">
                <label className="note-field">
                  <span className="sr-only">本輪要求</span>
                  <input
                    aria-label="本輪要求"
                    value={summaryPrompt}
                    onChange={(e) => setSummaryPrompt(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' && !summarising) void summarize(detail.summary.id);
                    }}
                    placeholder={
                      detail.runs.some((r) => r.status === 'completed')
                        ? '要改什麼？會在最後一版的基礎上修訂'
                        : '這一版想要什麼？留空由 AI 自行規劃'
                    }
                    autoComplete="off"
                    disabled={summarising}
                  />
                </label>
                <button
                  className="btn btn-primary"
                  disabled={summarising || detail.segments.length === 0}
                  title={
                    detail.segments.length === 0 ? '這場會議沒有逐字稿可以摘要' : undefined
                  }
                  onClick={() => void summarize(detail.summary.id)}
                >
                  {summarising ? '生成中…' : '建立摘要'}
                </button>
              </div>
            )}

            {detail.runs.length > 0 && (
              <div className="runs">
                {detail.runs.map((r) => (
                  <span
                    className="run-chip"
                    key={r.runId}
                    data-s={r.status}
                    aria-current={r.runId === openRun}
                  >
                    v{r.versionNo}・涵蓋至 seq {r.throughEventSeq}
                    {r.failureReason ? `・${r.failureReason}` : ''}
                    {/* §13：LLM 失敗要允許重試。重試是新的一版，但不必重打
                        要求 —— 那才是使用者真正要重做的事。 */}
                    {r.status === 'failed' && r.prompt && (
                      <button
                        className="mini"
                        disabled={summarising}
                        onClick={() => setSummaryPrompt(r.prompt)}
                        title="把這一版的要求填回輸入框"
                      >
                        沿用要求
                      </button>
                    )}
                    {r.status === 'completed' && (
                      <>
                        <button
                          className="mini"
                          onClick={() => {
                            if (r.runId === openRun) {
                              setOpenRun(null);
                              setBlocks(null);
                              return;
                            }
                            setOpenRun(r.runId);
                            setBlocks(null);
                            snapshotDocument(detail.summary.id, r.versionNo)
                              .then(setBlocks)
                              .catch((e) => {
                                setError(String(e));
                                setOpenRun(null);
                              });
                          }}
                        >
                          {r.runId === openRun ? '收起' : '看摘要'}
                        </button>
                        <button
                          className="mini"
                          onClick={() =>
                            history
                              .exportDocument(detail.summary.id, r.runId)
                              // 匯出讀的是快照當時的版本，不是現在的逐字稿
                              .then((path) => setNotice({ path, versionNo: r.versionNo }))
                              .catch((e) => setError(String(e)))
                          }
                        >
                          匯出 HTML
                        </button>
                      </>
                    )}
                  </span>
                ))}
              </div>
            )}

            <div className="hist-scroll">
              {openRun !== null && (
                <div className="hist-block">
                  <h3>摘要</h3>
                  {blocks === null && <p className="hint">讀取中。</p>}
                  {blocks !== null && blocks.length === 0 && (
                    <p className="hint">這個版本沒有通過驗證的區塊。</p>
                  )}
                  {blocks !== null && blocks.length > 0 && (
                    <DocumentView
                      blocks={blocks}
                      segments={citedSegments}
                      onCite={(id) =>
                        document
                          .getElementById(`seg-${id}`)
                          ?.scrollIntoView({ block: 'center', behavior: 'smooth' })
                      }
                    />
                  )}
                </div>
              )}

              {detail.notes.length > 0 && (
                <div className="hist-block">
                  <h3>人工筆記</h3>
                  {detail.notes.map((n) => (
                    <div className="note-item" key={n.noteId}>
                      <time className="num">{mmss(n.meetingTimeMs)}</time>
                      <span>{n.text}</span>
                    </div>
                  ))}
                </div>
              )}

              <div className="hist-block">
                <h3>逐字稿</h3>
                {detail.segments.length === 0 && <p className="hint">這場會議沒有逐字稿內容。</p>}
                {detail.segments.map((s) => (
                  <article
                    className="utt"
                    key={s.segmentId}
                    id={`seg-${s.segmentId}`}
                    data-stability="final"
                  >
                    <span className="utt-time num">{mmss(s.meetingStartMs)}</span>
                    <div className="utt-body">
                      <span className="utt-who">
                        {(s.speakerId && names.get(s.speakerId)) ?? '未指派'}
                        <em>{s.track === 'mic' ? '麥克風軌' : '系統音訊軌'}</em>
                      </span>
                      <span className="utt-text">{s.text}</span>
                      {s.userEdited && <span className="edited-tag">已修訂 r{s.revision}</span>}
                    </div>
                  </article>
                ))}
              </div>
            </div>
          </>
        )}
      </section>
    </div>
  );
}
