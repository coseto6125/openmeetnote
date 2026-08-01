/**
 * 錄音脊柱：這個介面唯一的高密度資訊圖。
 *
 * 它回答一個其他控制項回答不了的問題：有多少錄音還沒有進入任何摘要。
 * 因此涵蓋範圍畫成填充區域而不是一條線，未涵蓋的尾段用另一種顏色標出來。
 * 同時承載暫停造成的時間軸空洞、兩軌活動與筆記錨點。
 */
import { useLayoutEffect, useRef, useState } from 'react';
import { mmss } from '../session';

export interface SpineSegment {
  meetingTimeMs: number;
  track: 'mic' | 'system';
  final: boolean;
}

export interface SpineSnapshot {
  version: number;
  meetingTimeMs: number;
  state: 'queued' | 'running' | 'completed' | 'failed';
}

export interface SpinePause {
  fromMs: number;
  toMs: number;
}

interface Props {
  meetingTimeMs: number;
  segments: SpineSegment[];
  snapshots: SpineSnapshot[];
  notes: { meetingTimeMs: number; text: string }[];
  pauses: SpinePause[];
  activeVersion: number | null;
  onSeek: (meetingTimeMs: number) => void;
}

export function Spine({
  meetingTimeMs,
  segments,
  snapshots,
  notes,
  pauses,
  activeVersion,
  onSeek,
}: Props) {
  const hostRef = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 96, h: 400 });
  const [hover, setHover] = useState<{ y: number; label: string } | null>(null);

  useLayoutEffect(() => {
    const el = hostRef.current;
    if (!el) return;
    const ro = new ResizeObserver(([entry]) => {
      const { width, height } = entry.contentRect;
      setSize({ w: Math.max(width, 40), h: Math.max(height, 80) });
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const total = Math.max(meetingTimeMs, 1);
  const { w, h } = size;
  const y = (ms: number) => (Math.min(ms, total) / total) * h;

  const midL = w * 0.34;
  const midR = w * 0.58;
  const trackW = 6;

  // 涵蓋上緣取最後一個已完成或進行中的快照；失敗的不算涵蓋
  const covering = snapshots.filter((s) => s.state !== 'failed');
  const coverUntil = covering.length ? covering[covering.length - 1].meetingTimeMs : 0;
  const lagMs = Math.max(0, meetingTimeMs - coverUntil);

  return (
    <div className="spine-plot" ref={hostRef}>
      <svg
        className="spine-svg"
        viewBox={`0 0 ${w} ${h}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`錄音時間軸，總長 ${mmss(meetingTimeMs)}，尚未進入摘要 ${mmss(lagMs)}`}
        onMouseMove={(e) => {
          const box = e.currentTarget.getBoundingClientRect();
          const py = e.clientY - box.top;
          const ms = (py / box.height) * total;
          const note = notes.find((n) => Math.abs(y(n.meetingTimeMs) - py) < 6);
          const snap = snapshots.find((s) => Math.abs(y(s.meetingTimeMs) - py) < 6);
          const pause = pauses.find((p) => py >= y(p.fromMs) - 2 && py <= y(p.toMs) + 2);
          setHover({
            y: py,
            label: note
              ? `筆記　${note.text.slice(0, 12)}`
              : snap
                ? `快照 v${snap.version}`
                : pause
                  ? `暫停 ${mmss(pause.toMs - pause.fromMs)}`
                  : mmss(ms),
          });
        }}
        onMouseLeave={() => setHover(null)}
        onClick={(e) => {
          const box = e.currentTarget.getBoundingClientRect();
          onSeek(((e.clientY - box.top) / box.height) * total);
        }}
      >
        <defs>
          <pattern id="lagHatch" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
            <rect width="6" height="6" fill="var(--amber-soft)" />
            <line x1="0" y1="0" x2="0" y2="6" stroke="var(--amber)" strokeWidth="1.1" opacity="0.5" />
          </pattern>
          <pattern id="pauseHatch" width="5" height="5" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
            <rect width="5" height="5" fill="transparent" />
            <line x1="0" y1="0" x2="0" y2="5" stroke="var(--line-2)" strokeWidth="1" />
          </pattern>
        </defs>

        {/* 已進入摘要的範圍 */}
        {coverUntil > 0 && (
          <rect x="0" y="0" width={w} height={y(coverUntil)} fill="var(--live-soft)" />
        )}
        {/* 尚未進入任何摘要的尾段 */}
        {lagMs > 0 && (
          <rect x="0" y={y(coverUntil)} width={w} height={h - y(coverUntil)} fill="url(#lagHatch)" />
        )}

        {/* 兩軌床 */}
        <rect x={midL - trackW / 2} y="0" width={trackW} height={h} rx={trackW / 2} fill="var(--spine-bed)" />
        <rect x={midR - trackW / 2} y="0" width={trackW} height={h} rx={trackW / 2} fill="var(--spine-bed)" />

        {/* 暫停期間沒有錄音，兩軌在此開孔 */}
        {pauses.map((p, i) => (
          <g key={`p${i}`}>
            <rect
              x={midL - trackW / 2 - 1}
              y={y(p.fromMs)}
              width={midR - midL + trackW + 2}
              height={Math.max(y(p.toMs) - y(p.fromMs), 2)}
              fill="var(--panel)"
            />
            <rect
              x={midL - trackW / 2 - 1}
              y={y(p.fromMs)}
              width={midR - midL + trackW + 2}
              height={Math.max(y(p.toMs) - y(p.fromMs), 2)}
              fill="url(#pauseHatch)"
            />
          </g>
        ))}

        {/* 活動：麥克風軌在左，系統音訊軌在右 */}
        {segments.map((s, i) => (
          <rect
            key={`s${i}`}
            x={(s.track === 'mic' ? midL : midR) - trackW / 2}
            y={y(s.meetingTimeMs)}
            width={trackW}
            height={Math.max(h * 0.006, 2)}
            rx={trackW / 2}
            fill={s.track === 'mic' ? 'var(--live)' : 'var(--violet)'}
            opacity={s.final ? 0.95 : 0.4}
          />
        ))}

        {/* 快照涵蓋上緣 */}
        {snapshots.map((s) => {
          const yy = y(s.meetingTimeMs);
          const active = s.version === activeVersion;
          const failed = s.state === 'failed';
          return (
            <g key={`v${s.version}`}>
              <line
                x1="0"
                x2={w}
                y1={yy}
                y2={yy}
                stroke={failed ? 'var(--red)' : 'var(--live)'}
                strokeWidth={active ? 2 : 1}
                strokeDasharray={failed ? '3 2' : undefined}
                opacity={active ? 1 : 0.45}
              />
              <text
                x="4"
                y={yy - 3}
                fontSize="9"
                fontFamily="var(--mono)"
                fill={failed ? 'var(--red)' : 'var(--live)'}
                opacity={active ? 1 : 0.6}
              >
                v{s.version}
              </text>
            </g>
          );
        })}

        {/* 筆記錨點 */}
        {notes.map((n, i) => (
          <circle key={`n${i}`} cx={w - 7} cy={y(n.meetingTimeMs)} r="3" fill="var(--amber)" />
        ))}

        {/* 目前位置 */}
        <line x1="0" x2={w} y1={h - 1} y2={h - 1} stroke="var(--live)" strokeWidth="2" />

        {hover && (
          <line x1="0" x2={w} y1={hover.y} y2={hover.y} stroke="var(--ink-2)" strokeWidth="1" opacity="0.35" />
        )}
      </svg>

      {hover && (
        <span className="spine-readout num" style={{ top: Math.min(Math.max(hover.y - 9, 0), size.h - 18) }}>
          {hover.label}
        </span>
      )}
    </div>
  );
}
