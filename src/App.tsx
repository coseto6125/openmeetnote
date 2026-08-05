/**
 * 應用外殼：分頁切換、事件訂閱、會議生命週期控制。
 *
 * 訂閱與 model 放在這裡而不是 LiveView，因為切到歷史或設定分頁時，錄音與
 * 事件累積都必須繼續。若訂閱跟著畫面卸載，回到錄音分頁就會發現中間漏了
 * 一段事件，然後被迫 resync，那是自己製造出來的問題。
 */
import { useEffect, useState } from 'react';
import {
  activeMeeting,
  commands,
  hhmmss,
  subscribe,
  type Health,
  type MeetingState,
} from './session';
import {
  applyBatch,
  emptyMeeting,
  fromProjection,
  type Degrade,
  type MeetingModel,
} from './meeting';
import { LiveView } from './views/LiveView';
import { HistoryView } from './views/HistoryView';
import { SettingsView } from './views/SettingsView';

type Tab = 'live' | 'history' | 'settings';

const TABS: { id: Tab; label: string }[] = [
  { id: 'live', label: '錄音' },
  { id: 'history', label: '歷史' },
  { id: 'settings', label: '設定' },
];

const STATE_LABEL: Record<MeetingState, string> = {
  idle: '尚未開始',
  recording: '錄音中',
  paused: '已暫停',
  stopping: '收尾中',
  finalizing: '結算中',
  completed: '已結束',
  failed: '異常結束',
};

export default function App() {
  const [tab, setTab] = useState<Tab>('live');
  const [model, setModel] = useState<MeetingModel>(emptyMeeting);
  const [localDegrade, setLocalDegrade] = useState<Degrade | null>(null);
  const [theme, setTheme] = useState<'light' | 'dark' | null>(null);
  const [meetingId, setMeetingId] = useState<number | null>(null);

  /* ── 訂閱。不自動開始會議：開始會建立一列會議紀錄，
        每次啟動就多一場沒發生過的會議會把歷史頁塞滿。 ───────── */

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;

    (async () => {
      const fn = await subscribe((batch) => setModel((m) => applyBatch(m, batch)));
      // StrictMode 會在這個 promise resolve 之前執行 cleanup
      if (cancelled) {
        fn();
        return;
      }
      unlisten = fn;
    })();

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  /* ── 缺號就重新同步，不帶著破洞繼續套用增量 ─────────────── */

  useEffect(() => {
    if (!model.desynced) return;
    let alive = true;
    commands
      .resync()
      .then((p) => alive && setModel((m) => fromProjection(m, p)))
      .catch(() => {
        if (alive) {
          setLocalDegrade({
            title: '無法與核心重新同步',
            body: '畫面內容可能不完整。錄音與逐字稿仍在本機寫入，重新開啟應用程式即可恢復。',
            tone: 'bad',
          });
        }
      });
    return () => {
      alive = false;
    };
  }, [model.desynced]);

  /* ── 會議狀態一變就重問一次「哪一場還在進行」 ─────────────
        結束之後那場就不是進行中了，歷史頁要能正確標示並允許刪除。 */

  useEffect(() => {
    let alive = true;
    activeMeeting()
      .then((id) => alive && setMeetingId(id))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [model.state]);

  const live = model.state === 'recording' || model.state === 'paused';

  const start = async () => {
    const r = await commands.start();
    if (!r.accepted) {
      setLocalDegrade({ title: '無法開始錄音', body: r.note ?? '未知原因', tone: 'bad' });
      return;
    }
    setMeetingId(await activeMeeting());

  };

  const newMeeting = async () => {
    await commands.newMeeting();
    setModel(emptyMeeting);
    setLocalDegrade(null);
    setMeetingId(null);
  };

  const togglePause = () => {
    if (model.state === 'recording') commands.pause();
    else if (model.state === 'paused') commands.resume();
  };

  const toggleTheme = () => {
    const now = theme ?? (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');
    const next = now === 'dark' ? 'light' : 'dark';
    document.documentElement.dataset.theme = next;
    setTheme(next);
  };

  const healthLevel = (h: Health) => (h === 'ok' ? 'ok' : h === 'degraded' ? 'warn' : 'bad');

  return (
    <>
      <header className="topbar">
        <div className="brand">
          <span className="brand-mark" aria-hidden="true">
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round">
              <path d="M8 2v12M4.5 5v6M11.5 5v6M1.5 7.2v1.6M14.5 7.2v1.6" />
            </svg>
          </span>
          OpenMeetNote
        </div>

        <nav className="tabs" role="tablist" aria-label="主要分頁">
          {TABS.map((t) => (
            <button
              key={t.id}
              role="tab"
              className="tab"
              aria-selected={tab === t.id}
              onClick={() => setTab(t.id)}
            >
              {t.label}
              {/* 離開錄音分頁時仍要看得到會議還在跑 */}
              {t.id === 'live' && live && <span className="tab-dot" aria-label="錄音進行中" />}
            </button>
          ))}
        </nav>

        <div className="rec" data-state={model.state}>
          <span className="rec-dot" aria-hidden="true" />
          <span>{STATE_LABEL[model.state]}</span>
        </div>

        <span className="clock num" aria-label="會議已進行時間">{hhmmss(model.meetingTimeMs)}</span>

        <div className="health" role="group" aria-label="來源與工作健康狀態">
          <span className="hpill" data-health={healthLevel(model.health.mic)}>
            <span className="hbar"><i style={{ width: `${model.levels.mic * 100}%` }} /></span>
            <b>我</b>
          </span>
          <span className="hpill" data-health={healthLevel(model.health.system)}>
            <span className="hbar"><i style={{ width: `${model.levels.system * 100}%` }} /></span>
            <b>遠端</b>
          </span>
          <span className="hpill" data-health={healthLevel(model.health.stt)}>
            <b>STT</b> <span>{model.health.stt === 'ok' ? '已連線' : '重連中'}</span>
          </span>
        </div>

        <div className="toolbar-right">
          <button className="btn btn-ghost" onClick={toggleTheme} aria-label="切換配色模式">
            <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5">
              <circle cx="8" cy="8" r="3.2" />
              <path d="M8 1v1.6M8 13.4V15M15 8h-1.6M2.6 8H1M12.9 3.1l-1.1 1.1M4.2 11.8l-1.1 1.1M12.9 12.9l-1.1-1.1M4.2 4.2 3.1 3.1" strokeLinecap="round" />
            </svg>
          </button>
          {model.state === 'idle' && (
            <button className="btn btn-primary" onClick={() => void start()}>
              開始錄音
            </button>
          )}
          {model.state === 'completed' && (
            <button className="btn btn-primary" onClick={() => void newMeeting()}>
              開新會議
            </button>
          )}
          <button className="btn" onClick={togglePause} disabled={!live}>
            {model.state === 'paused' ? '繼續' : '暫停'}
          </button>
          <button className="btn btn-danger" onClick={() => commands.stop()} disabled={!live}>
            結束會議
          </button>
        </div>
      </header>

      {/* 分頁切換時卸載而不是隱藏：訂閱在 App，會議不受影響，
          重新掛載的成本只是一次渲染，換來不必同時持有三份畫面狀態。 */}
      {tab === 'live' && (
        <LiveView
          model={model}
          setModel={setModel}
          localDegrade={localDegrade}
          setLocalDegrade={setLocalDegrade}
        />
      )}
      {tab === 'history' && <HistoryView activeMeetingId={meetingId} />}
      {tab === 'settings' && <SettingsView />}
    </>
  );
}
