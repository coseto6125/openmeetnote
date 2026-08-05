/**
 * 成果文件的分區規則（§10）。
 *
 * 這裡只測純函式。畫面元件的渲染由 Rust 端的 `render_html` 測試守住同一套
 * 規則，兩邊各測各的，因為它們是兩份實作。
 */
import { describe, expect, it } from 'vitest';
import { isSummary, parseContent, sectionOf } from './components/DocumentView';

describe('sectionOf', () => {
  it('把決議與行動項目送進獨立區段', () => {
    expect(sectionOf('decision', 'fact')).toBe('decisions');
    expect(sectionOf('actionItem', 'fact')).toBe('decisions');
  });

  it('依 claimKind 而不是區塊種類決定缺口與建議', () => {
    // 一張表格可能列的是 AI 建議的選項，那它就屬於「缺口與建議」
    expect(sectionOf('table', 'suggestion')).toBe('open');
    expect(sectionOf('paragraph', 'gap')).toBe('open');
    expect(sectionOf('table', 'fact')).toBe('body');
  });

  it('其餘都留在主文', () => {
    for (const kind of ['heading', 'paragraph', 'bulletList', 'callout', 'mermaidDiagram']) {
      expect(sectionOf(kind, 'inference')).toBe('body');
    }
  });
});

describe('parseContent', () => {
  it('解得開就回結構化內容', () => {
    const c = parseContent('{"type":"bullets","items":["a","b"]}');
    expect(c).toEqual({ type: 'bullets', items: ['a', 'b'] });
  });

  it('壞掉的內容回 null 而不是丟例外', () => {
    // 資料損壞與「這一版沒有內容」必須分得開
    expect(parseContent('{ 不是 JSON')).toBeNull();
    expect(parseContent('"只是一個字串"')).toBeNull();
    expect(parseContent('{"items":[]}')).toBeNull();
  });
});

describe('isSummary', () => {
  it('只認 tone 為 summary 的 callout', () => {
    expect(isSummary({ type: 'callout', tone: 'summary', title: 't', body: 'b' })).toBe(true);
    expect(isSummary({ type: 'callout', tone: 'warn', title: 't', body: 'b' })).toBe(false);
    expect(isSummary({ type: 'text', text: '這不是摘要' })).toBe(false);
    expect(isSummary(null)).toBe(false);
  });
});
