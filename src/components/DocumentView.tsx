/**
 * 成果文件的畫面渲染（BLUEPRINT.md §10）。
 *
 * 與匯出的 HTML 走同一套分區規則：成果摘要在最前、決議與行動項目獨立成區、
 * 缺口與建議與事實分開。兩邊各自渲染是必要的（一邊是 Rust 產字串、一邊是
 * React），但分區規則只有一份定義，寫在這個檔案的 `sectionOf` 與
 * `document.rs` 的 `section_of`，有測試守著兩邊一致。
 *
 * 這裡不做逃逸處理：React 對插入的字串本來就會轉義。危險的是 `dangerouslySetInnerHTML`
 * 與 href，前者一次都沒有用到，後者只允許 http/https。
 */
import type { ClaimKind, DocumentBlock } from '../session';
import { mmss } from '../session';

/* ── 區塊內容（§10 的 tagged union，與 Rust 的 BlockContent 對應） ─── */

export type BlockContent =
  | { type: 'heading'; level: number; text: string }
  | { type: 'text'; text: string }
  | { type: 'bullets'; items: string[] }
  | { type: 'table'; headers: string[]; rows: string[][] }
  | { type: 'mermaid'; source: string }
  | { type: 'callout'; tone: string; title: string; body: string }
  | { type: 'actionItem'; text: string; owner?: string | null; due?: string | null }
  | { type: 'excerpt'; speaker: string; text: string; meetingTimeMs: number }
  | { type: 'link'; label: string; target: string };

/** 解不開就回 null，讓呼叫端明確處理，不生一個空區塊假裝沒事。 */
export function parseContent(raw: string): BlockContent | null {
  try {
    const c = JSON.parse(raw) as BlockContent;
    return c && typeof c.type === 'string' ? c : null;
  } catch {
    return null;
  }
}

export type Section = 'decisions' | 'open' | 'body';

/** 與 `document.rs` 的 `section_of` 同一份規則。 */
export function sectionOf(kind: string, claimKind: ClaimKind): Section {
  if (kind === 'decision' || kind === 'actionItem') return 'decisions';
  if (claimKind === 'gap' || claimKind === 'suggestion') return 'open';
  return 'body';
}

/** 成果摘要用 tone 為 summary 的 Callout 表達，不新增區塊種類。 */
export function isSummary(c: BlockContent | null): boolean {
  return c?.type === 'callout' && c.tone === 'summary';
}

/* ── 引用 ────────────────────────────────────────────────────── */

/** 引用要指回的片段。兩個呼叫端的片段形狀不同，各自映射成這個最小形狀。 */
export interface CitedSegment {
  id: string;
  meetingTimeMs: number;
  revision: number;
}

const CLAIM_LABEL: Record<ClaimKind, string> = {
  fact: '事實',
  inference: '推論',
  suggestion: '建議',
  gap: '缺口',
};

function Cites({
  block,
  segments,
  onCite,
}: {
  block: DocumentBlock;
  segments: Map<string, CitedSegment>;
  onCite?: (sourceId: string) => void;
}) {
  if (block.sourceRefs.length === 0) return null;
  return (
    <span className="cites">
      {block.sourceRefs.map((r, i) => {
        const cited = segments.get(r.sourceId);
        // 這個片段在生成之後又被改過，引用指向的內容已不是當時看到的
        const stale =
          r.sourceKind === 'transcript_segment' &&
          cited !== undefined &&
          cited.revision > r.sourceRevision;
        // 用時間指路而不是內部識別碼：segment id 對使用者沒有意義，
        // 而且系統音訊軌的編號從 2^32 起跳，畫面上會出現十位數字
        const label = cited
          ? `${mmss(cited.meetingTimeMs)} r${r.sourceRevision}`
          : `${r.sourceKind} ${r.sourceId} r${r.sourceRevision}`;
        // 只有逐字稿引用跳得回去。筆記與附件同樣有合法的 id，但畫面上沒有
        // 它們的錨點 —— 跳到「第 7 段逐字稿」那個完全無關的位置比不跳更糟。
        if (r.sourceKind !== 'transcript_segment') {
          return (
            <span className="cite num" key={i} title={`${r.sourceKind}：${r.quotedText}`}>
              {label}
            </span>
          );
        }
        return (
          <a
            className="cite num"
            key={i}
            href={`#seg-${r.sourceId}`}
            // 逐字稿可能不在畫面上（摘要與逐字稿共用同一欄），純錨點會什麼都不做。
            // 交給呼叫端切換並捲動，它才知道自己的版面。
            onClick={
              onCite &&
              ((e) => {
                e.preventDefault();
                onCite(r.sourceId);
              })
            }
            data-stale={stale ? '1' : '0'}
            title={
              stale
                ? `此引用依據的片段已被修訂，內容可能與生成當時不同（seg ${r.sourceId}）：${r.quotedText}`
                : `來源可回溯，不代表此陳述已被驗證為真（seg ${r.sourceId}）：${r.quotedText}`
            }
          >
            {label}
          </a>
        );
      })}
    </span>
  );
}

/* ── 單一區塊 ────────────────────────────────────────────────── */

function Body({ c, anchor }: { c: BlockContent; anchor?: string }) {
  switch (c.type) {
    case 'heading': {
      // level 1..=4 由後端 schema 保證，這裡只負責對應到畫面層級
      const Tag = (['h3', 'h4', 'h5', 'h6'][Math.min(Math.max(c.level, 1), 4) - 1] ??
        'h4') as 'h3';
      return (
        <Tag className="doc-h" id={anchor}>
          {c.text}
        </Tag>
      );
    }
    case 'text':
      return <p className="doc-p">{c.text}</p>;
    case 'bullets':
      return (
        <ul className="doc-ul">
          {c.items.map((i, n) => (
            <li key={n}>{i}</li>
          ))}
        </ul>
      );
    case 'table':
      return (
        <div className="doc-table-wrap">
          <table className="doc-table">
            <thead>
              <tr>
                {c.headers.map((h, n) => (
                  <th key={n}>{h}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {c.rows.map((r, n) => (
                <tr key={n}>
                  {r.map((cell, m) => (
                    <td key={m}>{cell}</td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    case 'mermaid':
      // 沒有內嵌 Mermaid runtime（§18 的決策）。原始碼照實顯示並說明它是什麼，
      // 比畫一個假的圖或靜默丟掉誠實。
      return (
        <details className="doc-mermaid">
          <summary>流程圖（Mermaid 原始碼）</summary>
          <pre>{c.source}</pre>
        </details>
      );
    case 'callout':
      return (
        <aside className="doc-callout" data-tone={c.tone}>
          <b>{c.title}</b>
          <p>{c.body}</p>
        </aside>
      );
    case 'actionItem':
      return (
        <p className="doc-action">
          <span>{c.text}</span>
          {/* 沒講好負責人就留空，不填「未指定」：那是沒人說過的值，
              而「這件事沒人認領」正是會後最需要看見的資訊 */}
          {c.owner && <span className="doc-chip">{c.owner}</span>}
          {c.due && <span className="doc-chip">{c.due}</span>}
        </p>
      );
    case 'excerpt':
      return (
        <blockquote className="doc-quote">
          <span className="num">{mmss(c.meetingTimeMs)}</span>
          <b>{c.speaker}</b>
          <p>{c.text}</p>
        </blockquote>
      );
    case 'link':
      // 白名單而不是黑名單：javascript: 的變形寫法擋不完
      return /^https?:\/\//i.test(c.target) ? (
        <a className="doc-link" href={c.target} target="_blank" rel="noopener noreferrer">
          {c.label}
        </a>
      ) : (
        <span className="doc-link" title={`已封鎖的連結目標：${c.target}`}>
          {c.label}
        </span>
      );
  }
}

function Blk({
  block,
  content,
  segments,
  onCite,
}: {
  block: DocumentBlock;
  content: BlockContent;
  segments: Map<string, CitedSegment>;
  onCite?: (sourceId: string) => void;
}) {
  return (
    <div className="doc-blk" data-kind={block.kind} data-claim={block.claimKind}>
      {/* 推論、建議與缺口要看得出來不是會議事實（§10）。
          事實不標記：滿版的「事實」標籤會把真正需要注意的那三種淹掉。 */}
      {block.claimKind !== 'fact' && (
        <span className="doc-claim">{CLAIM_LABEL[block.claimKind]}</span>
      )}
      <Body c={content} anchor={`doc-h-${block.position}`} />
      <Cites block={block} segments={segments} onCite={onCite} />
    </div>
  );
}

/* ── 整份文件 ────────────────────────────────────────────────── */

export interface DocumentViewProps {
  blocks: DocumentBlock[];
  /** 引用要指回的片段。給空陣列時引用退化成 `seg <id>`，仍然看得到。 */
  segments: CitedSegment[];
  /** 點引用時要做什麼。沒給就退回一般錨點跳轉。 */
  onCite?: (sourceId: string) => void;
}

export function DocumentView({ blocks, segments, onCite }: DocumentViewProps) {
  const index = new Map(segments.map((s) => [s.id, s]));
  const parsed = blocks.map((b) => ({ b, c: parseContent(b.content) }));

  const summaryAt = parsed.findIndex((x) => isSummary(x.c));
  const summary = summaryAt >= 0 ? parsed[summaryAt] : null;
  const rest = parsed.filter((_, i) => i !== summaryAt);

  const pick = (s: Section) => rest.filter((x) => sectionOf(x.b.kind, x.b.claimKind) === s);
  const body = pick('body');
  const decisions = pick('decisions').filter((x) => x.b.kind === 'decision');
  const actions = pick('decisions').filter((x) => x.b.kind === 'actionItem');
  const open = pick('open');

  // 解不開的區塊不是空區塊：把它算成 0 會讓「這一版沒有內容」與
  // 「這一版存壞了」看起來一樣
  const broken = parsed.filter((x) => x.c === null).length;

  const render = (rows: typeof parsed) =>
    rows.map(({ b, c }) =>
      c === null ? null : (
        <Blk key={b.position} block={b} content={c} segments={index} onCite={onCite} />
      ),
    );

  // 目錄與匯出檔用同一條規則（`document.rs` 的 `toc`）：只列真的存在的區，
  // 只有一個入口就不算目錄。畫面沒有目錄而匯出有，讀者會以為兩份文件的
  // 組織方式不一樣。
  const toc: { href: string; label: string; sub?: boolean }[] = [];
  if (summary?.c?.type === 'callout') toc.push({ href: 'doc-summary', label: '成果摘要' });
  if (body.length > 0) {
    toc.push({ href: 'doc-body', label: '主文' });
    for (const { b, c } of body) {
      if (c?.type === 'heading' && c.level <= 2) {
        toc.push({ href: `doc-h-${b.position}`, label: c.text, sub: true });
      }
    }
  }
  if (decisions.length > 0 || actions.length > 0)
    toc.push({ href: 'doc-decisions', label: '決議與行動項目' });
  if (open.length > 0) toc.push({ href: 'doc-open', label: '缺口與建議' });

  return (
    <div className="doc">
      {toc.length > 1 && (
        <nav className="doc-toc" aria-label="目錄">
          {/* 用按鈕捲動而不是 <a href="#…">：這是應用程式不是文件，改寫
              網址列的 hash 沒有人會想要。匯出的那份是靜態 HTML，那邊用錨點 */}
          {toc.map((t) => (
            <button
              key={t.href}
              type="button"
              data-sub={t.sub ? '1' : undefined}
              onClick={() =>
                document.getElementById(t.href)?.scrollIntoView({ behavior: 'smooth' })
              }
            >
              {t.label}
            </button>
          ))}
        </nav>
      )}

      {summary?.c?.type === 'callout' && (
        <section className="doc-tldr" id="doc-summary">
          <h3>成果摘要</h3>
          <p>{summary.c.body}</p>
          <Cites block={summary.b} segments={index} onCite={onCite} />
        </section>
      )}

      {body.length > 0 && (
        <section className="doc-sec" id="doc-body">
          {render(body)}
        </section>
      )}

      {(decisions.length > 0 || actions.length > 0) && (
        <section className="doc-sec" id="doc-decisions">
          <h3 className="doc-sec-head">決議與行動項目</h3>
          {/* 決議也帶自己的小標。匯出檔兩個群組各有一個 h3，畫面上只有
              行動項目有，於是同樣一份文件在兩邊看起來層級不同 */}
          {decisions.length > 0 && (
            <div className="doc-group">
              <span className="doc-group-head">決議</span>
              {render(decisions)}
            </div>
          )}
          {actions.length > 0 && (
            <div className="doc-group doc-actions">
              <span className="doc-group-head">行動項目</span>
              {render(actions)}
            </div>
          )}
        </section>
      )}

      {open.length > 0 && (
        <section className="doc-sec doc-open" id="doc-open">
          <h3 className="doc-sec-head">缺口與建議</h3>
          {render(open)}
        </section>
      )}

      {broken > 0 && (
        <p className="hint">
          有 {broken} 個區塊的內容無法解讀，可能是資料損壞。匯出的 HTML 也不會包含它們。
        </p>
      )}
    </div>
  );
}
