/**
 * 歷史會議：列表加上重新開啟。
 *
 * 這裡讀的是 SQLite 投影，不是 Session 的記憶體狀態。正在錄的那場會議
 * 也會出現在列表上，但打開它看到的是「已經落地的部分」，不是即時畫面。
 * 這個區別必須在畫面上講出來，否則使用者會以為歷史頁沒有更新。
 */
import { useCallback, useEffect, useState } from 'react';
import { history, hhmmss, mmss, type MeetingDetail, type MeetingSummary } from '../session';

const STATE_LABEL: Record<string, string> = {
  idle: '未開始',
  recording: '錄音中',
  paused: '已暫停',
  stopping: '收尾中',
  finalizing: '結算中',
  completed: '已結束',
};

/** `2026-08-01T09:30:00.000Z` → `08/01 09:30`。留空時回破折線以外的佔位。 */
function stamp(iso: string | null): string {
  if (!iso) return '未開始';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const p = (n: number) => String(n).padStart(2, '0');
  return `${p(d.getMonth() + 1)}/${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

export interface HistoryViewProps {
  /** 目前正在錄的會議 id。列表上要標出來，也不允許刪除。 */
  activeMeetingId: number | null;
}

export function HistoryView({ activeMeetingId }: HistoryViewProps) {
  const [list, setList] = useState<MeetingSummary[] | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [detail, setDetail] = useState<MeetingDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<number | null>(null);
  const [draft, setDraft] = useState('');
  const [notice, setNotice] = useState<string | null>(null);

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

  useEffect(() => {
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

  const remove = async (id: number) => {
    try {
      await history.remove(id);
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
          <span className="count num">{list?.length ?? 0}</span>
          <span className="spacer" />
          <button className="btn btn-ghost" onClick={() => void reload()}>
            重新整理
          </button>
        </div>

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

        <div className="mlist">
          {list?.map((m) => (
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
              <button
                className="mini mini-danger"
                disabled={m.id === activeMeetingId}
                title={m.id === activeMeetingId ? '進行中的會議不能刪除' : '刪除這場會議'}
                onClick={() => void remove(m.id)}
              >
                刪除
              </button>
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
                  <p>{notice}</p>
                </span>
                <button className="btn-ghost close" onClick={() => setNotice(null)} aria-label="關閉">
                  ×
                </button>
              </div>
            )}

            {detail.runs.length > 0 && (
              <div className="runs">
                {detail.runs.map((r) => (
                  <span className="run-chip" key={r.runId} data-s={r.status}>
                    v{r.versionNo}・涵蓋至 seq {r.throughEventSeq}
                    {r.failureReason ? `・${r.failureReason}` : ''}
                    {r.status === 'completed' && (
                      <button
                        className="mini"
                        onClick={() =>
                          history
                            .exportDocument(detail.summary.id, r.runId)
                            // 匯出讀的是快照當時的版本，不是現在的逐字稿
                            .then((path) => setNotice(path))
                            .catch((e) => setError(String(e)))
                        }
                      >
                        匯出 HTML
                      </button>
                    )}
                  </span>
                ))}
              </div>
            )}

            <div className="hist-scroll">
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
                  <article className="utt" key={s.segmentId} data-stability="final">
                    <span className="utt-time num">{mmss(s.meetingStartMs)}</span>
                    <div className="utt-body">
                      <span className="utt-who">
                        {s.speakerId ?? '未指派'}
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
