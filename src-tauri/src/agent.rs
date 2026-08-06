//! 通用 Agent Loop（BLUEPRINT.md §9）。
//!
//! 這個模組只實作確定性的部分：證據索引、Context 預算、迭代控制與停止條件。
//! 模型那一步在 `Planner` trait 後面，因為它是唯一無法保證輸出的環節。
//!
//! 三條規則寫在結構上：
//!
//! 1. **沒有固定方向映射（§9.1）。** 程式裡找不到「會議類型 → 文件模板」的
//!    對照。文件方向由本輪 Prompt、證據與成果目標共同形成，參考結構只是
//!    Planner 可以借用的素材。
//! 2. **引用驗證在迴圈裡，不在模型裡（§9.6）。** 每一輪草稿都要通過
//!    `document::verify_blocks`，沒通過的區塊不會進入下一步，也不會進成果。
//! 3. **證據裁切不得靜默（§9.5）。** 額度不足時在呼叫 Provider 之前就拒絕
//!    並說明原因，不啟用 Provider 端的自動截斷，那會從最前面開始丟。

use serde::{Deserialize, Serialize};

use crate::document::{self, Block, BlockVerdict};
use crate::store::{MeetingId, Store, StoredNote, StoredSegment};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("{0}")]
    Store(#[from] crate::store::StoreError),
    /// 在呼叫 Provider 之前就擋下來，因為送出去也只會被靜默截斷
    #[error("本輪證據超出所選模型的能力：{0}")]
    BudgetExceeded(String),
    /// 真實 Planner 回報的錯誤。
    #[error("Provider 回報錯誤：{0}")]
    Provider(String),
}

pub type Result<T> = std::result::Result<T, AgentError>;

/* ── Context 預算（§9.5） ────────────────────────────────────────── */

/// 預算的輸入。每一項都有明確來源，不是散落在程式裡的常數。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetInputs {
    /// 模型的 context window。取自 Provider 的能力宣告，缺少時由 GUI 填寫。
    pub context_window: usize,
    /// 保留給輸出的額度。
    pub output_reserve: usize,
    /// 安全邊際。tokenizer 估計有誤差，貼著上限送必然偶爾超限。
    pub safety_margin: usize,
    /// 證據額度的下限。低於這個值時本輪不值得生成。
    pub minimum_evidence: usize,
}

impl Default for BudgetInputs {
    fn default() -> Self {
        // 這些是 ProviderConfig 的具名設定，有預設值且可被覆寫（§9.5）。
        // 真正的值要在 M5 以評測固定，在那之前它們是設定項不是承諾。
        Self {
            context_window: 128_000,
            output_reserve: 8_000,
            safety_margin: 2_000,
            minimum_evidence: 4_000,
        }
    }
}

/// 證據可用額度 = context − 輸出保留 − 安全邊際 − 指令與 schema 的實際佔用。
///
/// 指令佔用以實際送出的內容量測，不用估計值：schema 一改就會變，
/// 寫死的估計值會在最需要準確的時候失準。
pub fn evidence_allowance(b: BudgetInputs, instruction_tokens: usize) -> Result<usize> {
    let fixed = b.output_reserve + b.safety_margin + instruction_tokens;
    let left = b.context_window.saturating_sub(fixed);
    if left < b.minimum_evidence {
        return Err(AgentError::BudgetExceeded(format!(
            "扣除輸出保留、安全邊際與指令佔用之後只剩 {left} token，低於下限 {}",
            b.minimum_evidence
        )));
    }
    Ok(left)
}

/// Token 計數。真實 Provider 用它自己的 tokenizer。
pub trait Tokenizer: Send {
    fn count(&self, text: &str) -> usize;
}

/// 沒有 tokenizer 時的保守上界。
///
/// 繁體中文一個字經常就是一個 token，英文約四個字元一個。取每字元 1.0
/// 是刻意高估：寧可提早縮減證據，也不要在呼叫當下超限（§9.5）。
pub struct CharUpperBound;

impl Tokenizer for CharUpperBound {
    fn count(&self, text: &str) -> usize {
        text.chars().count()
    }
}

/* ── 證據索引（§9.5） ────────────────────────────────────────────── */

/// 大綱的一段：若干連續片段的摘要與它們涵蓋的 `segment_id` 範圍。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutlineChunk {
    pub first_segment_id: u64,
    pub last_segment_id: u64,
    pub meeting_start_ms: u64,
    pub meeting_end_ms: u64,
    pub summary: String,
}

/// 快照建立時產生一次，供該輪所有階段共用。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceIndex {
    pub meeting_id: MeetingId,
    pub through_event_seq: u64,
    /// 恆定送入每一輪
    pub outline: Vec<OutlineChunk>,
    /// 高優先證據，不參與裁切。§17 完成定義第 5 點要求筆記可被引用，
    /// 任何裁切策略都不得讓它落選。
    pub notes: Vec<StoredNote>,
    pub speakers: Vec<SpeakerName>,
    /// 需要原文時才取用的候選
    pub segments: Vec<StoredSegment>,
}

/// 語者的識別碼與該顯示的名字。
///
/// 兩個都要：逐字稿片段帶的是識別碼，而讀者（與模型）要看的是名字。
/// 只送名單不送對應關係的話，模型看到「語者=s1」與一份不知道誰是誰的名單，
/// 使用者確認過的名字就到不了成果 —— 而確認語者名稱是 §8 的一整節。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerName {
    pub id: String,
    /// §8.4 的優先順序：確認名 > 暫定名 > 識別碼
    pub display: String,
}

/// 一段大綱涵蓋幾個片段。長度在 M5 以評測固定，在那之前是設定項。
const OUTLINE_CHUNK_SEGMENTS: usize = 8;
/// 每段摘要的字元上限。
const OUTLINE_SUMMARY_CHARS: usize = 60;

/// 從 Store 建立索引。只涵蓋已 final 的片段（§5.4.2）。
pub fn build_index(
    store: &Store,
    meeting: MeetingId,
    through_event_seq: u64,
) -> Result<EvidenceIndex> {
    let segments = store.segments_through(meeting, through_event_seq)?;
    let notes = store.notes_through(meeting, through_event_seq)?;
    let speakers = store
        .speakers_through(meeting, through_event_seq)?
        .into_iter()
        .map(|s| SpeakerName {
            display: s
                .confirmed_name
                .clone()
                .or(s.proposed_name.clone())
                .unwrap_or_else(|| s.speaker_id.clone()),
            id: s.speaker_id,
        })
        .collect();

    let outline = segments
        .chunks(OUTLINE_CHUNK_SEGMENTS)
        .map(|c| OutlineChunk {
            first_segment_id: c[0].segment_id,
            last_segment_id: c[c.len() - 1].segment_id,
            meeting_start_ms: c[0].meeting_start_ms,
            meeting_end_ms: c[c.len() - 1].meeting_end_ms,
            summary: truncate_chars(
                &c.iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                OUTLINE_SUMMARY_CHARS,
            ),
        })
        .collect();

    Ok(EvidenceIndex {
        meeting_id: meeting,
        through_event_seq,
        outline,
        notes,
        speakers,
        segments,
    })
}

fn truncate_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// 本輪實際送出的證據。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidencePack {
    pub outline: Vec<OutlineChunk>,
    pub notes: Vec<StoredNote>,
    pub speakers: Vec<SpeakerName>,
    /// 以原文送入的片段
    pub segments: Vec<StoredSegment>,
    pub tokens_used: usize,
    /// 因額度不足而未送入原文的片段數。大於零時必須讓使用者看見，
    /// 靜默裁切會讓成果看起來涵蓋了整場會議（§9.5）。
    pub segments_omitted: usize,
}

/// 依額度挑選證據。
///
/// 順序是規格決定的，不是實作偏好：必送的部分（大綱、筆記、語者）先佔額度，
/// 剩下的才給片段原文。必送的部分本身就超過額度時直接失敗，不裁切它們。
pub fn pack_evidence(
    index: &EvidenceIndex,
    allowance: usize,
    tok: &dyn Tokenizer,
) -> Result<EvidencePack> {
    let mandatory: usize = index
        .outline
        .iter()
        .map(|c| tok.count(&c.summary))
        .chain(index.notes.iter().map(|n| tok.count(&n.text)))
        .chain(index.speakers.iter().map(|s| tok.count(&s.display)))
        .sum();

    if mandatory > allowance {
        return Err(AgentError::BudgetExceeded(format!(
            "大綱與人工筆記本身就需要 {mandatory} token，超過可用的 {allowance}。\
             筆記不得被裁切，請改用 context 更大的模型"
        )));
    }

    let mut used = mandatory;
    let mut segments = Vec::new();
    let mut omitted = 0;
    // 由後往前取原文：會議後段通常承載結論，先被裁掉的應該是開場閒聊
    for s in index.segments.iter().rev() {
        let cost = tok.count(&s.text);
        if used + cost > allowance {
            omitted += 1;
            continue;
        }
        used += cost;
        segments.push(s.clone());
    }
    segments.reverse();

    Ok(EvidencePack {
        outline: index.outline.clone(),
        notes: index.notes.clone(),
        speakers: index.speakers.clone(),
        segments,
        tokens_used: used,
        segments_omitted: omitted,
    })
}

/* ── 迭代（§9.2、§9.3） ──────────────────────────────────────────── */

#[derive(Debug, Clone)]
pub struct DraftRequest<'a> {
    /// 本輪使用者 Prompt，可為空
    pub prompt: &'a str,
    pub evidence: &'a EvidencePack,
    /// 上一輪被拒絕的區塊與原因。第一輪為空。
    /// FixturePlanner 用不到這兩個欄位，真實 Planner 靠它們收斂。
    #[allow(dead_code)]
    pub rejections: &'a [String],
    #[allow(dead_code)]
    pub round: u32,
    /// 前一版成果，若本輪是修訂（§5.5）。沒有前一版時為空。
    ///
    /// 這是「輪」與「版」的差別：`rejections` 是同一版之內上一輪的失敗，
    /// `previous` 是上一個已完成的版本。前者要避開，後者要改進。
    #[allow(dead_code)]
    pub previous: &'a [Block],
}

/// 產生草稿的那一步。真實實作送 Prompt 給 LLM 並解析結構化輸出。
///
/// 只有這個 trait 後面的東西是不確定的。規劃、驗證、停止條件都在外面，
/// 因此換 Provider 不會改變任何一條規則。
pub trait Planner: Send {
    fn draft(&mut self, req: &DraftRequest<'_>) -> Result<Vec<Block>>;

    /// 重出單一個 schema 不合的區塊（§10）。
    ///
    /// 預設回 `None`，代表這個 Planner 不支援單塊重試，直接走降級或移除。
    /// 不預設成「回傳原區塊」，那會讓重試上限變成無意義的空轉。
    fn redraft(
        &mut self,
        _req: &DraftRequest<'_>,
        _block: &Block,
        _reason: &str,
    ) -> Result<Option<Block>> {
        Ok(None)
    }
}

/// 單一區塊重試的上限（§10）。
const BLOCK_RETRY_LIMIT: u32 = 2;

/// 一個被降級的區塊，連同它原本的種類與原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Degraded {
    pub position: u32,
    pub original_kind: String,
    pub reason: String,
}

/// 處理 schema 不合的區塊：先重試，仍失敗則依 claim_kind 決定移除或降級。
///
/// 這一步在引用驗證之前。`Fact` 區塊不得降級成 `Paragraph`：降級會讓沒通過
/// §9.6 的內容以純文字照樣渲染，等於繞過引用驗證（§10）。
fn settle_schema(
    planner: &mut dyn Planner,
    req: &DraftRequest<'_>,
    drafted: Vec<Block>,
) -> Result<(Vec<Block>, Vec<Degraded>, usize)> {
    use crate::document::{BlockContent, BlockKind};

    let mut out = Vec::with_capacity(drafted.len());
    let mut degraded = Vec::new();
    let mut removed = 0usize;

    for (i, block) in drafted.into_iter().enumerate() {
        let position = i as u32;
        let mut current = block;
        let mut last_reason = match current.validate() {
            Ok(()) => {
                out.push(current);
                continue;
            }
            Err(e) => e.to_string(),
        };

        let original_kind = current.kind;
        let mut fixed = false;
        for _ in 0..BLOCK_RETRY_LIMIT {
            let Some(next) = planner.redraft(req, &current, &last_reason)? else {
                break;
            };
            match next.validate() {
                Ok(()) => {
                    current = next;
                    fixed = true;
                    break;
                }
                Err(e) => {
                    last_reason = e.to_string();
                    current = next;
                }
            }
        }
        if fixed {
            out.push(current);
            continue;
        }

        if crate::document::BlockKind::may_degrade(current.claim_kind) {
            // 降級不改變 claim_kind，只放棄原本的結構
            out.push(Block {
                kind: BlockKind::Paragraph,
                claim_kind: current.claim_kind,
                content: BlockContent::Text {
                    text: current.content.plain_text(),
                },
                source_refs: current.source_refs.clone(),
            });
            degraded.push(Degraded {
                position,
                original_kind: original_kind.as_str().to_owned(),
                reason: last_reason,
            });
        } else {
            removed += 1;
        }
    }
    Ok((out, degraded, removed))
}

/* ── §9.2 第五步與第七步裡確定性的那一部分 ───────────────────────── */

/// 涵蓋範圍的缺口。
///
/// 「這一輪看不到多少內容」是系統知道而模型不知道的事實：額度裁掉了幾段、
/// 快照游標之後還有沒有錄音。交給模型寫，它最多只能複述我們告訴它的數字，
/// 而且經常整段忘記寫。系統自己補，這一條就不會漏。
///
/// 因此 Prompt 也不再要求模型寫涵蓋範圍的缺口 —— 兩邊都寫就是重複內容，
/// 而重複正是第七步要消掉的東西。
fn coverage_gaps(pack: &EvidencePack) -> Vec<Block> {
    let mut out = Vec::new();
    if pack.segments_omitted > 0 {
        out.push(system_gap(format!(
            "本輪有 {} 段逐字稿因額度限制未以原文納入證據，這些區間的內容未被涵蓋。",
            pack.segments_omitted
        )));
    }
    out
}

/// 快照游標之後又累積了多少內容，寫成一個缺口區塊。
///
/// 這一則**不能**在 `generate` 裡產生：整趟生成固定在同一個讀取快照上，
/// 而那個快照的用途正是「跨輪看到同一個世界」。在它裡面查「游標之後有沒有
/// 新東西」永遠是生成開始那一刻的答案，而錄音在生成期間會繼續寫入 —— 使用者
/// 最在意的那幾分鐘剛好全部看不到。
///
/// 因此由呼叫端在生成結束、寫入成果之前，用一個沒有被凍結的連線量一次。
/// 那也是語意上正確的時間點：讀者問的是「這份文件寫成的時候，外面還有沒有
/// 沒收進來的東西」。
///
/// 只在真的有未涵蓋內容時才回傳。會後才建立的摘要涵蓋整場會議，
/// 對它宣告「還有東西沒看到」是憑空製造一個不存在的缺口。
pub fn uncovered_content_gap(count: u64) -> Option<Block> {
    (count > 0).then(|| {
        system_gap(format!(
            "快照建立之後又記錄了 {count} 筆逐字稿或筆記，那些內容不在本輪成果裡。"
        ))
    })
}

/// 系統產生的缺口都以這幾個開頭。
///
/// 修訂時前一版會整份送回給模型，而 Prompt 要求「沒有要動的區塊照原樣輸出」。
/// 上一版的涵蓋缺口因此會被照抄進新版 —— 但那是上一個游標的事實，新版的
/// 涵蓋範圍不一樣。這幾個前綴讓載入前一版時能把它們濾掉，每一輪重新產生。
const SYSTEM_GAP_PREFIXES: [&str; 2] = ["本輪有", "快照建立之後"];

/// 這是系統自己產的涵蓋缺口嗎。
///
/// 用開頭比對而不是加一個 provenance 欄位：那個欄位要進 schema、進資料庫、
/// 進兩個渲染器，而它唯一的用途是這一個判斷。前綴與產生它們的地方在同一個
/// 檔案裡，有測試守著兩邊一致。
fn is_system_gap(b: &Block) -> bool {
    if b.claim_kind != crate::model::ClaimKind::Gap || !b.source_refs.is_empty() {
        return false;
    }
    let text = b.content.plain_text();
    SYSTEM_GAP_PREFIXES.iter().any(|p| text.starts_with(p))
}

fn system_gap(text: String) -> Block {
    Block {
        kind: crate::document::BlockKind::Gap,
        claim_kind: crate::model::ClaimKind::Gap,
        content: crate::document::BlockContent::Text { text },
        source_refs: vec![],
    }
}

/// 重複內容。
///
/// §9.2 第七步要檢查「是否有重複內容」。判斷「這兩段講的是不是同一件事」
/// 需要語意，但「這兩段是同一段話」不需要 —— 正規化之後字面相同就是重複，
/// 而模型在多輪修訂裡最常產生的就是這種一字不差的重複。
///
/// 只比純文字不比種類：同一句話一次寫成 Paragraph、一次寫成 Decision，
/// 讀者看到的仍然是同一句話出現兩次。留下先出現的那一個。
fn drop_duplicates(blocks: Vec<Block>) -> (Vec<Block>, usize) {
    // 留下承載最多結構的那一個，不是先出現的那一個。
    //
    // 同一句「採用方案 A」寫成 Paragraph 也寫成 Decision 時，只留先出現的
    // 會讓「決議區有沒有這條」取決於模型的輸出順序 —— 那正是 §10.1 說不該
    // 發生的事。Decision 與 ActionItem 會進獨立段落、帶負責人與期限，
    // 資訊嚴格多於一段同文的內文。
    fn specificity(b: &Block) -> u8 {
        match b.kind {
            crate::document::BlockKind::Decision | crate::document::BlockKind::ActionItem => 3,
            crate::document::BlockKind::Gap | crate::document::BlockKind::Suggestion => 2,
            crate::document::BlockKind::Paragraph => 0,
            _ => 1,
        }
    }

    let key_of = |b: &Block| crate::document::normalize_for_match(&b.content.plain_text());

    // 第一趟決定每段文字要留哪一個位置，第二趟依原順序輸出
    let mut winner: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, b) in blocks.iter().enumerate() {
        let key = key_of(b);
        if key.is_empty() {
            continue;
        }
        match winner.get(&key) {
            Some(&j) if specificity(&blocks[j]) >= specificity(b) => {}
            _ => {
                winner.insert(key, i);
            }
        }
    }

    let before = blocks.len();
    let kept: Vec<Block> = blocks
        .iter()
        .enumerate()
        .filter(|(i, b)| {
            let key = key_of(b);
            // 空內容通不過 schema 驗證，走不到這裡；真走到了也不該被當成重複
            key.is_empty() || winner.get(&key) == Some(i)
        })
        .map(|(_, b)| b.clone())
        .collect();
    let removed = before - kept.len();
    (kept, removed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// 所有區塊通過驗證
    Verified,
    /// 達到輪數上限，回傳目前最佳版本
    RoundLimit,
    /// 新一輪沒有實質改善
    NoImprovement,
    /// 沒有任何區塊通過驗證
    NothingAdmissible,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationResult {
    pub blocks: Vec<Block>,
    pub verdicts: Vec<BlockVerdict>,
    pub rounds: u32,
    pub stop_reason: StopReason,
    /// 因 schema 不合而降級為 Paragraph 的區塊。降級不改變 claim_kind。
    pub degraded: Vec<Degraded>,
    /// 沒通過驗證的區塊為什麼沒過。全軍覆沒時這是使用者唯一的線索。
    pub rejections: Vec<String>,
    pub evidence_tokens: usize,
    pub segments_omitted: usize,
    /// 未通過驗證而被移除的區塊數，供 §15.1 的 Interface 測試斷言
    pub rejected_blocks: usize,
    /// 字面重複而被移除的區塊數（§9.2 第七步）
    pub duplicates_removed: usize,
}

impl GenerationResult {
    /// 這一輪該不該被寫成「完成」。
    ///
    /// 一個區塊都沒有不是完成。兩條路會走到這裡：模型回了合法的空陣列，
    /// 或者每一個區塊都沒通過引用驗證。兩種都會讓歷史頁顯示「已完成」，
    /// 使用者點開看到一份空文件而且沒有任何線索。缺什麼要明說（§17.2）。
    ///
    /// 回 `Some(理由)` 代表這一輪應該記成失敗。
    pub fn failure_reason(&self) -> Option<String> {
        if !self.blocks.is_empty() {
            return None;
        }
        Some(if self.rejected_blocks == 0 {
            "模型沒有產出任何區塊。".into()
        } else {
            format!(
                "產出的 {} 個區塊都沒有通過驗證。最後一輪的原因：{}",
                self.rejected_blocks,
                if self.rejections.is_empty() {
                    "未提供".into()
                } else {
                    self.rejections.join("；")
                }
            )
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_rounds: u32,
}

impl Default for Limits {
    fn default() -> Self {
        // Agent Loop 不得無限制自我呼叫（§9.3）
        Self { max_rounds: 3 }
    }
}

/// 一次生成所需的全部輸入。
///
/// 收成一個結構而不是八個參數：這些值必須一起決定，拆開傳遲早會出現
/// 「預算是這個模型的，證據是另一個快照的」這種組合。
pub struct GenerationRequest<'a> {
    pub meeting: MeetingId,
    /// 快照游標。本輪的涵蓋範圍由它凍結（§5.4.2）。
    pub through_event_seq: u64,
    /// 本輪使用者 Prompt，可為空（§9.1）
    pub prompt: &'a str,
    pub budget: BudgetInputs,
    pub limits: Limits,
    /// 要修訂的版本號，若本輪是修訂（§5.5）。
    ///
    /// 帶版本號而不是直接帶區塊：呼叫端只知道「使用者在看哪一版」，
    /// 「那一版的內容是什麼」是 Store 的事，兩者不該在呼叫端拼起來。
    pub revise_of: Option<u32>,
}

/// 讀出某一版的成果區塊，供本輪修訂。
///
/// 解不開的區塊直接略過而不是讓整輪失敗：那一版早就寫進資料庫了，
/// 因為其中一塊壞掉就不讓使用者修訂，代價遠大於少一塊參考內容。
fn load_version(store: &Store, meeting: MeetingId, version: u32) -> Result<Vec<Block>> {
    let Some(run) = store
        .runs(meeting)?
        .into_iter()
        .find(|r| r.version_no == version && r.status == "completed")
    else {
        // 指定了一版卻找不到，那是呼叫端算錯了版本號。靜默當成「不是修訂」
        // 會讓使用者以為改寫成功，實際上拿到一份從零開始的文件。
        return Err(AgentError::Provider(format!(
            "找不到可修訂的第 {version} 版"
        )));
    };
    Ok(store
        .run_blocks(run.run_id)?
        .iter()
        .filter_map(Block::from_stored)
        // 上一版的涵蓋缺口講的是上一個游標的事實，這一版重算
        .filter(|b| !is_system_gap(b))
        .collect())
}

/// 跑完一輪生成。
///
/// 迴圈本身是確定性的：草稿由 Planner 產生，其餘每一步都在這裡，
/// 而且每一步的結果都可以被斷言。
pub fn generate(
    store: &Store,
    req: &GenerationRequest<'_>,
    planner: &mut dyn Planner,
    tok: &dyn Tokenizer,
) -> Result<GenerationResult> {
    let GenerationRequest {
        meeting,
        through_event_seq,
        prompt,
        budget,
        limits,
        revise_of,
    } = *req;
    let index = build_index(store, meeting, through_event_seq)?;
    let previous = match revise_of {
        Some(v) => load_version(store, meeting, v)?,
        None => Vec::new(),
    };
    // 指令佔用以實際內容量測。這裡的指令就是 Prompt、schema 說明與前一版
    // 成果——前一版是本輪要改的對象，它跟著指令走而不是跟著證據走，
    // 而且它一大，證據額度就該跟著縮，不是靜默超限。
    //
    // 前一版算的是序列化之後的 JSON，不是純文字：實際送出去的就是那份
    // JSON，裡面的 sourceRefs、引文與表格結構經常比純文字多好幾倍。
    // 只算純文字會配過多證據，然後在呼叫當下才超限。
    let previous_tokens = serde_json::to_string(&previous)
        .map(|s| tok.count(&s))
        .unwrap_or_else(|_| {
            previous
                .iter()
                .map(|b| tok.count(&b.content.plain_text()))
                .sum()
        });
    let instruction_tokens = tok.count(prompt) + tok.count(SCHEMA_BRIEF) + previous_tokens;
    let allowance = evidence_allowance(budget, instruction_tokens)?;
    let pack = pack_evidence(&index, allowance, tok)?;

    let mut best: Vec<Block> = Vec::new();
    let mut best_verdicts: Vec<BlockVerdict> = Vec::new();
    let mut rejections: Vec<String> = Vec::new();
    let mut rejected_total = 0usize;
    let mut degraded: Vec<Degraded> = Vec::new();
    let mut duplicates = 0usize;
    // 目前最佳版本丟掉了幾個區塊。數量打平時用它決定要不要換。
    let mut best_rejected = usize::MAX;
    // 迴圈一定至少跑一輪，所以這個值一定會在讀之前被寫入
    let mut last_rejections: Vec<String>;
    let mut round = 0u32;

    let stop_reason = loop {
        round += 1;
        let req = DraftRequest {
            prompt,
            evidence: &pack,
            rejections: &rejections,
            round,
            previous: &previous,
        };
        let drafted = planner.draft(&req)?;
        // schema 先結清，再送引用驗證：形狀都不對的區塊沒必要去查證據
        let (drafted, degraded_now, removed) = settle_schema(planner, &req, drafted)?;
        rejected_total += removed;
        degraded.extend(degraded_now);
        let (admitted, verdicts) =
            document::verify_blocks(store, meeting, through_event_seq, &drafted)?;
        // 去重放在驗證之後：被拒絕的區塊本來就不會出現，先去重只是白做工
        let (admitted, dupes) = drop_duplicates(admitted);
        duplicates += dupes;
        let rejected: Vec<String> = verdicts
            .iter()
            .filter(|v| !v.admitted)
            .filter_map(|v| v.reason.clone())
            .collect();
        rejected_total += rejected.len();

        // 「改善」不只看通過幾個區塊。同樣通過五個，但這一輪少丟掉三個，
        // 那是實質更好的一版 —— 只比數量的話會把它丟掉，繼續拿第一輪的。
        let improved = admitted.len() > best.len()
            || (admitted.len() == best.len() && rejected.len() < best_rejected);
        if improved {
            best = admitted;
            best_verdicts = verdicts;
            best_rejected = rejected.len();
        }
        last_rejections = rejected.clone();

        if rejected.is_empty() {
            break StopReason::Verified;
        }
        if round >= limits.max_rounds {
            break if best.is_empty() {
                StopReason::NothingAdmissible
            } else {
                StopReason::RoundLimit
            };
        }
        // 沒有實質改善就停：再跑一輪只是重複付費（§9.3）。
        //
        // 但「一個都沒過」不算沒有改善 —— 那正是要把拒絕原因餵回去、
        // 靠下一輪修好的情況。0 跟 0 比永遠不會變大，於是迴圈在第一輪就停，
        // 多輪修正引用這件事從來沒有發生過。
        if !improved && !best.is_empty() {
            break StopReason::NoImprovement;
        }
        rejections = rejected;
    };

    // 空成果不得回報成 Verified。
    //
    // 模型回一個合法的空陣列時，rejected 也是空的，迴圈會當成「全部通過」
    // 而停在 Verified —— 那是一句假話，而呼叫端就是靠這個判斷要不要把 run
    // 寫成完成的。
    let stop_reason = if best.is_empty() {
        StopReason::NothingAdmissible
    } else {
        stop_reason
    };

    // 涵蓋範圍的缺口最後補：它與模型產出無關，也不該影響「有沒有改善」的判斷。
    // 全軍覆沒時不補，那時該讓使用者看到的是生成失敗，不是一份只有缺口的文件。
    if !best.is_empty() {
        best.extend(coverage_gaps(&pack));
        // 再去重一次：修訂時模型會照抄前一版，而前一版裡就有上次補的涵蓋缺口
        let (deduped, dupes) = drop_duplicates(best);
        best = deduped;
        duplicates += dupes;
    }

    Ok(GenerationResult {
        blocks: best,
        verdicts: best_verdicts,
        rounds: round,
        stop_reason,
        degraded,
        evidence_tokens: pack.tokens_used,
        segments_omitted: pack.segments_omitted,
        rejected_blocks: rejected_total,
        duplicates_removed: duplicates,
        rejections: last_rejections,
    })
}

/// 送給模型的 schema 說明。放在這裡是為了讓指令佔用能被實際量測。
pub const SCHEMA_BRIEF: &str = "\
每個區塊必須包含 kind、claimKind 與 content。claimKind 取值為 fact、inference、\
suggestion、gap，沒有預設值。claimKind 為 fact 或 inference 的區塊必須附上 \
sourceRefs，每筆包含 sourceKind、sourceId、sourceRevision、locator、quotedText \
與 quotedTextSha256。quotedText 必須逐字取自證據，不得改寫。";

/* ── Fixture Planner ─────────────────────────────────────────────── */

/// 不呼叫任何 Provider 的規劃器。
///
/// 存在的理由不是 demo，而是 §15.3 要求的可重複整合測試：真實 LLM 的輸出
/// 每次不同，無法拿來斷言引用驗證與停止條件。它從證據裡逐字取引文，
/// 因此走的是與真實輸出完全相同的驗證路徑。
pub struct FixturePlanner;

impl Planner for FixturePlanner {
    fn draft(&mut self, req: &DraftRequest<'_>) -> Result<Vec<Block>> {
        use crate::document::{BlockContent, BlockKind};
        use crate::model::ClaimKind;

        let mut blocks = vec![
            // 成果摘要那一區的內容。這個規劃器不會摘要，所以它說的是自己是什麼，
            // 不是會議是什麼 —— 生一段像摘要的文字出來就變成憑空捏造。
            Block {
                kind: BlockKind::Callout,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Callout {
                    tone: "summary".into(),
                    title: "成果摘要".into(),
                    body: "這一版由內建測試規劃器產生，沒有呼叫任何模型。\
                           以下內容是逐字稿的直接摘錄，不是摘要。"
                        .into(),
                },
                source_refs: vec![],
            },
            Block {
                kind: BlockKind::Heading,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Heading {
                    level: 1,
                    text: if req.prompt.trim().is_empty() {
                        "會議摘要".into()
                    } else {
                        req.prompt.trim().chars().take(40).collect()
                    },
                },
                source_refs: vec![],
            },
        ];

        // 每個片段各出一個 Fact，引文逐字取自該版本的開頭
        for s in req.evidence.segments.iter().take(6) {
            let quote: String = s.text.chars().take(12).collect();
            // 用與驗證同一條判準，不是「非空白」：fixture 送出一筆註定被
            // 拒絕的引用，整合測試就分不出是驗證有效還是 fixture 有問題
            if !document::is_quotable(&quote) {
                continue;
            }
            blocks.push(Block {
                kind: BlockKind::Paragraph,
                claim_kind: ClaimKind::Fact,
                content: BlockContent::Text {
                    text: s.text.clone(),
                },
                source_refs: vec![crate::store::SourceRef {
                    source_kind: "transcript_segment".into(),
                    source_id: s.segment_id.to_string(),
                    source_revision: s.revision,
                    locator: format!("0-{}", quote.chars().count()),
                    quoted_text_sha256: document::sha256_hex(&quote),
                    quoted_text: quote,
                    validation_status: "unverified".into(),
                }],
            });
        }

        // 涵蓋範圍的缺口由 generate 補，不在這裡：那是每個 Planner 都一樣的
        // 系統事實，寫在其中一個裡面，換一個 Planner 就會靜默消失
        Ok(blocks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::document::{BlockContent, BlockKind};
    use crate::model::{ClaimKind, Origin, Timeline, Track};
    use crate::store::{DomainEvent, SegmentRevision, SourceRef};

    fn seeded() -> (Store, MeetingId, u64) {
        let mut s = Store::new(db::open_in_memory().unwrap());
        let m = s.create_meeting("測試").unwrap();
        let mut events = Vec::new();
        for i in 1..=10u64 {
            events.push((
                DomainEvent::TranscriptSegmentFinalized {
                    segment: SegmentRevision {
                        segment_id: i,
                        revision: 1,
                        text: format!("這是第 {i} 段的內容，講的是報價與範圍"),
                        speaker_id: Some("s1".into()),
                        track: Track::System,
                        meeting_start_ms: i * 1000,
                        meeting_end_ms: i * 1000 + 900,
                        captured_start_ms: i * 1000,
                        captured_end_ms: i * 1000 + 900,
                        echo_likelihood: None,
                        overlap_group_id: None,
                        provider_stream_id: None,
                        provider_result_id: None,
                        rollover_generation: 0,
                        origin: Origin::Provider,
                        speaker_spans: Vec::new(),
                    },
                },
                Timeline::new(i * 1000, i * 1000),
            ));
        }
        events.push((
            DomainEvent::NoteAdded {
                note_id: 1,
                text: "客戶要求維運月費區間".into(),
            },
            Timeline::new(11_000, 11_000),
        ));
        let seqs = s.append(m, &events).unwrap();
        let cursor = *seqs.last().unwrap();
        (s, m, cursor)
    }

    /* ── 預算 ─────────────────────────────────────────────────── */

    #[test]
    fn allowance_is_what_remains_after_every_named_deduction() {
        let b = BudgetInputs {
            context_window: 100_000,
            output_reserve: 8_000,
            safety_margin: 2_000,
            minimum_evidence: 1_000,
        };
        assert_eq!(evidence_allowance(b, 5_000).unwrap(), 85_000);
    }

    #[test]
    fn a_budget_below_the_floor_is_refused_before_calling_the_provider() {
        let b = BudgetInputs {
            context_window: 8_000,
            output_reserve: 4_000,
            safety_margin: 1_000,
            minimum_evidence: 4_000,
        };
        // 送出去只會被 Provider 靜默截斷，而且通常從最前面開始丟
        assert!(matches!(
            evidence_allowance(b, 1_000),
            Err(AgentError::BudgetExceeded(_))
        ));
    }

    #[test]
    fn notes_are_never_dropped_to_make_room() {
        let (s, m, cursor) = seeded();
        let index = build_index(&s, m, cursor).unwrap();
        // 額度剛好夠必送的部分，片段原文全部落選
        let mandatory: usize = index
            .outline
            .iter()
            .map(|c| c.summary.chars().count())
            .chain(index.notes.iter().map(|n| n.text.chars().count()))
            .chain(index.speakers.iter().map(|x| x.display.chars().count()))
            .sum();
        let pack = pack_evidence(&index, mandatory, &CharUpperBound).unwrap();
        assert_eq!(pack.notes.len(), 1, "筆記被裁掉了");
        assert!(pack.segments.is_empty());
        assert_eq!(pack.segments_omitted, 10);
    }

    #[test]
    fn mandatory_evidence_larger_than_the_allowance_fails_loudly() {
        let (s, m, cursor) = seeded();
        let index = build_index(&s, m, cursor).unwrap();
        let err = pack_evidence(&index, 5, &CharUpperBound).unwrap_err();
        assert!(matches!(err, AgentError::BudgetExceeded(_)));
        assert!(err.to_string().contains("筆記不得被裁切"));
    }

    #[test]
    fn omitted_segments_are_counted_not_silently_dropped() {
        let (s, m, cursor) = seeded();
        let index = build_index(&s, m, cursor).unwrap();
        let pack = pack_evidence(&index, 200, &CharUpperBound).unwrap();
        assert!(pack.segments_omitted > 0);
        assert_eq!(pack.segments.len() + pack.segments_omitted, 10);
    }

    #[test]
    fn the_outline_covers_every_segment_id_in_order() {
        let (s, m, cursor) = seeded();
        let index = build_index(&s, m, cursor).unwrap();
        assert_eq!(index.outline[0].first_segment_id, 1);
        assert_eq!(index.outline.last().unwrap().last_segment_id, 10);
    }

    /* ── 迭代 ─────────────────────────────────────────────────── */

    #[test]
    fn a_clean_draft_stops_after_one_round() {
        let (s, m, cursor) = seeded();
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut FixturePlanner,
            &CharUpperBound,
        )
        .unwrap();
        assert_eq!(r.rounds, 1);
        assert_eq!(r.stop_reason, StopReason::Verified);
        assert_eq!(r.rejected_blocks, 0);
        assert!(r.blocks.iter().any(|b| b.claim_kind == ClaimKind::Fact));
    }

    /* ── 空成果不是完成 ─────────────────────────────────────────── */

    /// 回一個合法的空陣列。模型偶爾會這樣，而它完全通過 schema 與驗證。
    struct ReturnsNothing;

    impl Planner for ReturnsNothing {
        fn draft(&mut self, _req: &DraftRequest<'_>) -> Result<Vec<Block>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn an_empty_document_is_never_reported_as_verified() {
        // 空陣列沒有被拒絕的區塊，於是迴圈會當成「全部通過」而停在 Verified。
        // 呼叫端就是靠這個判斷要不要把 run 寫成完成的，那是一句假話。
        let (s, m, cursor) = seeded();
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut ReturnsNothing,
            &CharUpperBound,
        )
        .unwrap();
        assert!(r.blocks.is_empty());
        assert_eq!(r.stop_reason, StopReason::NothingAdmissible);
        let reason = r.failure_reason().expect("空成果應該要求記成失敗");
        assert!(reason.contains("沒有產出"), "理由沒說清楚：{reason}");
    }

    #[test]
    fn a_document_whose_blocks_all_failed_verification_says_why() {
        // 全部被拒也是空成果，但理由不一樣：使用者要知道是「模型沒生」
        // 還是「生了但引用都是假的」
        let (s, m, cursor) = seeded();
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut AlwaysFabricates,
            &CharUpperBound,
        )
        .unwrap();
        let reason = r.failure_reason().expect("空成果應該要求記成失敗");
        assert!(reason.contains("沒有通過驗證"), "理由沒說清楚：{reason}");
        // 最後一輪的拒絕原因要帶出來，那是唯一的線索
        assert!(!r.rejections.is_empty(), "沒有留下拒絕原因");
    }

    #[test]
    fn a_document_with_content_is_not_asked_to_fail() {
        let (s, m, cursor) = seeded();
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut FixturePlanner,
            &CharUpperBound,
        )
        .unwrap();
        assert!(r.failure_reason().is_none());
    }

    #[test]
    fn a_decision_survives_a_paragraph_that_says_the_same_thing() {
        // 同一句話寫成 Paragraph 也寫成 Decision 時，留先出現的那一個會讓
        // 「決議區有沒有這條」取決於模型的輸出順序 —— 正是 §10.1 說不該
        // 發生的事。兩種順序都要得到同一份文件。
        let same = "採用方案 A";
        let para = || Block {
            kind: BlockKind::Paragraph,
            claim_kind: ClaimKind::Inference,
            content: BlockContent::Text { text: same.into() },
            source_refs: vec![],
        };
        let decision = || Block {
            kind: BlockKind::Decision,
            claim_kind: ClaimKind::Fact,
            content: BlockContent::Text { text: same.into() },
            source_refs: vec![],
        };

        for (label, blocks) in [
            ("內文在前", vec![para(), decision()]),
            ("決議在前", vec![decision(), para()]),
        ] {
            let (kept, removed) = drop_duplicates(blocks);
            assert_eq!(removed, 1, "{label}：沒有消掉重複");
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].kind, BlockKind::Decision, "{label}：決議被丟掉了");
        }
    }

    /// 每一輪都全軍覆沒，用來確認迴圈會把拒絕原因餵回去而不是第一輪就停。
    struct CountsRounds {
        rounds: u32,
    }

    impl Planner for CountsRounds {
        fn draft(&mut self, req: &DraftRequest<'_>) -> Result<Vec<Block>> {
            self.rounds = req.round;
            AlwaysFabricates.draft(req)
        }
    }

    #[test]
    fn a_round_where_nothing_passes_still_gets_another_try() {
        // 0 跟 0 比永遠不會變大，於是「沒有改善就停」會在第一輪就中止 ——
        // 而那正是要把拒絕原因餵回去、靠下一輪修好的情況
        let (s, m, cursor) = seeded();
        let mut p = CountsRounds { rounds: 0 };
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits { max_rounds: 3 },
                revise_of: None,
            },
            &mut p,
            &CharUpperBound,
        )
        .unwrap();
        assert_eq!(p.rounds, 3, "第一輪就放棄了，拒絕原因從來沒有被餵回去");
        assert_eq!(r.stop_reason, StopReason::NothingAdmissible);
    }

    /// 兩輪都通過同樣數量的區塊，但第二輪少丟掉幾個。
    struct SameCountFewerRejects;

    impl Planner for SameCountFewerRejects {
        fn draft(&mut self, req: &DraftRequest<'_>) -> Result<Vec<Block>> {
            let mut out = vec![Block {
                kind: BlockKind::Paragraph,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Text {
                    text: format!("第 {} 輪的內容", req.round),
                },
                source_refs: vec![],
            }];
            // 第一輪多兩個引用捏造的區塊，第二輪只剩一個
            let bogus = if req.round == 1 { 2 } else { 1 };
            for i in 0..bogus {
                let quote = format!("這句話從來沒有人說過{i}");
                out.push(Block {
                    kind: BlockKind::Paragraph,
                    claim_kind: ClaimKind::Fact,
                    content: BlockContent::Text {
                        text: format!("捏造 {i}"),
                    },
                    source_refs: vec![SourceRef {
                        source_kind: "transcript_segment".into(),
                        source_id: "1".into(),
                        source_revision: 1,
                        locator: "0-5".into(),
                        quoted_text_sha256: document::sha256_hex(&quote),
                        quoted_text: quote,
                        validation_status: "unverified".into(),
                    }],
                });
            }
            Ok(out)
        }
    }

    #[test]
    fn a_round_that_loses_fewer_blocks_wins_even_when_the_count_ties() {
        // 只比通過數量的話，第二輪雖然少丟掉一個區塊也會被當成「沒有改善」
        // 而丟棄，成果停在第一輪那一版
        let (s, m, cursor) = seeded();
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut SameCountFewerRejects,
            &CharUpperBound,
        )
        .unwrap();
        assert!(
            r.blocks
                .iter()
                .any(|b| b.content.plain_text().contains("第 2 輪")),
            "留在成果裡的是第一輪那一版：{:?}",
            r.blocks
                .iter()
                .map(|b| b.content.plain_text())
                .collect::<Vec<_>>()
        );
    }

    /* ── 修訂：前一版文件（§5.5） ────────────────────────────────── */

    /// 記下自己收到的前一版，讓測試看得到那份文件有沒有真的送進去。
    #[derive(Default)]
    struct RecordsPrevious {
        saw: Vec<String>,
    }

    impl Planner for RecordsPrevious {
        fn draft(&mut self, req: &DraftRequest<'_>) -> Result<Vec<Block>> {
            self.saw = req
                .previous
                .iter()
                .map(|b| b.content.plain_text())
                .collect();
            Ok(vec![Block {
                kind: BlockKind::Paragraph,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Text {
                    text: "第二版的內容".into(),
                },
                source_refs: vec![],
            }])
        }
    }

    /// 寫進一個已完成的第一版，回傳版本號。
    ///
    /// 走事件而不是直接寫表：區塊與 run 都是投影，繞過事件寫出來的狀態
    /// 在重建投影之後就會消失，測試也就守不住真正的路徑。
    fn completed_version(s: &mut Store, m: MeetingId, cursor: u64, text: &str) -> u32 {
        let block = Block {
            kind: BlockKind::Paragraph,
            claim_kind: ClaimKind::Inference,
            content: BlockContent::Text { text: text.into() },
            source_refs: vec![],
        };
        s.append(
            m,
            &[
                (
                    DomainEvent::SnapshotCreated {
                        document_id: 1,
                        run_id: 1,
                        parent_run_id: None,
                        version_no: 1,
                        purpose: "meeting-summary".into(),
                        title: "會議摘要".into(),
                        through_event_seq: cursor,
                        prompt: String::new(),
                    },
                    Timeline::new(0, 0),
                ),
                (
                    DomainEvent::GenerationCompleted {
                        run_id: 1,
                        blocks: vec![block.to_stored(0)],
                        usage: serde_json::json!({}),
                    },
                    Timeline::new(0, 0),
                ),
            ],
        )
        .unwrap();
        1
    }

    #[test]
    fn a_revision_receives_the_version_it_is_revising() {
        let (mut s, m, cursor) = seeded();
        let v1 = completed_version(&mut s, m, cursor, "第一版的內容");
        let mut p = RecordsPrevious::default();
        generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "補上行動項目",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: Some(v1),
            },
            &mut p,
            &CharUpperBound,
        )
        .unwrap();
        assert_eq!(
            p.saw,
            vec!["第一版的內容".to_string()],
            "前一版沒有送進 Planner"
        );
    }

    #[test]
    fn a_revision_does_not_inherit_the_previous_versions_coverage_gap() {
        // 修訂時整份前一版送回模型，而 Prompt 要求「沒有要動的照原樣輸出」。
        // 上一版的涵蓋缺口講的是上一個游標的事實 —— 抄進新版就成了一句
        // 過時的假話，而新版還會再算一個自己的。獨立審查找到的。
        let (mut s, m, cursor) = seeded();
        let stale = "快照建立之後又記錄了 5 筆逐字稿或筆記，那些內容不在本輪成果裡。";
        let blocks = [
            Block {
                kind: BlockKind::Paragraph,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Text {
                    text: "第一版的內容".into(),
                },
                source_refs: vec![],
            },
            system_gap(stale.into()),
        ];
        s.append(
            m,
            &[
                (
                    DomainEvent::SnapshotCreated {
                        document_id: 1,
                        run_id: 1,
                        parent_run_id: None,
                        version_no: 1,
                        purpose: "meeting-summary".into(),
                        title: "會議摘要".into(),
                        through_event_seq: cursor,
                        prompt: String::new(),
                    },
                    Timeline::new(0, 0),
                ),
                (
                    DomainEvent::GenerationCompleted {
                        run_id: 1,
                        blocks: blocks
                            .iter()
                            .enumerate()
                            .map(|(i, b)| b.to_stored(i as u32))
                            .collect(),
                        usage: serde_json::json!({}),
                    },
                    Timeline::new(0, 0),
                ),
            ],
        )
        .unwrap();

        let mut p = RecordsPrevious::default();
        generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: Some(1),
            },
            &mut p,
            &CharUpperBound,
        )
        .unwrap();
        assert_eq!(
            p.saw,
            vec!["第一版的內容".to_string()],
            "舊的涵蓋缺口被送回模型了"
        );
    }

    #[test]
    fn the_system_gap_prefixes_match_what_the_system_actually_writes() {
        // 前綴比對認錯的話，濾除就會靜默失效。兩邊在同一個檔案裡，
        // 這個測試盯的是它們沒有各自漂走。
        let pack = EvidencePack {
            outline: vec![],
            notes: vec![],
            speakers: vec![],
            segments: vec![],
            tokens_used: 0,
            segments_omitted: 3,
        };
        for g in coverage_gaps(&pack) {
            assert!(is_system_gap(&g), "自己產的缺口認不出來：{:?}", g.content);
        }
        let after = uncovered_content_gap(2).expect("有未涵蓋內容");
        assert!(is_system_gap(&after), "認不出游標缺口");

        // 模型自己寫的缺口不該被當成系統產的濾掉
        let human = Block {
            kind: BlockKind::Gap,
            claim_kind: ClaimKind::Gap,
            content: BlockContent::Text {
                text: "維運的月費區間還沒有數字。".into(),
            },
            source_refs: vec![],
        };
        assert!(!is_system_gap(&human), "把模型寫的缺口濾掉了");
    }

    #[test]
    fn a_first_version_has_no_previous_document() {
        let (s, m, cursor) = seeded();
        let mut p = RecordsPrevious::default();
        generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut p,
            &CharUpperBound,
        )
        .unwrap();
        assert!(p.saw.is_empty());
    }

    #[test]
    fn pointing_at_a_version_that_does_not_exist_fails_instead_of_starting_over() {
        // 靜默當成「不是修訂」會讓使用者以為改寫成功，
        // 實際上拿到一份從零開始的文件
        let (s, m, cursor) = seeded();
        let err = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: Some(7),
            },
            &mut FixturePlanner,
            &CharUpperBound,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("第 7 版"),
            "錯誤訊息沒指出哪一版：{err}"
        );
    }

    #[test]
    fn the_previous_document_eats_into_the_evidence_budget() {
        // 前一版是本輪要改的對象，它一大證據額度就該跟著縮，
        // 不是靜默超限讓 Provider 從最前面開始截斷
        let (mut s, m, cursor) = seeded();
        let long: String = "很長的一版".repeat(200);
        let v1 = completed_version(&mut s, m, cursor, &long);

        let budget = BudgetInputs {
            context_window: 1_500,
            output_reserve: 0,
            safety_margin: 0,
            minimum_evidence: 10,
        };
        let plain = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget,
                limits: Limits::default(),
                revise_of: None,
            },
            &mut FixturePlanner,
            &CharUpperBound,
        )
        .unwrap();
        let revising = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget,
                limits: Limits::default(),
                revise_of: Some(v1),
            },
            &mut FixturePlanner,
            &CharUpperBound,
        )
        .unwrap();
        assert!(
            revising.evidence_tokens < plain.evidence_tokens,
            "帶了前一版卻沒有佔到額度：{} vs {}",
            revising.evidence_tokens,
            plain.evidence_tokens
        );
    }

    /* ── §9.2 第五步與第七步的確定性部分 ─────────────────────────── */

    /// 同一句話說三次。模型在多輪修訂裡最常見的失手就是這種一字不差的重複。
    struct RepeatsItself;

    impl Planner for RepeatsItself {
        fn draft(&mut self, _req: &DraftRequest<'_>) -> Result<Vec<Block>> {
            let same = |kind| Block {
                kind,
                claim_kind: ClaimKind::Inference,
                content: BlockContent::Text {
                    text: "報價分成設計、開發、維運三項".into(),
                },
                source_refs: vec![],
            };
            Ok(vec![
                same(BlockKind::Paragraph),
                // 全形空白與換行不該讓它逃過去
                Block {
                    kind: BlockKind::Paragraph,
                    claim_kind: ClaimKind::Inference,
                    content: BlockContent::Text {
                        text: "報價分成　設計、開發、維運三項".into(),
                    },
                    source_refs: vec![],
                },
                Block {
                    kind: BlockKind::Paragraph,
                    claim_kind: ClaimKind::Inference,
                    content: BlockContent::Text {
                        text: "另一件事：時程還沒定".into(),
                    },
                    source_refs: vec![],
                },
            ])
        }
    }

    #[test]
    fn the_same_sentence_twice_is_dropped_the_second_time() {
        let (s, m, cursor) = seeded();
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut RepeatsItself,
            &CharUpperBound,
        )
        .unwrap();
        assert_eq!(r.duplicates_removed, 1, "字面重複沒有被消掉");
        assert_eq!(r.blocks.len(), 2);
        // 留下的是先出現的那一個
        assert!(r.blocks[0].content.plain_text().starts_with("報價分成設計"));
    }

    #[test]
    fn the_part_of_the_evidence_that_was_cut_is_reported_by_the_system() {
        // 額度只夠放大綱與筆記，片段原文全部被裁掉
        let (s, m, cursor) = seeded();
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs {
                    context_window: 400,
                    output_reserve: 0,
                    safety_margin: 0,
                    minimum_evidence: 10,
                },
                limits: Limits::default(),
                revise_of: None,
            },
            &mut FixturePlanner,
            &CharUpperBound,
        )
        .unwrap();
        assert!(r.segments_omitted > 0, "這個預算下應該裁掉片段");
        let gaps: Vec<String> = r
            .blocks
            .iter()
            .filter(|b| b.claim_kind == ClaimKind::Gap)
            .map(|b| b.content.plain_text())
            .collect();
        assert!(
            gaps.iter()
                .any(|g| g.contains(&r.segments_omitted.to_string())),
            "裁掉的段數沒有被講出來：{gaps:?}"
        );
    }

    #[test]
    fn a_snapshot_that_covers_everything_does_not_invent_a_coverage_gap() {
        // 會後才建立的摘要涵蓋整場會議。對它宣告「還有內容沒看到」
        // 是憑空製造一個不存在的缺口，而缺口正是使用者要去補的東西
        let (s, m, cursor) = seeded();
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut FixturePlanner,
            &CharUpperBound,
        )
        .unwrap();
        assert_eq!(r.segments_omitted, 0);
        assert!(
            !r.blocks.iter().any(|b| b.claim_kind == ClaimKind::Gap),
            "無中生有了一個缺口：{:?}",
            r.blocks
                .iter()
                .filter(|b| b.claim_kind == ClaimKind::Gap)
                .map(|b| b.content.plain_text())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_snapshots_own_bookkeeping_is_not_mistaken_for_uncovered_content() {
        // 快照本身記一筆 SnapshotCreated、生成完成再記一筆 GenerationCompleted，
        // 兩者的 seq 都在游標之後。拿事件總數來比的話，每一份成果都會被標上
        // 一個不存在的缺口 —— 真實跑一次才看得出來，因為測試的種子資料
        // 從來沒有游標之後的事件。
        let (mut s, m, cursor) = seeded();
        completed_version(&mut s, m, cursor, "第一版的內容");
        assert_eq!(
            s.content_events_after(m, cursor).unwrap(),
            0,
            "這一版只寫了快照自己的紀錄，不該有內容事件"
        );

        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut FixturePlanner,
            &CharUpperBound,
        )
        .unwrap();
        assert!(
            !r.blocks.iter().any(|b| b.claim_kind == ClaimKind::Gap),
            "快照自己的紀錄被當成沒涵蓋到的內容"
        );
    }

    #[test]
    fn uncovered_content_is_reported_only_when_there_really_is_some() {
        // 這一則刻意不在 generate 裡產生：整趟生成固定在同一個讀取快照上，
        // 在那裡面查「游標之後有沒有新東西」永遠是生成開始那一刻的答案，
        // 而錄音在生成期間會繼續寫入。呼叫端在生成結束後量一次才對。
        assert!(uncovered_content_gap(0).is_none(), "無中生有了一個缺口");
        let g = uncovered_content_gap(3).expect("有未涵蓋內容卻沒有缺口");
        assert_eq!(g.claim_kind, ClaimKind::Gap);
        assert!(g.content.plain_text().contains('3'), "沒說有多少筆");
    }

    /// 每輪都產生同一個引用捏造的區塊，用來驗證迴圈不會把它放行。
    struct AlwaysFabricates;

    impl Planner for AlwaysFabricates {
        fn draft(&mut self, _req: &DraftRequest<'_>) -> Result<Vec<Block>> {
            let quote = "這句話從來沒有人說過";
            Ok(vec![Block {
                kind: BlockKind::Paragraph,
                claim_kind: ClaimKind::Fact,
                content: BlockContent::Text {
                    text: "客戶已同意八折".into(),
                },
                source_refs: vec![SourceRef {
                    source_kind: "transcript_segment".into(),
                    source_id: "1".into(),
                    source_revision: 1,
                    locator: "0-10".into(),
                    quoted_text: quote.into(),
                    quoted_text_sha256: document::sha256_hex(quote),
                    validation_status: "unverified".into(),
                }],
            }])
        }
    }

    #[test]
    fn a_fabricated_citation_never_reaches_the_result_however_many_rounds_run() {
        let (s, m, cursor) = seeded();
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut AlwaysFabricates,
            &CharUpperBound,
        )
        .unwrap();
        assert!(r.blocks.is_empty(), "捏造引用的區塊進到成果了");
        assert_eq!(r.stop_reason, StopReason::NothingAdmissible);
        assert!(r.rejected_blocks > 0);
    }

    /// 第一輪部分失敗，第二輪修好，用來確認迴圈真的會迭代。
    struct FixesOnSecondRound;

    impl Planner for FixesOnSecondRound {
        fn draft(&mut self, req: &DraftRequest<'_>) -> Result<Vec<Block>> {
            let mut out = FixturePlanner.draft(req)?;
            if req.round == 1 {
                // 第一輪多塞一個沒有引用的 Fact
                out.push(Block {
                    kind: BlockKind::Paragraph,
                    claim_kind: ClaimKind::Fact,
                    content: BlockContent::Text {
                        text: "沒有來源的斷言".into(),
                    },
                    source_refs: vec![],
                });
            } else {
                assert!(!req.rejections.is_empty(), "第二輪沒有收到上一輪的失敗原因");
            }
            Ok(out)
        }
    }

    #[test]
    fn the_loop_feeds_rejections_back_and_the_second_round_can_recover() {
        let (s, m, cursor) = seeded();
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "整理報價範圍",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut FixesOnSecondRound,
            &CharUpperBound,
        )
        .unwrap();
        assert_eq!(r.rounds, 2);
        assert_eq!(r.stop_reason, StopReason::Verified);
        assert_eq!(r.rejected_blocks, 1);
        assert!(!r.blocks.iter().any(|b| matches!(
            &b.content,
            BlockContent::Text { text } if text == "沒有來源的斷言"
        )));
    }

    #[test]
    fn the_loop_never_exceeds_its_round_limit() {
        let (s, m, cursor) = seeded();
        // AlwaysFabricates 永遠不會改善，因此靠 NoImprovement 先停
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits { max_rounds: 2 },
                revise_of: None,
            },
            &mut AlwaysFabricates,
            &CharUpperBound,
        )
        .unwrap();
        assert!(r.rounds <= 2, "Agent Loop 不得無限制自我呼叫");
    }

    #[test]
    fn omitted_evidence_shows_up_as_a_gap_block_not_as_silence() {
        let (s, m, cursor) = seeded();
        // 額度只夠一小部分片段
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: // 額度夠放大綱與筆記，但不夠放全部片段原文
            BudgetInputs {
                context_window: 500,
                output_reserve: 50,
                safety_margin: 10,
                minimum_evidence: 50,
            },
                limits: Limits::default(),
                revise_of: None,
            },
            &mut FixturePlanner,
            &CharUpperBound,
        )
        .unwrap();
        assert!(r.segments_omitted > 0);
        assert!(
            r.blocks.iter().any(|b| matches!(
                &b.content,
                BlockContent::Text { text } if text.contains("未以原文納入證據")
            )),
            "裁切了證據卻沒有在成果裡說"
        );
    }

    /* ── schema 重試與降級（§10） ─────────────────────────────── */

    /// 產生一個 schema 不合的區塊，並在被要求重出時修好。
    struct BrokenThenFixed {
        claim: ClaimKind,
        redrafts: u32,
        fix_on: u32,
    }

    impl Planner for BrokenThenFixed {
        fn draft(&mut self, _req: &DraftRequest<'_>) -> Result<Vec<Block>> {
            Ok(vec![Block {
                kind: BlockKind::Table,
                claim_kind: self.claim,
                // Table 配 Text：形狀不合
                content: BlockContent::Text {
                    text: "設計、開發、維運".into(),
                },
                source_refs: vec![],
            }])
        }

        fn redraft(
            &mut self,
            _req: &DraftRequest<'_>,
            b: &Block,
            reason: &str,
        ) -> Result<Option<Block>> {
            assert!(!reason.is_empty(), "重試時必須說明上一次為什麼不合");
            self.redrafts += 1;
            if self.redrafts >= self.fix_on {
                return Ok(Some(Block {
                    kind: BlockKind::Table,
                    claim_kind: b.claim_kind,
                    content: BlockContent::Table {
                        headers: vec!["項目".into()],
                        rows: vec![vec!["設計".into()]],
                    },
                    source_refs: vec![],
                }));
            }
            Ok(Some(b.clone()))
        }
    }

    #[test]
    fn a_malformed_block_is_retried_and_kept_once_it_validates() {
        let (s, m, cursor) = seeded();
        let mut p = BrokenThenFixed {
            claim: ClaimKind::Inference,
            redrafts: 0,
            fix_on: 1,
        };
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut p,
            &CharUpperBound,
        )
        .unwrap();
        assert_eq!(p.redrafts, 1);
        assert!(r.degraded.is_empty());
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].kind, BlockKind::Table);
    }

    #[test]
    fn retries_are_capped_and_a_non_fact_block_then_degrades_to_a_paragraph() {
        let (s, m, cursor) = seeded();
        let mut p = BrokenThenFixed {
            claim: ClaimKind::Inference,
            redrafts: 0,
            fix_on: 99,
        };
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut p,
            &CharUpperBound,
        )
        .unwrap();
        assert_eq!(p.redrafts, 2, "重試上限是兩次");
        assert_eq!(r.degraded.len(), 1);
        assert_eq!(r.degraded[0].original_kind, "table");
        assert_eq!(r.blocks[0].kind, BlockKind::Paragraph);
        // 降級不改變 claim_kind
        assert_eq!(r.blocks[0].claim_kind, ClaimKind::Inference);
        assert!(matches!(
            &r.blocks[0].content,
            BlockContent::Text { text } if text.contains("設計")
        ));
    }

    #[test]
    fn a_malformed_fact_block_is_removed_rather_than_degraded() {
        let (s, m, cursor) = seeded();
        let mut p = BrokenThenFixed {
            claim: ClaimKind::Fact,
            redrafts: 0,
            fix_on: 99,
        };
        let r = generate(
            &s,
            &GenerationRequest {
                meeting: m,
                through_event_seq: cursor,
                prompt: "",
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of: None,
            },
            &mut p,
            &CharUpperBound,
        )
        .unwrap();
        // 降級成 Paragraph 會讓沒通過 §9.6 的內容照樣渲染，等於繞過驗證
        assert!(r.degraded.is_empty());
        assert!(r.blocks.is_empty());
        assert!(r.rejected_blocks > 0);
    }

    #[test]
    fn the_index_excludes_speakers_first_heard_after_the_cursor() {
        let (mut s, m, cursor) = seeded();
        // 快照凍結之後才第一次聽到的語者
        s.append(
            m,
            &[(
                DomainEvent::SpeakerProposed {
                    speaker_id: "late".into(),
                    ordinal: 9,
                    proposed_name: None,
                    provider_labels: vec![],
                },
                Timeline::new(99_000, 99_000),
            )],
        )
        .unwrap();

        let index = build_index(&s, m, cursor).unwrap();
        // 片段與筆記都以游標凍結，語者沒有理由例外：本輪證據裡沒有這個人
        // 說過的任何一句話，把他放進 Prompt 只會讓模型以為他在場。
        assert!(
            !index.speakers.iter().any(|n| n.display == "late"),
            "游標之後才出現的語者進了本輪證據"
        );
    }
}
