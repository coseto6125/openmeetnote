//! 成果文件：受控區塊、引用驗證與 HTML 渲染。
//!
//! 對應 BLUEPRINT.md §9.4、§9.6 與 §10。
//!
//! 這個模組承擔整套設計中唯一能被強制執行的防幻覺機制。三件事因此寫在
//! 程式裡，不寫在 Prompt 裡：
//!
//! 1. **引用驗證由程式做，不交給模型自評。** 模型說自己驗證過，只是又一段
//!    生成內容。
//! 2. **`claim_kind` 沒有預設值，缺漏就是驗證失敗。** 把缺漏當成 `Inference`
//!    會讓模型只要漏傳欄位就能繞過引用義務，整套機制 fail-open。
//! 3. **Renderer 只吃結構化區塊，不吃 HTML 字串。** 逐字稿與附件是不受信任
//!    的內容（§9.4），任何一條沒轉義的路徑都是注入點。
//!
//! 界限同樣要寫明：逐字比對只能證明引用確實存在於證據中，不能證明區塊的
//! 論述被引用內容所支持。UI 因此不得把引用標記呈現成「已驗證為真」。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::ClaimKind;
use crate::store::{MeetingId, SourceRef, Store};

/* ── 區塊種類（§10） ─────────────────────────────────────────────── */

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockKind {
    Heading,
    Paragraph,
    BulletList,
    Table,
    MermaidDiagram,
    Callout,
    Decision,
    ActionItem,
    Gap,
    Suggestion,
    SourceLink,
    TranscriptExcerpt,
}

/// §10 的全部區塊種類。schema 的 CHECK 清單必須與它一致，有測試守著。
#[cfg_attr(not(test), allow(dead_code))]
pub const ALL_BLOCK_KINDS: [BlockKind; 12] = [
    BlockKind::Heading,
    BlockKind::Paragraph,
    BlockKind::BulletList,
    BlockKind::Table,
    BlockKind::MermaidDiagram,
    BlockKind::Callout,
    BlockKind::Decision,
    BlockKind::ActionItem,
    BlockKind::Gap,
    BlockKind::Suggestion,
    BlockKind::SourceLink,
    BlockKind::TranscriptExcerpt,
];

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Heading => "heading",
            BlockKind::Paragraph => "paragraph",
            BlockKind::BulletList => "bulletList",
            BlockKind::Table => "table",
            BlockKind::MermaidDiagram => "mermaidDiagram",
            BlockKind::Callout => "callout",
            BlockKind::Decision => "decision",
            BlockKind::ActionItem => "actionItem",
            BlockKind::Gap => "gap",
            BlockKind::Suggestion => "suggestion",
            BlockKind::SourceLink => "sourceLink",
            BlockKind::TranscriptExcerpt => "transcriptExcerpt",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "heading" => BlockKind::Heading,
            "paragraph" => BlockKind::Paragraph,
            "bulletList" => BlockKind::BulletList,
            "table" => BlockKind::Table,
            "mermaidDiagram" => BlockKind::MermaidDiagram,
            "callout" => BlockKind::Callout,
            "decision" => BlockKind::Decision,
            "actionItem" => BlockKind::ActionItem,
            "gap" => BlockKind::Gap,
            "suggestion" => BlockKind::Suggestion,
            "sourceLink" => BlockKind::SourceLink,
            "transcriptExcerpt" => BlockKind::TranscriptExcerpt,
            _ => return None,
        })
    }

    /// 這種區塊只能承載某個 `claim_kind`（§10）。
    ///
    /// `Decision`、`ActionItem` 與 `TranscriptExcerpt` 本質上在陳述會議發生過
    /// 的事，因此固定為 `Fact`；`Gap` 與 `Suggestion` 固定為同名值。
    /// 其餘種類可以承載任何 `claim_kind`：同一張表格可能列已確認的報價，
    /// 也可能列 AI 建議的選項。
    pub fn required_claim_kind(self) -> Option<ClaimKind> {
        match self {
            BlockKind::Decision | BlockKind::ActionItem | BlockKind::TranscriptExcerpt => {
                Some(ClaimKind::Fact)
            }
            BlockKind::Gap => Some(ClaimKind::Gap),
            BlockKind::Suggestion => Some(ClaimKind::Suggestion),
            _ => None,
        }
    }

    /// 這種區塊失敗時可否降級成 `Paragraph`（§10）。
    ///
    /// `Fact` 區塊不可降級：降級會讓沒通過 §9.6 的內容以純文字照樣渲染，
    /// 等於繞過引用驗證。判準是 `claim_kind` 而不是區塊種類。
    pub fn may_degrade(claim: ClaimKind) -> bool {
        claim != ClaimKind::Fact
    }
}

/// Agent 回傳的一個區塊，尚未寫入資料庫。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    pub kind: BlockKind,
    /// 沒有 `#[serde(default)]`。缺這個欄位就是反序列化失敗，
    /// 而不是被補成某個值（§10）。
    pub claim_kind: ClaimKind,
    pub content: BlockContent,
    #[serde(default)]
    pub source_refs: Vec<SourceRef>,
}

/// 各區塊種類的內容。以 tagged enum 表達，寫入前依 kind 驗證。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum BlockContent {
    Heading {
        level: u8,
        text: String,
    },
    Text {
        text: String,
    },
    Bullets {
        items: Vec<String>,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Mermaid {
        source: String,
    },
    Callout {
        tone: String,
        title: String,
        body: String,
    },
    ActionItem {
        text: String,
        owner: Option<String>,
        due: Option<String>,
    },
    Excerpt {
        speaker: String,
        text: String,
        meeting_time_ms: u64,
    },
    Link {
        label: String,
        target: String,
    },
}

impl BlockContent {
    /// 降級成 Paragraph 時用的純文字（§10）。
    ///
    /// 盡力取出可讀內容而不是丟棄：降級的目的是保住資訊，
    /// 只是不再宣稱它有原本那種結構。
    pub fn plain_text(&self) -> String {
        match self {
            BlockContent::Heading { text, .. }
            | BlockContent::Text { text }
            | BlockContent::ActionItem { text, .. }
            | BlockContent::Excerpt { text, .. } => text.clone(),
            BlockContent::Bullets { items } => items.join("；"),
            BlockContent::Table { headers, rows } => {
                let mut out = headers.join("、");
                for r in rows {
                    out.push('；');
                    out.push_str(&r.join("、"));
                }
                out
            }
            BlockContent::Mermaid { source } => source.clone(),
            BlockContent::Callout { title, body, .. } => format!("{title}：{body}"),
            BlockContent::Link { label, target } => format!("{label}（{target}）"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// 區塊種類與內容形狀不搭，例如 Table 配 Text
    ContentShapeMismatch,
    /// 這種區塊的 claim_kind 被規格固定住，模型給了別的值
    ClaimKindNotAllowed {
        required: ClaimKind,
    },
    Empty(&'static str),
    OutOfRange(&'static str),
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaError::ContentShapeMismatch => write!(f, "區塊內容與種類不符"),
            SchemaError::ClaimKindNotAllowed { required } => {
                write!(f, "這種區塊的 claim_kind 必須是 {}", required.as_str())
            }
            SchemaError::Empty(what) => write!(f, "{what} 不得為空"),
            SchemaError::OutOfRange(what) => write!(f, "{what} 超出允許範圍"),
        }
    }
}

impl Block {
    /// 寫入前的 schema 驗證（§10）。
    pub fn validate(&self) -> Result<(), SchemaError> {
        if let Some(required) = self.kind.required_claim_kind() {
            if self.claim_kind != required {
                return Err(SchemaError::ClaimKindNotAllowed { required });
            }
        }
        match (self.kind, &self.content) {
            (BlockKind::Heading, BlockContent::Heading { level, text }) => {
                if !(1..=4).contains(level) {
                    return Err(SchemaError::OutOfRange("標題層級"));
                }
                non_empty(text, "標題文字")
            }
            (
                BlockKind::Paragraph | BlockKind::Decision | BlockKind::Gap | BlockKind::Suggestion,
                BlockContent::Text { text },
            ) => non_empty(text, "內文"),
            (BlockKind::BulletList, BlockContent::Bullets { items }) => {
                if items.is_empty() {
                    return Err(SchemaError::Empty("項目清單"));
                }
                items.iter().try_for_each(|i| non_empty(i, "清單項目"))
            }
            (BlockKind::Table, BlockContent::Table { headers, rows }) => {
                if headers.is_empty() {
                    return Err(SchemaError::Empty("表頭"));
                }
                // 欄數不齊的表格渲染出來會錯位，這是結構錯誤不是內容問題
                if rows.iter().any(|r| r.len() != headers.len()) {
                    return Err(SchemaError::OutOfRange("表格欄數"));
                }
                Ok(())
            }
            (BlockKind::MermaidDiagram, BlockContent::Mermaid { source }) => {
                non_empty(source, "圖表原始碼")
            }
            (BlockKind::Callout, BlockContent::Callout { title, body, .. }) => {
                non_empty(title, "提示標題").and_then(|()| non_empty(body, "提示內容"))
            }
            (BlockKind::ActionItem, BlockContent::ActionItem { text, .. }) => {
                non_empty(text, "行動項目")
            }
            (BlockKind::TranscriptExcerpt, BlockContent::Excerpt { text, .. }) => {
                non_empty(text, "逐字稿摘錄")
            }
            (BlockKind::SourceLink, BlockContent::Link { label, target }) => {
                non_empty(label, "連結文字").and_then(|()| non_empty(target, "連結目標"))
            }
            _ => Err(SchemaError::ContentShapeMismatch),
        }
    }
}

fn non_empty(s: &str, what: &'static str) -> Result<(), SchemaError> {
    if s.trim().is_empty() {
        Err(SchemaError::Empty(what))
    } else {
        Ok(())
    }
}

impl Block {
    /// 轉成可寫入資料庫的形狀。內容以 JSON 存（§10），因此 kind 與 content
    /// 的搭配必須先通過 `validate`，否則讀回來會解不開。
    pub fn to_stored(&self, position: u32) -> crate::store::DocumentBlock {
        crate::store::DocumentBlock {
            position,
            kind: self.kind.as_str().to_owned(),
            claim_kind: self.claim_kind,
            content: serde_json::to_string(&self.content).unwrap_or_else(|_| "{}".into()),
            source_refs: self.source_refs.clone(),
        }
    }

    /// 從資料庫讀回。解不開就是資料損壞，回 None 讓呼叫端明確處理，
    /// 不生一個空區塊出來假裝沒事。
    pub fn from_stored(b: &crate::store::DocumentBlock) -> Option<Self> {
        Some(Self {
            kind: BlockKind::parse(&b.kind)?,
            claim_kind: b.claim_kind,
            content: serde_json::from_str(&b.content).ok()?,
            source_refs: b.source_refs.clone(),
        })
    }
}

/* ── 引用驗證（§9.6） ────────────────────────────────────────────── */

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RefVerdict {
    Valid,
    /// identity 或版本不存在
    UnknownSource,
    /// 版本存在，但它是在本輪快照游標之後才產生的
    OutsideSnapshot,
    /// locator 落在該版本內容範圍之外
    LocatorOutOfRange,
    /// 引文不是該版本內容的子字串
    QuoteNotFound,
    /// 引文與它自己的雜湊不符，代表引用紀錄本身被改過
    HashMismatch,
}

impl RefVerdict {
    pub fn is_valid(self) -> bool {
        self == RefVerdict::Valid
    }

    pub fn as_status(self) -> &'static str {
        match self {
            RefVerdict::Valid => "valid",
            RefVerdict::UnknownSource | RefVerdict::OutsideSnapshot => "invalid",
            RefVerdict::LocatorOutOfRange | RefVerdict::QuoteNotFound => "invalid",
            RefVerdict::HashMismatch => "invalid",
        }
    }

    pub fn reason(self) -> &'static str {
        match self {
            RefVerdict::Valid => "來源可回溯",
            RefVerdict::UnknownSource => "找不到這個來源或版本",
            RefVerdict::OutsideSnapshot => "這個版本不在本輪快照的涵蓋範圍內",
            RefVerdict::LocatorOutOfRange => "位置超出該版本的內容長度",
            RefVerdict::QuoteNotFound => "引文不存在於該版本的內容中",
            RefVerdict::HashMismatch => "引文與其雜湊不符",
        }
    }
}

/// locator 的字元位移。`"12-48"`，附件另帶頁碼 `"p3/12-48"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharSpan {
    pub start: usize,
    pub end: usize,
}

/// 解析 locator。無法解析視為超出範圍，而不是略過檢查。
pub fn parse_locator(locator: &str) -> Option<CharSpan> {
    let span = locator.rsplit('/').next()?;
    let (a, b) = span.split_once('-')?;
    let (start, end) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (start <= end).then_some(CharSpan { start, end })
}

/// 引用比對用的正規化：只處理空白與全形半形，不做語意比對（§9.6）。
///
/// 兩件事必須折疊，否則同一句話會因為排版差異就比對失敗：
///
/// - **全形 ASCII 與全形空白。** 中文輸入法很容易混入全形標點，而各家
///   Provider 的正規化程度不一致。
/// - **中日韓字元之間的空白。** 中文沒有詞間分隔符，夾在漢字之間的空白
///   不帶資訊。拉丁字母之間的空白則相反，它就是詞界，因此保留。
pub fn normalize_for_match(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        let ch = match ch as u32 {
            // 全形 ASCII 區 U+FF01..U+FF5E 對應半形 U+0021..U+007E
            c @ 0xFF01..=0xFF5E => char::from_u32(c - 0xFEE0).unwrap_or(ch),
            // 全形空白
            0x3000 => ' ',
            _ => ch,
        };
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        // 只有拉丁詞界那種空白才留下來
        if pending_space {
            let prev = out.chars().next_back().unwrap_or(' ');
            if prev.is_ascii_alphanumeric() && ch.is_ascii_alphanumeric() {
                out.push(' ');
            }
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 驗證一筆引用。三項條件全部通過才算有效。
pub fn verify_ref(
    store: &Store,
    meeting: MeetingId,
    through_event_seq: u64,
    r: &SourceRef,
) -> crate::store::Result<RefVerdict> {
    // 條件三的前半：引文與其雜湊必須相符。這一項不需要查資料庫，
    // 先做可以在紀錄被竄改時給出更精確的原因。
    if sha256_hex(&r.quoted_text) != r.quoted_text_sha256 {
        return Ok(RefVerdict::HashMismatch);
    }

    let Some(ev) = store.evidence_text(meeting, &r.source_kind, &r.source_id, r.source_revision)?
    else {
        return Ok(RefVerdict::UnknownSource);
    };

    // 條件一：該版本必須落在本輪快照的涵蓋範圍內。
    // 生成期間錄音持續寫入，沒有這一項的話，Agent 可以引用它根本沒看過的內容。
    if ev.created_event_seq > through_event_seq {
        return Ok(RefVerdict::OutsideSnapshot);
    }

    // 條件二：locator 落在該版本的有效範圍內。
    // 以字元計算而不是位元組：中文一個字三個位元組，用位元組會全錯。
    let len = ev.text.chars().count();
    let Some(span) = parse_locator(&r.locator) else {
        return Ok(RefVerdict::LocatorOutOfRange);
    };
    if span.end > len {
        return Ok(RefVerdict::LocatorOutOfRange);
    }

    // 條件三的後半：正規化之後必須是子字串。
    // 這只證明引用存在於證據中，不證明區塊的論述被它支持。
    if !normalize_for_match(&ev.text).contains(&normalize_for_match(&r.quoted_text)) {
        return Ok(RefVerdict::QuoteNotFound);
    }
    Ok(RefVerdict::Valid)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockVerdict {
    pub position: u32,
    /// 這個區塊是否可以出現在成果中
    pub admitted: bool,
    pub reason: Option<String>,
    pub refs: Vec<RefVerdict>,
}

/// 驗證整批區塊，回傳可入選的區塊與判定結果。
///
/// 規則（§9.6、§10）：
/// - `Fact` 必須帶至少一筆引用，判準是 `claim_kind` 而非區塊種類。
/// - 任何一筆引用無效，該區塊即不得出現在成果中。
/// - 不做「降級成 Paragraph 照樣渲染」這種處置：那等於繞過驗證。
pub fn verify_blocks(
    store: &Store,
    meeting: MeetingId,
    through_event_seq: u64,
    blocks: &[Block],
) -> crate::store::Result<(Vec<Block>, Vec<BlockVerdict>)> {
    let mut admitted = Vec::new();
    let mut verdicts = Vec::new();

    for (i, b) in blocks.iter().enumerate() {
        let position = i as u32;
        let mut reject = |reason: String, refs: Vec<RefVerdict>| {
            verdicts.push(BlockVerdict {
                position,
                admitted: false,
                reason: Some(reason),
                refs,
            });
        };

        if let Err(e) = b.validate() {
            reject(e.to_string(), Vec::new());
            continue;
        }
        if b.claim_kind.requires_citation() && b.source_refs.is_empty() {
            reject(
                format!("{} 區塊必須帶至少一筆引用", b.claim_kind.as_str()),
                Vec::new(),
            );
            continue;
        }

        let mut refs = Vec::with_capacity(b.source_refs.len());
        for r in &b.source_refs {
            refs.push(verify_ref(store, meeting, through_event_seq, r)?);
        }
        match refs.iter().position(|v| !v.is_valid()) {
            Some(i) => reject(refs[i].reason().to_owned(), refs),
            None => {
                // 判定結果寫回引用本身，之後 UI 與匯出讀的是驗證過的狀態，
                // 而不是模型自己填的 unverified
                let mut kept = b.clone();
                for (r, v) in kept.source_refs.iter_mut().zip(&refs) {
                    r.validation_status = v.as_status().to_owned();
                }
                verdicts.push(BlockVerdict {
                    position,
                    admitted: true,
                    reason: None,
                    refs,
                });
                admitted.push(kept);
            }
        }
    }
    Ok((admitted, verdicts))
}

/* ── HTML 渲染（§9.4、§10） ──────────────────────────────────────── */

/// HTML 文字轉義。
///
/// 逐字稿與附件是不受信任的內容，任何一條沒轉義的路徑都是注入點。
/// 單引號一併轉，因為屬性值也走這個函式。
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// 只允許安全的連結協定。
///
/// 白名單而不是黑名單：`javascript:` 的變形寫法太多，逐個擋不完，
/// 而這裡真正需要的協定只有三種。
fn safe_href(target: &str) -> String {
    let t = target.trim();
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || t.starts_with('#') {
        escape(t)
    } else {
        // 擋下來的連結仍然顯示原文，只是不可點：靜默改寫會讓使用者
        // 以為自己點到的是原本的目標
        String::from("#blocked")
    }
}

pub struct RenderContext<'a> {
    pub title: &'a str,
    pub version_no: u32,
    pub through_event_seq: u64,
    pub created_at: &'a str,
    /// 逐字稿附件。§10 要求匯出至少包含完整逐字稿或可選附件。
    pub transcript: &'a [crate::store::StoredSegment],
}

pub fn render_html(ctx: &RenderContext<'_>, blocks: &[Block]) -> String {
    let mut body = String::new();
    // Fact 與 Inference 混排會讓推論看起來像會議事實（§10），
    // 因此非事實內容集中到獨立區段。
    let (facts, others): (Vec<_>, Vec<_>) = blocks
        .iter()
        .partition(|b| matches!(b.claim_kind, ClaimKind::Fact | ClaimKind::Inference));

    for b in &facts {
        render_block(&mut body, b);
    }
    if !others.is_empty() {
        body.push_str("<section class=\"aside-claims\"><h2>缺口與建議</h2>");
        for b in &others {
            render_block(&mut body, b);
        }
        body.push_str("</section>");
    }

    let mut transcript = String::new();
    for s in ctx.transcript {
        transcript.push_str(&format!(
            "<div class=\"t-row\" id=\"seg-{id}-r{rev}\"><span class=\"t-time\">{time}</span>\
             <span class=\"t-who\">{who}</span><p>{text}</p></div>",
            id = s.segment_id,
            rev = s.revision,
            time = escape(&mmss(s.meeting_start_ms)),
            who = escape(s.speaker_id.as_deref().unwrap_or("未指派")),
            text = escape(&s.text),
        ));
    }

    format!(
        "<!doctype html><html lang=\"zh-Hant\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>{css}</style></head><body>\
         <header><h1>{title}</h1><p class=\"meta\">版本 v{ver}・涵蓋至事件 {seq}・生成於 {at}</p>\
         <p class=\"disclaimer\">引用標記代表來源可回溯，不代表該陳述已被驗證為真。</p></header>\
         <main>{body}</main>\
         <section class=\"transcript\"><h2>逐字稿</h2>{transcript}</section>\
         </body></html>",
        title = escape(ctx.title),
        ver = ctx.version_no,
        seq = ctx.through_event_seq,
        at = escape(ctx.created_at),
        css = EXPORT_CSS,
        body = body,
        transcript = transcript,
    )
}

fn mmss(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}", s / 60, s % 60)
}

fn render_block(out: &mut String, b: &Block) {
    let claim = b.claim_kind.as_str();
    out.push_str(&format!(
        "<div class=\"blk\" data-kind=\"{}\" data-claim=\"{claim}\">",
        b.kind.as_str()
    ));
    match &b.content {
        BlockContent::Heading { level, text } => {
            // 層級只允許 1..=4，validate 已擋掉其他值
            let h = (*level).clamp(1, 4) + 1;
            out.push_str(&format!("<h{h}>{}</h{h}>", escape(text)));
        }
        BlockContent::Text { text } => {
            out.push_str(&format!("<p>{}</p>", escape(text)));
        }
        BlockContent::Bullets { items } => {
            out.push_str("<ul>");
            for i in items {
                out.push_str(&format!("<li>{}</li>", escape(i)));
            }
            out.push_str("</ul>");
        }
        BlockContent::Table { headers, rows } => {
            out.push_str("<table><thead><tr>");
            for h in headers {
                out.push_str(&format!("<th>{}</th>", escape(h)));
            }
            out.push_str("</tr></thead><tbody>");
            for r in rows {
                out.push_str("<tr>");
                for c in r {
                    out.push_str(&format!("<td>{}</td>", escape(c)));
                }
                out.push_str("</tr>");
            }
            out.push_str("</tbody></table>");
        }
        BlockContent::Mermaid { source } => {
            // 原始碼一樣轉義。Mermaid runtime 讀的是文字節點，
            // 不轉義的話 </pre><script> 就能跳出容器（§9.4）。
            out.push_str(&format!("<pre class=\"mermaid\">{}</pre>", escape(source)));
        }
        BlockContent::Callout { tone, title, body } => {
            out.push_str(&format!(
                "<aside class=\"callout\" data-tone=\"{}\"><b>{}</b><p>{}</p></aside>",
                escape(tone),
                escape(title),
                escape(body)
            ));
        }
        BlockContent::ActionItem { text, owner, due } => {
            out.push_str(&format!("<p class=\"action\">{}", escape(text)));
            if let Some(o) = owner {
                out.push_str(&format!(" <span class=\"owner\">{}</span>", escape(o)));
            }
            if let Some(d) = due {
                out.push_str(&format!(" <span class=\"due\">{}</span>", escape(d)));
            }
            out.push_str("</p>");
        }
        BlockContent::Excerpt {
            speaker,
            text,
            meeting_time_ms,
        } => {
            out.push_str(&format!(
                "<blockquote><span class=\"t-time\">{}</span><b>{}</b><p>{}</p></blockquote>",
                escape(&mmss(*meeting_time_ms)),
                escape(speaker),
                escape(text)
            ));
        }
        BlockContent::Link { label, target } => {
            out.push_str(&format!(
                "<a href=\"{}\" rel=\"noopener noreferrer\">{}</a>",
                safe_href(target),
                escape(label)
            ));
        }
    }

    if !b.source_refs.is_empty() {
        out.push_str("<span class=\"cites\">");
        for r in &b.source_refs {
            out.push_str(&format!(
                "<a class=\"cite\" href=\"#seg-{id}-r{rev}\" title=\"{q}\">{kind} {id} r{rev}</a>",
                id = escape(&r.source_id),
                rev = r.source_revision,
                kind = escape(&r.source_kind),
                q = escape(&r.quoted_text),
            ));
        }
        out.push_str("</span>");
    }
    out.push_str("</div>");
}

const EXPORT_CSS: &str = "\
:root{color-scheme:light dark;--ink:#16181d;--muted:#6b7280;--line:#e3e5ea;--bg:#fff;--soft:#f6f7f9}\
@media(prefers-color-scheme:dark){:root{--ink:#e6e8ee;--muted:#9aa1ad;--line:#2a2d35;--bg:#14161a;--soft:#1b1e24}}\
*{box-sizing:border-box}body{margin:0 auto;padding:40px 24px;max-width:56rem;background:var(--bg);color:var(--ink);\
font:15px/1.7 system-ui,'Noto Sans TC',sans-serif}\
h1{font-size:26px;margin:0 0 6px}h2{font-size:18px;margin:28px 0 10px}h3{font-size:16px}\
.meta,.disclaimer{color:var(--muted);font-size:13px;margin:2px 0}\
.blk{margin:14px 0}\
.blk[data-claim='inference']{border-left:3px solid var(--line);padding-left:12px}\
.blk[data-claim='inference']::before{content:'推論';font-size:11px;color:var(--muted);display:block}\
.blk[data-claim='gap']::before{content:'缺口';font-size:11px;color:var(--muted);display:block}\
.blk[data-claim='suggestion']::before{content:'建議';font-size:11px;color:var(--muted);display:block}\
.aside-claims{margin-top:32px;padding-top:16px;border-top:1px solid var(--line)}\
table{border-collapse:collapse;width:100%}th,td{border:1px solid var(--line);padding:6px 9px;text-align:left}\
.callout{background:var(--soft);padding:12px 14px;border-radius:8px}\
blockquote{margin:0;padding:10px 14px;background:var(--soft);border-radius:8px}\
.cites{display:inline-flex;gap:6px;flex-wrap:wrap;margin-left:6px}\
.cite{font-size:11px;color:var(--muted);text-decoration:none;border:1px solid var(--line);\
border-radius:999px;padding:1px 7px}\
.transcript{margin-top:40px;border-top:1px solid var(--line);padding-top:16px}\
.t-row{display:grid;grid-template-columns:56px 96px 1fr;gap:10px;padding:4px 0}\
.t-time,.t-who{color:var(--muted);font-size:12px}\
.t-row p{margin:0}\
@media print{body{max-width:none}.cite{border:0}}";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::model::{Origin, Timeline, Track};
    use crate::store::{DomainEvent, SegmentRevision};

    fn store_with_evidence() -> (Store, MeetingId, u64) {
        let mut s = Store::new(db::open_in_memory().unwrap());
        let m = s.create_meeting("測試").unwrap();
        let seqs = s
            .append(
                m,
                &[
                    (
                        DomainEvent::TranscriptSegmentFinalized {
                            segment: SegmentRevision {
                                segment_id: 1,
                                revision: 1,
                                text: "報價要拆成設計、開發、維運三項".into(),
                                speaker_id: Some("s1".into()),
                                track: Track::System,
                                meeting_start_ms: 1000,
                                meeting_end_ms: 4000,
                                captured_start_ms: 1000,
                                captured_end_ms: 4000,
                                echo_likelihood: None,
                                overlap_group_id: None,
                                provider_stream_id: None,
                                provider_result_id: None,
                                rollover_generation: 0,
                                origin: Origin::Provider,
                                speaker_spans: Vec::new(),
                            },
                        },
                        Timeline::new(4000, 4000),
                    ),
                    (
                        DomainEvent::NoteAdded {
                            note_id: 1,
                            text: "客戶要求維運月費區間".into(),
                        },
                        Timeline::new(5000, 5000),
                    ),
                ],
            )
            .unwrap();
        let cursor = seqs[1];
        (s, m, cursor)
    }

    fn cite(kind: &str, id: &str, rev: u32, locator: &str, quote: &str) -> SourceRef {
        SourceRef {
            source_kind: kind.into(),
            source_id: id.into(),
            source_revision: rev,
            locator: locator.into(),
            quoted_text: quote.into(),
            quoted_text_sha256: sha256_hex(quote),
            validation_status: "unverified".into(),
        }
    }

    fn fact(text: &str, refs: Vec<SourceRef>) -> Block {
        Block {
            kind: BlockKind::Paragraph,
            claim_kind: ClaimKind::Fact,
            content: BlockContent::Text { text: text.into() },
            source_refs: refs,
        }
    }

    /* ── 引用驗證 ─────────────────────────────────────────────── */

    #[test]
    fn a_quote_that_exists_in_the_cited_revision_passes() {
        let (s, m, cursor) = store_with_evidence();
        let r = cite("transcript_segment", "1", 1, "0-15", "報價要拆成設計");
        assert_eq!(verify_ref(&s, m, cursor, &r).unwrap(), RefVerdict::Valid);
    }

    #[test]
    fn a_fabricated_quote_is_rejected_even_when_the_source_exists() {
        let (s, m, cursor) = store_with_evidence();
        let r = cite("transcript_segment", "1", 1, "0-10", "客戶同意打八折");
        assert_eq!(
            verify_ref(&s, m, cursor, &r).unwrap(),
            RefVerdict::QuoteNotFound
        );
    }

    #[test]
    fn a_revision_created_after_the_snapshot_cursor_is_out_of_scope() {
        let (mut s, m, _) = store_with_evidence();
        // 快照凍結在第一個事件，之後才產生的內容不得被引用
        let cursor = 1;
        s.append(
            m,
            &[(
                DomainEvent::NoteAdded {
                    note_id: 2,
                    text: "後來才記的".into(),
                },
                Timeline::new(9000, 9000),
            )],
        )
        .unwrap();
        let r = cite("note", "2", 3, "0-5", "後來才記的");
        assert_eq!(
            verify_ref(&s, m, cursor, &r).unwrap(),
            RefVerdict::OutsideSnapshot
        );
    }

    #[test]
    fn a_locator_past_the_end_of_the_revision_is_rejected() {
        let (s, m, cursor) = store_with_evidence();
        let r = cite("transcript_segment", "1", 1, "0-999", "報價要拆成設計");
        assert_eq!(
            verify_ref(&s, m, cursor, &r).unwrap(),
            RefVerdict::LocatorOutOfRange
        );
    }

    #[test]
    fn an_unparseable_locator_fails_instead_of_skipping_the_check() {
        let (s, m, cursor) = store_with_evidence();
        let r = cite("transcript_segment", "1", 1, "整段", "報價要拆成設計");
        assert_eq!(
            verify_ref(&s, m, cursor, &r).unwrap(),
            RefVerdict::LocatorOutOfRange
        );
    }

    #[test]
    fn a_tampered_citation_record_is_caught_by_its_own_hash() {
        let (s, m, cursor) = store_with_evidence();
        let mut r = cite("transcript_segment", "1", 1, "0-15", "報價要拆成設計");
        r.quoted_text = "報價要拆成設計、開發".into();
        assert_eq!(
            verify_ref(&s, m, cursor, &r).unwrap(),
            RefVerdict::HashMismatch
        );
    }

    #[test]
    fn a_note_citation_must_match_the_event_seq_that_created_it() {
        let (s, m, cursor) = store_with_evidence();
        let ok = cite("note", "1", 2, "0-6", "客戶要求維運月費區間");
        assert_eq!(verify_ref(&s, m, cursor, &ok).unwrap(), RefVerdict::Valid);
        // 版本對不上就不是同一份內容
        let wrong = cite("note", "1", 99, "0-6", "客戶要求維運月費區間");
        assert_eq!(
            verify_ref(&s, m, cursor, &wrong).unwrap(),
            RefVerdict::UnknownSource
        );
    }

    #[test]
    fn normalization_folds_width_and_whitespace_but_not_meaning() {
        assert_eq!(
            normalize_for_match("報價 ，  要拆"),
            normalize_for_match("報價, 要拆")
        );
        assert_eq!(normalize_for_match("ＡＰＩ"), "API");
        // 拉丁詞界不能被折掉，否則 "API call" 會等於 "APIcall"
        assert_eq!(normalize_for_match("the  API   call"), "the API call");
        assert_ne!(
            normalize_for_match("API call"),
            normalize_for_match("APIcall")
        );
        // 不同的話就是不同的話，正規化不做語意
        assert_ne!(normalize_for_match("同意"), normalize_for_match("不同意"));
    }

    /* ── 區塊規則 ─────────────────────────────────────────────── */

    #[test]
    fn a_fact_block_without_any_citation_is_not_admitted() {
        let (s, m, cursor) = store_with_evidence();
        let (ok, verdicts) =
            verify_blocks(&s, m, cursor, &[fact("會議決定拆成三項", vec![])]).unwrap();
        assert!(ok.is_empty());
        assert!(!verdicts[0].admitted);
        assert!(verdicts[0].reason.as_ref().unwrap().contains("引用"));
    }

    #[test]
    fn a_suggestion_needs_no_citation_and_is_admitted() {
        let (s, m, cursor) = store_with_evidence();
        let b = Block {
            kind: BlockKind::Suggestion,
            claim_kind: ClaimKind::Suggestion,
            content: BlockContent::Text {
                text: "建議在範圍書加入頁數上限".into(),
            },
            source_refs: vec![],
        };
        let (ok, _) = verify_blocks(&s, m, cursor, &[b]).unwrap();
        assert_eq!(ok.len(), 1);
    }

    #[test]
    fn a_decision_block_cannot_claim_to_be_an_inference() {
        let b = Block {
            kind: BlockKind::Decision,
            claim_kind: ClaimKind::Inference,
            content: BlockContent::Text {
                text: "決定採用方案 A".into(),
            },
            source_refs: vec![],
        };
        assert_eq!(
            b.validate(),
            Err(SchemaError::ClaimKindNotAllowed {
                required: ClaimKind::Fact
            })
        );
    }

    #[test]
    fn a_block_whose_content_shape_does_not_match_its_kind_is_rejected() {
        let b = Block {
            kind: BlockKind::Table,
            claim_kind: ClaimKind::Inference,
            content: BlockContent::Text {
                text: "不是表格".into(),
            },
            source_refs: vec![],
        };
        assert_eq!(b.validate(), Err(SchemaError::ContentShapeMismatch));
    }

    #[test]
    fn a_table_with_ragged_rows_is_rejected() {
        let b = Block {
            kind: BlockKind::Table,
            claim_kind: ClaimKind::Inference,
            content: BlockContent::Table {
                headers: vec!["項目".into(), "金額".into()],
                rows: vec![vec!["設計".into()]],
            },
            source_refs: vec![],
        };
        assert_eq!(b.validate(), Err(SchemaError::OutOfRange("表格欄數")));
    }

    #[test]
    fn a_missing_claim_kind_fails_to_deserialize_rather_than_defaulting() {
        // 模型漏傳 claimKind。補成 inference 會讓它繞過引用義務，
        // 因此這裡必須是反序列化失敗。
        let json = r#"{"kind":"paragraph","content":{"type":"text","text":"x"}}"#;
        assert!(serde_json::from_str::<Block>(json).is_err());
    }

    #[test]
    fn a_fact_block_may_not_degrade_to_a_paragraph() {
        assert!(!BlockKind::may_degrade(ClaimKind::Fact));
        assert!(BlockKind::may_degrade(ClaimKind::Inference));
        assert!(BlockKind::may_degrade(ClaimKind::Suggestion));
    }

    /* ── 渲染 ─────────────────────────────────────────────────── */

    fn render_one(b: Block) -> String {
        render_html(
            &RenderContext {
                title: "會議摘要",
                version_no: 1,
                through_event_seq: 10,
                created_at: "2026-08-01T00:00:00.000Z",
                transcript: &[],
            },
            &[b],
        )
    }

    #[test]
    fn transcript_content_is_escaped_not_executed() {
        let html = render_one(fact("<script>alert(1)</script>", vec![]));
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn mermaid_source_cannot_break_out_of_its_container() {
        let b = Block {
            kind: BlockKind::MermaidDiagram,
            claim_kind: ClaimKind::Inference,
            content: BlockContent::Mermaid {
                source: "graph TD</pre><script>alert(1)</script>".into(),
            },
            source_refs: vec![],
        };
        let html = render_one(b);
        assert!(!html.contains("</pre><script>"));
    }

    #[test]
    fn a_javascript_url_is_not_rendered_as_a_live_link() {
        let b = Block {
            kind: BlockKind::SourceLink,
            claim_kind: ClaimKind::Inference,
            content: BlockContent::Link {
                label: "看這裡".into(),
                target: "javascript:alert(1)".into(),
            },
            source_refs: vec![],
        };
        let html = render_one(b);
        assert!(!html.contains("javascript:"));
        assert!(html.contains("#blocked"));
    }

    #[test]
    fn inference_and_gap_are_rendered_apart_from_facts() {
        let (s, m, cursor) = store_with_evidence();
        let blocks = vec![
            fact(
                "報價拆成三項",
                vec![cite("transcript_segment", "1", 1, "0-15", "報價要拆成設計")],
            ),
            Block {
                kind: BlockKind::Gap,
                claim_kind: ClaimKind::Gap,
                content: BlockContent::Text {
                    text: "SLA 尚未談定".into(),
                },
                source_refs: vec![],
            },
        ];
        let (ok, _) = verify_blocks(&s, m, cursor, &blocks).unwrap();
        assert_eq!(ok.len(), 2);
        let html = render_html(
            &RenderContext {
                title: "會議摘要",
                version_no: 1,
                through_event_seq: cursor,
                created_at: "2026-08-01T00:00:00.000Z",
                transcript: &s.segments_through(m, cursor).unwrap(),
            },
            &ok,
        );
        // 找 class 屬性本身，不是 <style> 裡的同名選擇器
        let facts_end = html
            .find("<section class=\"aside-claims\"")
            .expect("缺口沒有獨立區段");
        assert!(html[..facts_end].contains("報價拆成三項"));
        assert!(html[facts_end..].contains("SLA 尚未談定"));
        // §10：匯出必須含逐字稿與版本資訊
        assert!(html.contains("報價要拆成設計、開發、維運三項"));
        assert!(html.contains("版本 v1"));
        assert!(html.contains("不代表該陳述已被驗證為真"));
    }

    #[test]
    fn a_block_survives_a_round_trip_through_the_database_shape() {
        let b = Block {
            kind: BlockKind::Table,
            claim_kind: ClaimKind::Inference,
            content: BlockContent::Table {
                headers: vec!["項目".into(), "金額".into()],
                rows: vec![vec!["設計".into(), "120000".into()]],
            },
            source_refs: vec![],
        };
        let stored = b.to_stored(0);
        assert_eq!(Block::from_stored(&stored), Some(b));
    }

    #[test]
    fn a_corrupt_stored_block_returns_none_instead_of_an_empty_block() {
        let bad = crate::store::DocumentBlock {
            position: 0,
            kind: "table".into(),
            claim_kind: ClaimKind::Fact,
            content: "{not json".into(),
            source_refs: vec![],
        };
        assert_eq!(Block::from_stored(&bad), None);
    }

    #[test]
    fn an_admitted_block_carries_its_verified_status_not_the_models_claim() {
        let (s, m, cursor) = store_with_evidence();
        let b = fact(
            "報價拆成三項",
            vec![cite("transcript_segment", "1", 1, "0-15", "報價要拆成設計")],
        );
        assert_eq!(b.source_refs[0].validation_status, "unverified");
        let (ok, _) = verify_blocks(&s, m, cursor, &[b]).unwrap();
        assert_eq!(ok[0].source_refs[0].validation_status, "valid");
    }

    #[test]
    fn plain_text_keeps_the_content_when_a_block_degrades() {
        let c = BlockContent::Table {
            headers: vec!["項目".into(), "金額".into()],
            rows: vec![vec!["設計".into(), "120000".into()]],
        };
        let t = c.plain_text();
        assert!(t.contains("設計") && t.contains("120000"));
    }

    #[test]
    fn every_block_kind_is_accepted_by_the_database_schema() {
        // 這個測試存在的理由：CHECK 清單與 BlockKind 分頭演化過一次，
        // 結果是生成完成的事件寫不進去，而畫面上已經顯示「已完成」。
        let (mut s, m, _) = store_with_evidence();
        let blocks: Vec<_> = ALL_BLOCK_KINDS
            .iter()
            .enumerate()
            .map(|(i, k)| crate::store::DocumentBlock {
                position: i as u32,
                kind: k.as_str().to_owned(),
                claim_kind: k.required_claim_kind().unwrap_or(ClaimKind::Inference),
                content: "{}".into(),
                source_refs: vec![],
            })
            .collect();
        s.append(
            m,
            &[
                (
                    crate::store::DomainEvent::SnapshotCreated {
                        document_id: 1,
                        run_id: 1,
                        parent_run_id: None,
                        version_no: 1,
                        purpose: "p".into(),
                        title: "t".into(),
                        through_event_seq: 1,
                        prompt: String::new(),
                    },
                    crate::model::Timeline::default(),
                ),
                (
                    crate::store::DomainEvent::GenerationCompleted {
                        run_id: 1,
                        blocks,
                        usage: serde_json::json!({}),
                    },
                    crate::model::Timeline::default(),
                ),
            ],
        )
        .expect("有區塊種類不被 schema 接受");
        assert_eq!(s.run_blocks(1).unwrap().len(), ALL_BLOCK_KINDS.len());
    }

    #[test]
    fn every_claim_kind_is_accepted_by_the_database_schema() {
        let (mut s, m, _) = store_with_evidence();
        for (i, ck) in [
            ClaimKind::Fact,
            ClaimKind::Inference,
            ClaimKind::Suggestion,
            ClaimKind::Gap,
        ]
        .into_iter()
        .enumerate()
        {
            let run_id = 10 + i as i64;
            s.append(
                m,
                &[
                    (
                        crate::store::DomainEvent::SnapshotCreated {
                            document_id: 1,
                            run_id,
                            parent_run_id: None,
                            version_no: 1 + i as u32,
                            purpose: "p".into(),
                            title: "t".into(),
                            through_event_seq: 1,
                            prompt: String::new(),
                        },
                        crate::model::Timeline::default(),
                    ),
                    (
                        crate::store::DomainEvent::GenerationCompleted {
                            run_id,
                            blocks: vec![crate::store::DocumentBlock {
                                position: 0,
                                kind: "paragraph".into(),
                                claim_kind: ck,
                                content: "{}".into(),
                                source_refs: vec![],
                            }],
                            usage: serde_json::json!({}),
                        },
                        crate::model::Timeline::default(),
                    ),
                ],
            )
            .unwrap_or_else(|e| panic!("{} 不被 schema 接受：{e}", ck.as_str()));
        }
    }
}
