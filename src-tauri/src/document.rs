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
        /// enum 上的 `rename_all` 只作用於 variant 名稱，欄位不會跟著轉。
        /// 這是唯一一個多字詞欄位，前端讀的是這份 JSON，因此明寫成 camelCase；
        /// alias 留給已經以 snake_case 存進資料庫的舊區塊。
        #[serde(rename = "meetingTimeMs", alias = "meeting_time_ms")]
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
            RefVerdict::LocatorOutOfRange => "位置是空區間或超出該版本的內容長度",
            RefVerdict::QuoteNotFound => "引文沒有實質內容，或不在位置指到的那一段裡",
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
///
/// 零寬區間（`start == end`）不算合法位置。`"0-0"` 指不到任何字元，卻曾經
/// 通過所有檢查 —— 見 [`verify_ref`] 的條件二。
pub fn parse_locator(locator: &str) -> Option<CharSpan> {
    let span = locator.rsplit('/').next()?;
    let (a, b) = span.split_once('-')?;
    let (start, end) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (start < end).then_some(CharSpan { start, end })
}

/// 引文至少要有這麼多個「有內容」的字元才算引用。
///
/// 標點與空白不計。單一個句號在任何一段中文逐字稿裡都找得到，它證明的
/// 是「這段話有標點」，不是「這件事有人說過」。
const MIN_QUOTE_CHARS: usize = 2;

/// 這段文字是否引得動。正規化後帶有資訊的字元要夠多，CJK 與拉丁字母
/// 都被 `is_alphanumeric` 涵蓋。
pub fn is_quotable(quote: &str) -> bool {
    normalize_for_match(quote)
        .chars()
        .filter(|c| c.is_alphanumeric())
        .count()
        >= MIN_QUOTE_CHARS
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

pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    let mut h = Sha256::new();
    h.update(bytes.as_ref());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 驗證一筆引用。三項條件全部通過才算有效。
pub fn verify_ref(
    store: &Store,
    meeting: MeetingId,
    through_event_seq: u64,
    r: &SourceRef,
) -> crate::store::Result<RefVerdict> {
    // 引文必須有實質內容。
    //
    // 起點是空引文：`contains("")` 永遠為真，於是任何一個 Fact 只要附上空字串
    // 引文與正確的空字串雜湊就能拿到 `valid`。但把門檻設在「非空」只擋掉了那
    // 個極端 —— `"。"` 一樣什麼都沒引到，而它在任何一段中文逐字稿裡都找得到。
    // 判準因此是正規化後的實質字元數，不是長度：全形空白與 CJK 之間的空白都
    // 不帶資訊，純標點也不帶。
    if !is_quotable(&r.quoted_text) {
        return Ok(RefVerdict::QuoteNotFound);
    }

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
    let chars: Vec<char> = ev.text.chars().collect();
    let Some(span) = parse_locator(&r.locator) else {
        return Ok(RefVerdict::LocatorOutOfRange);
    };
    if span.end > chars.len() {
        return Ok(RefVerdict::LocatorOutOfRange);
    }

    // 條件三的後半：正規化之後必須是 **locator 指到那一段** 的子字串。
    //
    // 比對整段 `ev.text` 的話，locator 就只是一個沒有人查的數字：引文可以
    // 落在片段的任何位置，位置與引文互相矛盾也照樣通過。要求引文出現在
    // locator 之內，才讓「這句話在這裡」這個宣稱本身可被機械檢查。
    //
    // 這仍然只證明引用存在於證據的那個位置，不證明區塊的論述被它支持。
    let cited: String = chars[span.start..span.end].iter().collect();
    if !normalize_for_match(&cited).contains(&normalize_for_match(&r.quoted_text)) {
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
    /// 快照當下的語者。匯出要顯示使用者確認過的名字，不是 `s1`。
    pub speakers: &'a [crate::store::StoredSpeaker],
}

/// 語者識別碼到顯示名稱的對照（§8.4）。
///
/// 規則與畫面端的 `speakerDisplayName`（src/meeting.ts）逐條相同：
/// 確認名 > 暫定名 > 麥克風軌是「我」、其餘是「語者 N」。兩個渲染器是刻意
/// 分開的，但顯示同一位語者時必須說出同一個名字 —— 畫面上是 Alice、匯出檔
/// 裡是 `s1`，使用者會以為那是兩個人。
///
/// 「語者 N」只數遠端語者：麥克風軌會佔掉一個 ordinal，跟著數的話遠端第一位
/// 就變成「語者 2」。
///
/// [`UNKNOWN_REMOTE`] 顯示成「遠端」，而且不佔編號 —— 它不是一個人（§8.1）。
/// 這一條排在確認名之前：那個識別碼底下可能有好幾位不同的人，讓其中一個名字
/// 蓋住全部，等於把他們併成一位。
///
/// 摘要的 `EvidenceIndex` 也呼叫這裡（`agent::build_index`）。三個渲染器各自
/// 實作過一次，結果同一位語者在畫面上是「遠端」、匯出檔裡是「語者 1」、送進
/// 模型的 prompt 裡是 `remote`。
///
/// [`UNKNOWN_REMOTE`]: crate::model::UNKNOWN_REMOTE
pub(crate) fn speaker_names(
    speakers: &[crate::store::StoredSpeaker],
    transcript: &[crate::store::StoredSegment],
) -> std::collections::HashMap<String, String> {
    use crate::model::Track;
    let mut track_of: std::collections::HashMap<&str, Track> = std::collections::HashMap::new();
    for s in transcript {
        if let Some(id) = s.speaker_id.as_deref() {
            track_of.entry(id).or_insert(s.track);
        }
    }
    let mut remote = 0u32;
    speakers
        .iter()
        .map(|s| {
            let track = track_of
                .get(s.speaker_id.as_str())
                .copied()
                .unwrap_or(Track::System);
            if s.speaker_id == crate::model::UNKNOWN_REMOTE {
                return (s.speaker_id.clone(), "遠端".to_owned());
            }
            if track == Track::System {
                remote += 1;
            }
            let name = s
                .confirmed_name
                .clone()
                .or_else(|| s.proposed_name.clone())
                .unwrap_or_else(|| match track {
                    Track::Mic => "我".to_owned(),
                    Track::System => format!("語者 {remote}"),
                });
            (s.speaker_id.clone(), name)
        })
        .collect()
}

/// 匯出文件的分區。§10 規定匯出至少要有哪幾塊，這個 enum 就是那份清單。
///
/// 分區由區塊自己的 `kind` 與 `claim_kind` 決定，不靠模型指定順序。模型
/// 只要產出正確的區塊種類，文件結構就是對的；它把決議寫在最前面或最後面
/// 都不影響讀者看到的組織方式。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    /// 決議與行動項目。獨立成區是因為這兩種是會後唯一會被回頭查的東西
    Decisions,
    /// 缺口與 AI 建議。與事實分離是 §10 的要求，混排會讓推論看起來像事實
    Open,
    Body,
}

fn section_of(b: &Block) -> Section {
    match b.kind {
        BlockKind::Decision | BlockKind::ActionItem => Section::Decisions,
        _ => match b.claim_kind {
            ClaimKind::Gap | ClaimKind::Suggestion => Section::Open,
            ClaimKind::Fact | ClaimKind::Inference => Section::Body,
        },
    }
}

/// 成果摘要用 `tone` 為 `summary` 的 Callout 表達。
///
/// 不新增區塊種類，也不由渲染器自己生一段摘要出來：渲染器沒有能力摘要，
/// 隨手取主文第一段當摘要會產生一段沒人寫過的內容。模型沒給就不出現這一區。
fn is_summary(b: &Block) -> bool {
    matches!(&b.content, BlockContent::Callout { tone, .. } if tone == "summary")
}

pub fn render_html(ctx: &RenderContext<'_>, blocks: &[Block]) -> String {
    let rendered: std::collections::HashMap<String, u32> = ctx
        .transcript
        .iter()
        .map(|s| (s.segment_id.to_string(), s.revision))
        .collect();
    let summary_at = blocks.iter().position(is_summary);
    let rest: Vec<&Block> = blocks
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != summary_at)
        .map(|(_, b)| b)
        .collect();

    let mut summary = String::new();
    if let Some(i) = summary_at {
        if let BlockContent::Callout { body, .. } = &blocks[i].content {
            summary.push_str(&format!(
                "<section id=\"s-summary\" class=\"tldr\"><h2>成果摘要</h2><p>{}</p>",
                escape(body)
            ));
            render_cites(&mut summary, &blocks[i], &rendered);
            summary.push_str("</section>");
        }
    }

    // 主文的標題進目錄，因為那是模型依本輪目標規劃出來的結構（§9.2）。
    // 從 1 開始編號，讓錨點在文件內唯一而不依賴標題文字。
    let mut headings: Vec<(usize, u8, &str)> = Vec::new();
    let mut body = String::new();
    for (i, b) in rest.iter().enumerate() {
        if section_of(b) != Section::Body {
            continue;
        }
        if let BlockContent::Heading { level, text } = &b.content {
            if *level <= 2 {
                headings.push((i, *level, text));
            }
        }
        render_block(&mut body, b, Some(i), &rendered);
    }

    let mut decisions = String::new();
    let mut actions = String::new();
    for b in rest.iter().filter(|b| section_of(b) == Section::Decisions) {
        render_block(
            if b.kind == BlockKind::Decision {
                &mut decisions
            } else {
                &mut actions
            },
            b,
            None,
            &rendered,
        );
    }

    let mut open = String::new();
    for b in rest.iter().filter(|b| section_of(b) == Section::Open) {
        render_block(&mut open, b, None, &rendered);
    }

    let has_summary = !summary.is_empty();
    let has_body = !body.is_empty();
    let has_decisions = !decisions.is_empty() || !actions.is_empty();
    let has_open = !open.is_empty();

    let mut main = summary;
    if !body.is_empty() {
        main.push_str(&format!("<section id=\"s-body\">{body}</section>"));
    }
    if !decisions.is_empty() || !actions.is_empty() {
        main.push_str("<section id=\"s-decisions\"><h2>決議與行動項目</h2>");
        if !decisions.is_empty() {
            main.push_str(&format!("<h3>決議</h3>{decisions}"));
        }
        if !actions.is_empty() {
            main.push_str(&format!("<h3>行動項目</h3>{actions}"));
        }
        main.push_str("</section>");
    }
    if !open.is_empty() {
        main.push_str(&format!(
            "<section id=\"s-open\" class=\"aside-claims\"><h2>缺口與建議</h2>{open}</section>"
        ));
    }

    // 目錄只列真的存在的區。列出空區等於告訴讀者這份文件漏了東西，
    // 但沒有決議的會議本來就不該生出一個空的「決議」段落。
    let mut toc = String::new();
    let mut toc_entries = 0usize;
    let mut item = |href: &str, label: &str, sub: bool| {
        toc_entries += 1;
        toc.push_str(&format!(
            "<li{}><a href=\"#{href}\">{}</a></li>",
            if sub { " class=\"sub\"" } else { "" },
            escape(label)
        ));
    };
    if has_summary {
        item("s-summary", "成果摘要", false);
    }
    if has_body {
        item("s-body", "主文", false);
        for (i, _, text) in &headings {
            item(&format!("h-{i}"), text, true);
        }
    }
    if has_decisions {
        item("s-decisions", "決議與行動項目", false);
    }
    if has_open {
        item("s-open", "缺口與建議", false);
    }
    if !ctx.transcript.is_empty() {
        item("s-transcript", "逐字稿", false);
    }
    // 只有一個入口的目錄不是目錄，那只是把標題再寫一次。畫面端同一條規則。
    let nav = if toc_entries > 1 {
        format!("<nav class=\"toc\" aria-label=\"目錄\"><ul>{toc}</ul></nav>")
    } else {
        String::new()
    };
    let body = main;

    // 沒有逐字稿就不要留一個空的段落標題。目錄也不會列它，
    // 兩邊不一致的話讀者會以為逐字稿掉了。
    let names = speaker_names(ctx.speakers, ctx.transcript);
    let mut rows = String::new();
    for s in ctx.transcript {
        rows.push_str(&format!(
            "<div class=\"t-row\" id=\"seg-{id}\"><span class=\"t-time\">{time}</span>\
             <span class=\"t-who\" id=\"seg-{id}-r{rev}\">{who}</span><p>{text}</p></div>",
            id = s.segment_id,
            rev = s.revision,
            time = escape(&mmss(s.meeting_start_ms)),
            who = escape(
                s.speaker_id
                    .as_deref()
                    .and_then(|id| names.get(id))
                    .map_or("未指派", String::as_str)
            ),
            text = escape(&s.text),
        ));
    }
    let transcript = if rows.is_empty() {
        String::new()
    } else {
        format!("<section id=\"s-transcript\" class=\"transcript\"><h2>逐字稿</h2>{rows}</section>")
    };

    format!(
        "<!doctype html><html lang=\"zh-Hant\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>{css}</style></head><body>\
         <header><h1>{title}</h1><p class=\"meta\">版本 v{ver}・涵蓋至事件 {seq}・生成於 {at}</p>\
         <p class=\"disclaimer\">引用標記代表來源可回溯，不代表該陳述已被驗證為真。</p></header>\
         {nav}<main>{body}</main>{transcript}\
         </body></html>",
        title = escape(ctx.title),
        ver = ctx.version_no,
        seq = ctx.through_event_seq,
        at = escape(ctx.created_at),
        css = EXPORT_CSS,
        nav = nav,
        body = body,
        transcript = transcript,
    )
}

fn mmss(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// `anchor` 只在主文區給，讓目錄裡的標題連得過去。其他區的標題由分區
/// 標題負責導覽，不需要再多一組錨點。
fn render_block(out: &mut String, b: &Block, anchor: Option<usize>, rendered: Rendered<'_>) {
    let claim = b.claim_kind.as_str();
    out.push_str(&format!(
        "<div class=\"blk\" data-kind=\"{}\" data-claim=\"{claim}\">",
        b.kind.as_str()
    ));
    match &b.content {
        BlockContent::Heading { level, text } => {
            // 層級只允許 1..=4，validate 已擋掉其他值
            let h = (*level).clamp(1, 4) + 1;
            let id = match anchor {
                Some(i) if *level <= 2 => format!(" id=\"h-{i}\""),
                _ => String::new(),
            };
            out.push_str(&format!("<h{h}{id}>{}</h{h}>", escape(text)));
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
            // 負責人與期限缺了就不顯示，不填「未指定」：那是一個沒人說過的值，
            // 而「這件事沒有人認領」正是會後最需要看見的資訊。
            out.push_str(&format!("<p class=\"action\">{}", escape(text)));
            if owner.is_some() || due.is_some() {
                out.push_str("<span class=\"action-meta\">");
                if let Some(o) = owner {
                    out.push_str(&format!("<span class=\"owner\">{}</span>", escape(o)));
                }
                if let Some(d) = due {
                    out.push_str(&format!("<span class=\"due\">{}</span>", escape(d)));
                }
                out.push_str("</span>");
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

    render_cites(out, b, rendered);
    out.push_str("</div>");
}

/// 匯出裡逐字稿實際渲染的版本，`segment_id` → `revision`。
type Rendered<'a> = &'a std::collections::HashMap<String, u32>;

fn render_cites(out: &mut String, b: &Block, rendered: Rendered<'_>) {
    if b.source_refs.is_empty() {
        return;
    }
    out.push_str("<span class=\"cites\">");
    for r in &b.source_refs {
        // 錨點指向片段而不是「片段的某一版」。引用可以合法地指向舊版本
        // （§9.6 只要求它落在快照範圍內），但匯出的逐字稿只會有一版，
        // 帶版本的錨點在那種情況下會指向不存在的位置，點了什麼都不會發生。
        let stale = rendered
            .get(&r.source_id)
            .is_some_and(|now| *now > r.source_revision);
        let label = format!(
            "{kind} {id} r{rev}{mark}",
            id = escape(&r.source_id),
            rev = r.source_revision,
            kind = escape(&r.source_kind),
            // 引用之後那一段又被改過，讀者看到的逐字稿已經不是生成時的內容
            mark = if stale { "（已修訂）" } else { "" },
        );
        // 只有逐字稿引用在這份文件裡有落點。筆記與附件同樣有合法的 id，
        // 但匯出裡沒有它們的段落 —— `#seg-7` 不是指不到，是會指到「第 7 段
        // 逐字稿」那個完全無關的位置。指錯比指不到嚴重。
        if r.source_kind == "transcript_segment" {
            out.push_str(&format!(
                "<a class=\"cite\" href=\"#seg-{id}\" data-stale=\"{stale}\" title=\"{q}\">{label}</a>",
                id = escape(&r.source_id),
                q = escape(&r.quoted_text),
                stale = if stale { "1" } else { "0" },
            ));
        } else {
            out.push_str(&format!(
                "<span class=\"cite\" title=\"{q}\">{label}</span>",
                q = escape(&r.quoted_text),
            ));
        }
    }
    out.push_str("</span>");
}

const EXPORT_CSS: &str = "\
:root{color-scheme:light dark;--ink:#16181d;--muted:#6b7280;--line:#e3e5ea;--bg:#fff;--soft:#f6f7f9}\
@media(prefers-color-scheme:dark){:root{--ink:#e6e8ee;--muted:#9aa1ad;--line:#2a2d35;--bg:#14161a;--soft:#1b1e24}}\
*{box-sizing:border-box}body{margin:0 auto;padding:40px 24px;max-width:56rem;background:var(--bg);color:var(--ink);\
font:15px/1.7 system-ui,'Noto Sans TC',sans-serif}\
h1{font-size:26px;margin:0 0 6px}h2{font-size:18px;margin:28px 0 10px}h3{font-size:16px}\
.meta,.disclaimer{color:var(--muted);font-size:13px;margin:2px 0}\
.toc{margin:24px 0;padding:12px 16px;background:var(--soft);border-radius:8px}\
.toc ul{margin:0;padding:0;list-style:none}\
.toc li{margin:2px 0}.toc li.sub{padding-left:16px;font-size:13px}\
.toc a{color:inherit;text-decoration:none}.toc a:hover{text-decoration:underline}\
.tldr{margin:24px 0;padding:16px 18px;border-left:3px solid var(--violet,#6b5bd2);background:var(--soft);border-radius:0 8px 8px 0}\
.tldr h2{margin:0 0 6px;font-size:15px;color:var(--muted);letter-spacing:.04em}\
.tldr p{margin:0;font-size:16px}\
#s-decisions h3{margin:18px 0 6px;font-size:14px;color:var(--muted)}\
.action{display:flex;flex-wrap:wrap;align-items:baseline;gap:8px;margin:0}\
.action-meta{display:inline-flex;gap:6px}\
.owner,.due{font-size:12px;border:1px solid var(--line);border-radius:999px;padding:1px 8px;color:var(--muted)}\
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
.cite[data-stale='1']{border-style:dashed;border-color:var(--muted)}\
.transcript{margin-top:40px;border-top:1px solid var(--line);padding-top:16px}\
.t-row{display:grid;grid-template-columns:56px 96px 1fr;gap:10px;padding:4px 0}\
.t-time,.t-who{color:var(--muted);font-size:12px}\
.t-row p{margin:0}\
@media print{body{max-width:none}.cite{border:0}}";

#[cfg(test)]
mod tests {
    /* ── 渲染的注入防線（§9.4） ──────────────────────────────────── */
    //
    // 逐字稿與筆記是不受信任的內容：任何人在會議裡念出一段 HTML，
    // 它就會走完整條管線進到匯出檔。這幾個測試守的是那條邊界。

    fn ctx() -> RenderContext<'static> {
        RenderContext {
            title: "測試會議",
            version_no: 1,
            through_event_seq: 10,
            created_at: "2026-08-05T00:00:00Z",
            transcript: &[],
            speakers: &[],
        }
    }

    fn block(kind: BlockKind, content: BlockContent) -> Block {
        Block {
            kind,
            claim_kind: ClaimKind::Inference,
            content,
            source_refs: vec![],
        }
    }

    #[test]
    fn test_script_tags_in_content_cannot_escape_into_markup() {
        let html = render_html(
            &ctx(),
            &[block(
                BlockKind::Paragraph,
                BlockContent::Text {
                    text: "<script>alert(1)</script>".into(),
                },
            )],
        );
        assert!(!html.contains("<script>"), "腳本標籤原樣進了輸出");
        assert!(html.contains("&lt;script&gt;"), "沒有被轉義成文字");
    }

    #[test]
    fn test_javascript_urls_are_blocked_but_still_visible() {
        let html = render_html(
            &ctx(),
            &[block(
                BlockKind::SourceLink,
                BlockContent::Link {
                    label: "看起來正常的連結".into(),
                    target: "javascript:alert(1)".into(),
                },
            )],
        );
        assert!(!html.contains("javascript:"), "危險協定進了 href");
        assert!(html.contains("#blocked"), "擋下來的連結沒有改成安全目標");
        // 標籤文字仍要顯示：靜默移除會讓使用者不知道文件裡本來有個連結
        assert!(html.contains("看起來正常的連結"));
    }

    #[test]
    fn test_a_link_target_cannot_break_out_of_the_href_attribute() {
        let html = render_html(
            &ctx(),
            &[block(
                BlockKind::SourceLink,
                BlockContent::Link {
                    label: "x".into(),
                    target: "https://example.com\" onclick=\"alert(1)".into(),
                },
            )],
        );
        // 轉義之後 onclick= 這幾個字元還在，但前面的引號已經是 &quot;，
        // 構不成屬性。要檢查的是「有沒有形成屬性」而不是「字串在不在」。
        assert!(!html.contains("\" onclick="), "屬性被跳脫出來了");
        assert!(html.contains("&quot;"), "引號沒有被轉義");
    }

    #[test]
    fn test_mermaid_source_is_escaped_so_it_cannot_close_its_container() {
        let html = render_html(
            &ctx(),
            &[block(
                BlockKind::MermaidDiagram,
                BlockContent::Mermaid {
                    source: "graph TD</pre><script>alert(1)</script><pre>".into(),
                },
            )],
        );
        assert!(!html.contains("<script>"), "從 mermaid 容器跳出去了");
        assert!(html.contains("&lt;/pre&gt;"), "結束標籤沒有被轉義");
    }

    #[test]
    fn test_a_callout_tone_cannot_inject_an_attribute() {
        let html = render_html(
            &ctx(),
            &[block(
                BlockKind::Callout,
                BlockContent::Callout {
                    tone: "warn\" onload=\"alert(1)".into(),
                    title: "標題".into(),
                    body: "內容".into(),
                },
            )],
        );
        assert!(!html.contains("\" onload="), "屬性值被跳脫出來了");
        assert!(html.contains("&quot;"), "引號沒有被轉義");
    }

    #[test]
    fn test_transcript_text_is_escaped_in_the_export() {
        // 逐字稿是最容易被塞東西的地方：講出來就會進來
        let seg = crate::store::StoredSegment {
            segment_id: 1,
            revision: 1,
            origin: crate::model::Origin::Provider,
            speaker_id: Some("<img src=x onerror=alert(1)>".into()),
            text: "<b>粗體</b>".into(),
            track: crate::model::Track::Mic,
            meeting_start_ms: 0,
            meeting_end_ms: 1000,
            user_edited: false,
        };
        // 名字現在來自語者表，注入點跟著移到那裡
        let speakers = vec![crate::store::StoredSpeaker {
            speaker_id: "<img src=x onerror=alert(1)>".into(),
            ordinal: 1,
            proposed_name: None,
            confirmed_name: Some("<script>alert(1)</script>".into()),
            status: "confirmed".into(),
        }];
        let c = RenderContext {
            transcript: std::slice::from_ref(&seg),
            speakers: &speakers,
            ..ctx()
        };
        let html = render_html(&c, &[]);
        assert!(!html.contains("<img "), "語者識別碼可以注入元素");
        assert!(!html.contains("<script>alert"), "語者名稱可以注入元素");
        assert!(!html.contains("<b>粗體</b>"), "逐字稿內容沒有被轉義");
        assert!(html.contains("&lt;b&gt;"), "轉義後的內容不見了");
    }

    /// 匯出檔要顯示使用者確認過的名字，不是內部識別碼。
    ///
    /// 畫面上寫 Alice、匯出檔裡寫 `s1`，使用者會以為那是兩個人。名稱規則
    /// 與 src/meeting.ts 的 `speakerDisplayName` 是同一條。
    #[test]
    fn the_export_calls_a_speaker_what_the_screen_calls_them() {
        let seg = |id: &str, track| crate::store::StoredSegment {
            segment_id: 1,
            revision: 1,
            origin: crate::model::Origin::Provider,
            speaker_id: Some(id.into()),
            text: "說了一句話".into(),
            track,
            meeting_start_ms: 0,
            meeting_end_ms: 1000,
            user_edited: false,
        };
        let speaker = |id: &str, ordinal, confirmed: Option<&str>| crate::store::StoredSpeaker {
            speaker_id: id.into(),
            ordinal,
            proposed_name: None,
            confirmed_name: confirmed.map(str::to_owned),
            status: "confirmed".into(),
        };
        let transcript = vec![
            seg("me", crate::model::Track::Mic),
            seg("s1", crate::model::Track::System),
            seg("s2", crate::model::Track::System),
        ];
        let speakers = vec![
            speaker("me", 1, None),
            speaker("s1", 2, Some("Alice")),
            speaker("s2", 3, None),
        ];
        let c = RenderContext {
            transcript: &transcript,
            speakers: &speakers,
            ..ctx()
        };
        let html = render_html(&c, &[]);
        assert!(html.contains("Alice"), "確認過的名字沒有進匯出檔");
        assert!(!html.contains(">s1<"), "還在顯示內部識別碼");
        assert!(html.contains("我"), "麥克風軌沒有顯示成「我」");
        // 「語者 N」只數遠端語者：麥克風軌佔掉一個 ordinal，跟著數的話
        // 未命名的遠端語者會從「語者 2」開始
        assert!(
            html.contains("語者 2"),
            "遠端語者的編號跟著麥克風軌一起數了"
        );
    }

    /// `remote` 與 `s1` 同時在場時，三個渲染器要說出同一組名字。
    fn remote_and_one_named_speaker() -> std::collections::HashMap<String, String> {
        use crate::model::{Track, UNKNOWN_REMOTE};
        let seg = |id: &str, track| crate::store::StoredSegment {
            segment_id: 1,
            revision: 1,
            origin: crate::model::Origin::Provider,
            speaker_id: Some(id.into()),
            text: "說了一句話".into(),
            track,
            meeting_start_ms: 0,
            meeting_end_ms: 1000,
            user_edited: false,
        };
        let speaker = |id: &str, ordinal| crate::store::StoredSpeaker {
            speaker_id: id.into(),
            ordinal,
            proposed_name: None,
            confirmed_name: None,
            status: "proposed".into(),
        };
        speaker_names(
            &[speaker(UNKNOWN_REMOTE, 1), speaker("s1", 2)],
            &[seg(UNKNOWN_REMOTE, Track::System), seg("s1", Track::System)],
        )
    }

    #[test]
    fn test_the_unidentified_remote_speaker_is_not_numbered_in_the_export() {
        // 畫面上是「遠端」「語者 1」。匯出自己數編號的話會變成「語者 1」
        // 「語者 2」，同一場會議於是在兩個地方看起來像有不同的人。
        let names = remote_and_one_named_speaker();
        assert_eq!(names[crate::model::UNKNOWN_REMOTE], "遠端");
        assert_eq!(names["s1"], "語者 1", "「遠端」佔掉了一個編號");
    }

    #[test]
    fn test_a_name_stored_on_the_remote_sentinel_does_not_take_effect() {
        // 舊資料裡可能已經有人替「遠端」命過名。那個識別碼底下是好幾個
        // 不同的人，讓那個名字生效等於把他們全部併成一位。
        use crate::model::{Track, UNKNOWN_REMOTE};
        let names = speaker_names(
            &[crate::store::StoredSpeaker {
                speaker_id: UNKNOWN_REMOTE.into(),
                ordinal: 1,
                proposed_name: None,
                confirmed_name: Some("Alice".into()),
                status: "confirmed".into(),
            }],
            &[crate::store::StoredSegment {
                segment_id: 1,
                revision: 1,
                origin: crate::model::Origin::Provider,
                speaker_id: Some(UNKNOWN_REMOTE.into()),
                text: "說了一句話".into(),
                track: Track::System,
                meeting_start_ms: 0,
                meeting_end_ms: 1000,
                user_edited: false,
            }],
        );
        assert_eq!(names[UNKNOWN_REMOTE], "遠端");
    }

    #[test]
    fn test_the_title_is_escaped_too() {
        // 會議標題可以改名，那也是使用者輸入
        let c = RenderContext {
            title: "</title><script>alert(1)</script>",
            ..ctx()
        };
        let html = render_html(&c, &[]);
        assert!(!html.contains("<script>"), "標題可以跳出 title 元素");
    }

    #[test]
    fn test_inferences_are_separated_from_facts_in_the_export() {
        // 推論混在事實裡排版，讀的人會把它當成會議事實（§10）
        let html = render_html(
            &ctx(),
            &[
                Block {
                    kind: BlockKind::Paragraph,
                    claim_kind: ClaimKind::Fact,
                    content: BlockContent::Text {
                        text: "這是事實".into(),
                    },
                    source_refs: vec![],
                },
                Block {
                    kind: BlockKind::Gap,
                    claim_kind: ClaimKind::Gap,
                    content: BlockContent::Text {
                        text: "這是缺口".into(),
                    },
                    source_refs: vec![],
                },
            ],
        );
        let fact_at = html.find("這是事實").expect("事實應該出現");
        // 找 section 標籤本身，不是樣式表裡同名的選擇器
        let aside_at = html
            .find("<section id=\"s-open\"")
            .expect("缺口區段應該存在");
        let gap_at = html.find("這是缺口").expect("缺口應該出現");
        assert!(
            fact_at < aside_at,
            "事實排在缺口區段之後了 fact={fact_at} aside={aside_at}"
        );
        assert!(
            aside_at < gap_at,
            "缺口沒有落在獨立區段裡 aside={aside_at} gap={gap_at}"
        );
    }

    /* ── §10 的分區（成果摘要、主文、決議與行動項目） ─────────────── */

    fn doc() -> Vec<Block> {
        vec![
            // 刻意把摘要放在中間、決議放在最前面：分區由渲染器決定，
            // 不該依賴模型剛好照順序輸出
            Block {
                kind: BlockKind::Decision,
                claim_kind: ClaimKind::Fact,
                content: BlockContent::Text {
                    text: "決議凍結兩百萬元".into(),
                },
                source_refs: vec![],
            },
            Block {
                kind: BlockKind::Callout,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Callout {
                    tone: "summary".into(),
                    title: "成果摘要".into(),
                    body: "本次會議審查預算案。".into(),
                },
                source_refs: vec![],
            },
            Block {
                kind: BlockKind::Heading,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Heading {
                    level: 1,
                    text: "預算審查".into(),
                },
                source_refs: vec![],
            },
            Block {
                kind: BlockKind::Paragraph,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Text {
                    text: "主文內容".into(),
                },
                source_refs: vec![],
            },
            Block {
                kind: BlockKind::ActionItem,
                claim_kind: ClaimKind::Fact,
                content: BlockContent::ActionItem {
                    text: "函請文化部表達意見".into(),
                    owner: Some("文化部".into()),
                    due: None,
                },
                source_refs: vec![],
            },
        ]
    }

    #[test]
    fn test_the_summary_callout_leads_the_document_wherever_the_model_put_it() {
        let html = render_html(&ctx(), &doc());
        let summary = html
            .find("<section id=\"s-summary\"")
            .expect("沒有成果摘要區");
        let body = html.find("<section id=\"s-body\"").expect("沒有主文區");
        assert!(summary < body, "成果摘要沒有排在主文之前");
        // 摘要那一塊不該又在主文裡出現一次
        assert_eq!(
            html.matches("本次會議審查預算案。").count(),
            1,
            "摘要被渲染了兩次"
        );
    }

    #[test]
    fn test_decisions_and_action_items_get_their_own_section() {
        let html = render_html(&ctx(), &doc());
        let at = html
            .find("<section id=\"s-decisions\"")
            .expect("沒有決議區");
        let tail = &html[at..];
        assert!(tail.contains("決議凍結兩百萬元"), "決議沒有進決議區");
        assert!(tail.contains("函請文化部表達意見"), "行動項目沒有進決議區");
        assert!(tail.contains("<h3>決議</h3>") && tail.contains("<h3>行動項目</h3>"));
        // 負責人要看得見，否則沒人知道這件事歸誰
        assert!(tail.contains("文化部"), "行動項目的負責人不見了");
        // 決議不該同時留在主文裡
        assert!(!html[..at].contains("決議凍結兩百萬元"), "決議被渲染了兩次");
    }

    #[test]
    fn test_the_table_of_contents_only_lists_sections_that_exist() {
        let html = render_html(&ctx(), &doc());
        assert!(html.contains("<nav class=\"toc\""), "沒有目錄");
        for anchor in ["#s-summary", "#s-body", "#s-decisions"] {
            assert!(html.contains(anchor), "目錄少了 {anchor}");
        }
        // 這份文件沒有缺口，也沒有逐字稿
        assert!(!html.contains("#s-open"), "目錄列了不存在的缺口區");
        assert!(!html.contains("#s-transcript"), "目錄列了不存在的逐字稿");
        // 主文的標題要能被目錄連到
        assert!(html.contains("id=\"h-"), "主文標題沒有錨點");
    }

    #[test]
    fn test_a_citation_to_an_older_revision_still_has_somewhere_to_land() {
        // §9.6 允許引用快照範圍內的舊版本，但匯出的逐字稿只會有最新那一版。
        // 錨點若帶著版本號，這種引用就會指向文件裡不存在的位置，點了什麼
        // 都不會發生，而讀者無從得知自己點了一個死連結。
        let (mut s, m, _) = store_with_evidence();
        let seq = s
            .append(
                m,
                &[(
                    DomainEvent::TranscriptSegmentRevised {
                        segment: SegmentRevision {
                            segment_id: 1,
                            revision: 2,
                            text: "報價要拆成設計、開發、維運三項，這點下次再確認".into(),
                            speaker_id: Some("s1".into()),
                            track: Track::System,
                            meeting_start_ms: 0,
                            meeting_end_ms: 4000,
                            captured_start_ms: 0,
                            captured_end_ms: 4000,
                            echo_likelihood: None,
                            overlap_group_id: None,
                            provider_stream_id: None,
                            provider_result_id: None,
                            rollover_generation: 0,
                            origin: Origin::User,
                            speaker_spans: Vec::new(),
                        },
                    },
                    Timeline::new(4000, 4000),
                )],
            )
            .unwrap()
            .last()
            .copied()
            .unwrap();

        let quote = "報價要拆成";
        let block = Block {
            kind: BlockKind::Paragraph,
            claim_kind: ClaimKind::Fact,
            content: BlockContent::Text {
                text: "報價分三項".into(),
            },
            source_refs: vec![SourceRef {
                source_kind: "transcript_segment".into(),
                source_id: "1".into(),
                source_revision: 1,
                locator: "0-5".into(),
                quoted_text: quote.into(),
                quoted_text_sha256: sha256_hex(quote),
                validation_status: "unverified".into(),
            }],
        };
        let (ok, _) = verify_blocks(&s, m, seq, &[block]).unwrap();
        assert_eq!(ok.len(), 1, "引用舊版本本身應該是合法的");

        let transcript = s.segments_through(m, seq).unwrap();
        let html = render_html(
            &RenderContext {
                title: "修訂",
                version_no: 1,
                through_event_seq: seq,
                created_at: "2026-08-05T00:00:00Z",
                transcript: &transcript,
                speakers: &[],
            },
            &ok,
        );
        assert!(html.contains("href=\"#seg-1\""), "引用沒有指向片段本身");
        assert!(html.contains("id=\"seg-1\""), "逐字稿沒有可落地的錨點");
        // 讀者看到的是 r2 的文字，引用寫的是 r1，這個落差要講出來
        assert!(
            html.contains("data-stale=\"1\""),
            "沒有標示引用依據的版本已被修訂"
        );
        assert!(html.contains("（已修訂）"));
    }

    #[test]
    fn a_note_citation_does_not_pretend_to_point_at_the_transcript() {
        // 筆記與附件同樣有合法的 id，但匯出裡沒有它們的段落。`#seg-7` 不是
        // 指不到，是會指到「第 7 段逐字稿」那個完全無關的位置 —— 指錯比
        // 指不到嚴重。獨立審查找到的。
        let html = render_html(
            &ctx(),
            &[Block {
                kind: BlockKind::Paragraph,
                claim_kind: ClaimKind::Fact,
                content: BlockContent::Text {
                    text: "筆記裡寫的事".into(),
                },
                source_refs: vec![SourceRef {
                    source_kind: "note".into(),
                    source_id: "7".into(),
                    source_revision: 3,
                    locator: "0-4".into(),
                    quoted_text: "追維運報價".into(),
                    quoted_text_sha256: sha256_hex("追維運報價"),
                    validation_status: "valid".into(),
                }],
            }],
        );
        assert!(!html.contains("href=\"#seg-7\""), "筆記引用指到了逐字稿");
        // 但它仍要看得見，而且看得出來源是什麼
        assert!(html.contains("note 7"), "筆記引用整個不見了");
    }

    #[test]
    fn test_the_front_end_sections_the_document_the_same_way_this_module_does() {
        // 分區規則有兩份實作：這裡產匯出的 HTML，DocumentView.tsx 產畫面。
        // 兩邊分歧的話，使用者在畫面上看到的文件與他匯出的那份會是不同的組織
        // 方式，而那種落差不會有任何一邊報錯。這個測試盯的就是那件事。
        let tsx = std::fs::read_to_string("../src/components/DocumentView.tsx")
            .expect("找不到前端的文件渲染元件");
        let rule = tsx
            .split_once("export function sectionOf")
            .expect("前端沒有 sectionOf")
            .1;
        let rule = &rule[..rule.find('}').unwrap_or(rule.len())];
        for kind in ALL_BLOCK_KINDS {
            let routed = matches!(kind, BlockKind::Decision | BlockKind::ActionItem);
            assert_eq!(
                rule.contains(&format!("kind === '{}'", kind.as_str())),
                routed,
                "{} 在兩邊的分區規則裡不一致",
                kind.as_str()
            );
        }
        for claim in ["gap", "suggestion"] {
            assert!(
                rule.contains(&format!("claimKind === '{claim}'")),
                "前端沒有把 {claim} 分到缺口與建議"
            );
        }
    }

    #[test]
    fn test_a_document_without_a_summary_simply_has_no_summary_section() {
        // 渲染器不會自己生一段摘要出來充數
        let html = render_html(
            &ctx(),
            &[Block {
                kind: BlockKind::Paragraph,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Text {
                    text: "只有主文".into(),
                },
                source_refs: vec![],
            }],
        );
        assert!(!html.contains("s-summary"), "無中生有了一個成果摘要區");
        assert!(html.contains("只有主文"));
    }

    #[test]
    fn test_a_summary_callout_cannot_inject_markup_from_its_lead_position() {
        // 摘要走的是與其他區塊不同的渲染路徑，轉義必須各自守住
        let html = render_html(
            &ctx(),
            &[Block {
                kind: BlockKind::Callout,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Callout {
                    tone: "summary".into(),
                    title: "x".into(),
                    body: "<img src=x onerror=alert(1)>".into(),
                },
                source_refs: vec![],
            }],
        );
        assert!(!html.contains("<img"), "摘要區沒有轉義");
        assert!(html.contains("&lt;img"));
    }

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
    fn an_empty_quote_is_not_a_citation() {
        // `contains("")` 永遠為真。附上空引文與它正確的雜湊，任何一個 Fact
        // 都能拿到 valid —— 整套防幻覺機制唯一能被強制執行的那一環就此
        // fail-open。獨立審查找到的。
        let (s, m, cursor) = store_with_evidence();
        for empty in ["", "   ", "　", "\n\t"] {
            let r = cite("transcript_segment", "1", 1, "0-0", empty);
            assert_eq!(
                verify_ref(&s, m, cursor, &r).unwrap(),
                RefVerdict::QuoteNotFound,
                "空引文 {empty:?} 通過了驗證"
            );
        }
    }

    #[test]
    fn a_block_whose_only_citation_is_empty_is_not_admitted() {
        let (s, m, cursor) = store_with_evidence();
        let block = Block {
            kind: BlockKind::Paragraph,
            claim_kind: ClaimKind::Fact,
            content: BlockContent::Text {
                text: "客戶已同意八折".into(),
            },
            source_refs: vec![cite("transcript_segment", "1", 1, "0-0", "")],
        };
        let (ok, verdicts) = verify_blocks(&s, m, cursor, &[block]).unwrap();
        assert!(ok.is_empty(), "空引用的 Fact 進到成果了");
        assert!(!verdicts[0].admitted);
    }

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

    /// 零寬 locator 加一個標點，曾經是任意 Fact 的通行證。
    ///
    /// `"0-0"` 通過了 `span.end > len` 那道檢查（0 不大於任何長度），而引文
    /// 當時是拿去跟整段內容比對的，於是只要片段裡有一個頓號，捏造的決議就
    /// 拿到 `valid`。雜湊擋不住：那是系統自己補上去的。
    #[test]
    fn a_zero_width_locator_with_a_lone_punctuation_mark_no_longer_passes() {
        let (s, m, cursor) = store_with_evidence();
        let r = cite("transcript_segment", "1", 1, "0-0", "、");
        assert_ne!(verify_ref(&s, m, cursor, &r).unwrap(), RefVerdict::Valid);
        // 換成非零寬的 locator 也一樣：單一個標點什麼都沒引到
        let r = cite("transcript_segment", "1", 1, "5-6", "、");
        assert_eq!(
            verify_ref(&s, m, cursor, &r).unwrap(),
            RefVerdict::QuoteNotFound
        );
    }

    /// locator 必須框住引文，否則它就只是一個沒有人查的數字。
    #[test]
    fn a_quote_outside_the_locator_is_rejected_even_though_it_is_in_the_source() {
        let (s, m, cursor) = store_with_evidence();
        // 「維運三項」確實在片段裡，但它落在第 11 到 15 個字元
        let r = cite("transcript_segment", "1", 1, "0-5", "維運三項");
        assert_eq!(
            verify_ref(&s, m, cursor, &r).unwrap(),
            RefVerdict::QuoteNotFound
        );
        let r = cite("transcript_segment", "1", 1, "11-15", "維運三項");
        assert_eq!(verify_ref(&s, m, cursor, &r).unwrap(), RefVerdict::Valid);
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
        let ok = cite("note", "1", 2, "0-10", "客戶要求維運月費區間");
        assert_eq!(verify_ref(&s, m, cursor, &ok).unwrap(), RefVerdict::Valid);
        // 版本對不上就不是同一份內容
        let wrong = cite("note", "1", 99, "0-10", "客戶要求維運月費區間");
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
                speakers: &[],
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
                speakers: &s.speakers_through(m, cursor).unwrap(),
            },
            &ok,
        );
        // 找 class 屬性本身，不是 <style> 裡的同名選擇器
        let facts_end = html
            .find("<section id=\"s-open\"")
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
