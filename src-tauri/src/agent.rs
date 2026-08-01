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
    /// 真實 Planner 回報的錯誤。Fixture 不會產生，因此本 build 用不到。
    #[allow(dead_code)]
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
    pub speakers: Vec<String>,
    /// 需要原文時才取用的候選
    pub segments: Vec<StoredSegment>,
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
        .meeting(meeting)?
        .speakers
        .into_iter()
        .map(|s| s.confirmed_name.or(s.proposed_name).unwrap_or(s.speaker_id))
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
    pub speakers: Vec<String>,
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
        .chain(index.speakers.iter().map(|s| tok.count(s)))
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
    pub evidence_tokens: usize,
    pub segments_omitted: usize,
    /// 未通過驗證而被移除的區塊數，供 §15.1 的 Interface 測試斷言
    pub rejected_blocks: usize,
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
    } = *req;
    let index = build_index(store, meeting, through_event_seq)?;
    // 指令佔用以實際內容量測。這裡的指令就是 Prompt 加上 schema 說明。
    let instruction_tokens = tok.count(prompt) + tok.count(SCHEMA_BRIEF);
    let allowance = evidence_allowance(budget, instruction_tokens)?;
    let pack = pack_evidence(&index, allowance, tok)?;

    let mut best: Vec<Block> = Vec::new();
    let mut best_verdicts: Vec<BlockVerdict> = Vec::new();
    let mut rejections: Vec<String> = Vec::new();
    let mut rejected_total = 0usize;
    let mut degraded: Vec<Degraded> = Vec::new();
    let mut round = 0u32;

    let stop_reason = loop {
        round += 1;
        let req = DraftRequest {
            prompt,
            evidence: &pack,
            rejections: &rejections,
            round,
        };
        let drafted = planner.draft(&req)?;
        // schema 先結清，再送引用驗證：形狀都不對的區塊沒必要去查證據
        let (drafted, degraded_now, removed) = settle_schema(planner, &req, drafted)?;
        rejected_total += removed;
        degraded.extend(degraded_now);
        let (admitted, verdicts) =
            document::verify_blocks(store, meeting, through_event_seq, &drafted)?;
        let rejected: Vec<String> = verdicts
            .iter()
            .filter(|v| !v.admitted)
            .filter_map(|v| v.reason.clone())
            .collect();
        rejected_total += rejected.len();

        let improved = admitted.len() > best.len();
        if improved {
            best = admitted;
            best_verdicts = verdicts;
        }

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
        // 沒有實質改善就停：再跑一輪只是重複付費（§9.3）
        if !improved {
            break if best.is_empty() {
                StopReason::NothingAdmissible
            } else {
                StopReason::NoImprovement
            };
        }
        rejections = rejected;
    };

    Ok(GenerationResult {
        blocks: best,
        verdicts: best_verdicts,
        rounds: round,
        stop_reason,
        degraded,
        evidence_tokens: pack.tokens_used,
        segments_omitted: pack.segments_omitted,
        rejected_blocks: rejected_total,
    })
}

/// 送給模型的 schema 說明。放在這裡是為了讓指令佔用能被實際量測。
const SCHEMA_BRIEF: &str = "\
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

        let mut blocks = vec![Block {
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
        }];

        // 每個片段各出一個 Fact，引文逐字取自該版本的開頭
        for s in req.evidence.segments.iter().take(6) {
            let quote: String = s.text.chars().take(12).collect();
            if quote.trim().is_empty() {
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

        if req.evidence.segments_omitted > 0 {
            blocks.push(Block {
                kind: BlockKind::Gap,
                claim_kind: ClaimKind::Gap,
                content: BlockContent::Text {
                    text: format!(
                        "本輪有 {} 段逐字稿未以原文納入證據，這些區間的內容未被涵蓋。",
                        req.evidence.segments_omitted
                    ),
                },
                source_refs: vec![],
            });
        }
        blocks.push(Block {
            kind: BlockKind::Gap,
            claim_kind: ClaimKind::Gap,
            content: BlockContent::Text {
                text: "建立快照時仍是 partial 的錄音區間不在本輪證據內。".into(),
            },
            source_refs: vec![],
        });
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
            .chain(index.speakers.iter().map(|x| x.chars().count()))
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
}
