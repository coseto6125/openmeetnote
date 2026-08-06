//! 完整流程的端到端測試：音訊進去，可交付的 HTML 出來。
//!
//! 上游的 `stt_pipeline` 守轉錄品質，`agent_cli` 守生成路徑，但兩者都只覆蓋
//! 自己那一段。真正會壞在接縫上的東西——時間基準沒對齊、引用指向不存在的
//! 片段、缺口被靜默吞掉——只有把整條鏈接起來才看得到。
//!
//! 音訊與模型都不進版本庫，缺席時整份跳過。

use std::path::PathBuf;

use openmeetnote_lib::agent::{self, BudgetInputs, GenerationRequest, Limits, Tokenizer};
use openmeetnote_lib::document::{self, Block, RenderContext};
use openmeetnote_lib::model::{Origin, Timeline, Track};
use openmeetnote_lib::store::{DomainEvent, SegmentRevision, Store};
use openmeetnote_lib::stt::{load_wav_16k_mono, whisper::Whisper};

fn assets() -> Option<PathBuf> {
    let dir = PathBuf::from(
        std::env::var("OMN_TEST_ASSETS").unwrap_or_else(|_| "/home/enor/whisper-bench".into()),
    );
    dir.is_dir().then_some(dir)
}

/// 依字元數估算 token。真實 Planner 用自己的 tokenizer，這裡只需要一個
/// 決定性的數字讓預算計算能跑。
struct CharTokenizer;

impl Tokenizer for CharTokenizer {
    fn count(&self, s: &str) -> usize {
        s.chars().count()
    }
}

fn revision(id: u64, text: &str, start_ms: u64, end_ms: u64) -> SegmentRevision {
    SegmentRevision {
        segment_id: id,
        revision: 1,
        text: text.into(),
        speaker_id: Some("s1".into()),
        track: Track::System,
        meeting_start_ms: start_ms,
        meeting_end_ms: end_ms,
        captured_start_ms: start_ms,
        captured_end_ms: end_ms,
        echo_likelihood: None,
        overlap_group_id: None,
        provider_stream_id: Some("whisper".into()),
        provider_result_id: Some(format!("r{id}")),
        rollover_generation: 0,
        origin: Origin::Provider,
        speaker_spans: Vec::new(),
    }
}

#[test]
fn test_audio_becomes_a_deliverable_document() {
    let Some(dir) = assets() else {
        eprintln!("略過：找不到測試素材目錄");
        return;
    };
    let (model, wav) = (
        dir.join("models/ggml-large-v3-turbo-q5_0.bin"),
        dir.join("near.wav"),
    );
    if !model.exists() || !wav.exists() {
        eprintln!("略過：缺少模型或測試音訊");
        return;
    }

    // ── 1. 音訊 → 逐字稿 ──────────────────────────────────────────
    let samples = load_wav_16k_mono(wav.to_str().unwrap()).expect("讀取音訊");
    let segments = Whisper::load(model.to_str().unwrap(), 4)
        .expect("載入 whisper")
        .transcribe(&samples)
        .expect("轉錄");
    assert!(segments.len() >= 5, "轉錄結果太少，後面的斷言會失去意義");

    // ── 2. 逐字稿 → 事件日誌 ──────────────────────────────────────
    let mut store = Store::new(openmeetnote_lib::db::open_in_memory().expect("開資料庫"));
    let meeting = store.create_meeting("端到端測試會議").expect("建立會議");
    let events: Vec<(DomainEvent, Timeline)> = segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            (
                DomainEvent::TranscriptSegmentFinalized {
                    segment: revision(i as u64 + 1, &s.text, s.start_ms, s.end_ms),
                },
                Timeline::new(s.start_ms, s.start_ms),
            )
        })
        .collect();
    let seq = *store
        .append(meeting, &events)
        .expect("寫入事件")
        .last()
        .expect("至少要有一個事件");

    // 投影必須讀得回來，而且是快照當時的版本
    let stored = store.segments_through(meeting, seq).expect("讀取逐字稿");
    assert_eq!(stored.len(), segments.len(), "投影漏了片段");

    // ── 3. 逐字稿 → 成果區塊 ──────────────────────────────────────
    // 用不呼叫 Provider 的規劃器：這個測試守的是接縫，不是模型品質。
    // 模型品質由 stt_pipeline 與 agent_cli 各自負責。
    let result = agent::generate(
        &store,
        &GenerationRequest {
            meeting,
            through_event_seq: seq,
            prompt: "整理成會議紀錄",
            budget: BudgetInputs::default(),
            limits: Limits::default(),
            revise_of: None,
        },
        &mut agent::FixturePlanner,
        &CharTokenizer,
    )
    .expect("生成");

    assert!(!result.blocks.is_empty(), "沒有產出任何區塊");
    assert_eq!(result.rejected_blocks, 0, "有區塊沒通過引用驗證");

    // 每個 Fact 都必須帶得回逐字稿，而且指向真的存在的片段版本
    let ids: std::collections::HashSet<String> =
        stored.iter().map(|s| s.segment_id.to_string()).collect();
    for b in &result.blocks {
        if b.claim_kind != openmeetnote_lib::model::ClaimKind::Fact {
            continue;
        }
        assert!(!b.source_refs.is_empty(), "Fact 沒有引用");
        for r in &b.source_refs {
            assert!(
                ids.contains(&r.source_id),
                "引用指向不存在的片段 {}",
                r.source_id
            );
            let cited = stored
                .iter()
                .find(|s| s.segment_id.to_string() == r.source_id)
                .unwrap();
            assert!(
                cited.text.contains(&r.quoted_text),
                "引文不是逐字取自該片段：{:?} 不在 {:?} 裡",
                r.quoted_text,
                cited.text
            );
        }
    }

    // ── 4. 成果區塊 → HTML ────────────────────────────────────────
    let html = document::render_html(
        &RenderContext {
            title: "端到端測試會議",
            version_no: 1,
            through_event_seq: seq,
            created_at: "2026-08-05T00:00:00Z",
            transcript: &stored,
            speakers: &store.speakers_through(meeting, seq).expect("讀取語者"),
        },
        &result.blocks,
    );

    assert!(
        html.starts_with("<!doctype html>") || html.contains("<html"),
        "不是完整的 HTML"
    );
    // 逐字稿必須附在匯出裡（§10）：沒有它，引用就沒有可回溯的對象
    let first = &stored[0].text;
    assert!(
        html.contains(&document::escape(first)),
        "匯出沒有包含逐字稿內容"
    );
    // 轉錄出來的專有名詞不該在渲染過程中被改動
    if first.contains("原住民") {
        assert!(html.contains("原住民"), "內容在渲染時被改掉了");
    }
}

#[test]
fn test_a_citation_to_a_revised_segment_is_detectable() {
    // 引用固定版本，片段之後被改過的話必須看得出來（§11）。
    // 沒有這個，讀者會以為引文對應的還是現在的內容。
    let mut store = Store::new(openmeetnote_lib::db::open_in_memory().expect("開資料庫"));
    let meeting = store.create_meeting("修訂測試").expect("建立會議");

    store
        .append(
            meeting,
            &[(
                DomainEvent::TranscriptSegmentFinalized {
                    segment: revision(1, "原本的內容", 0, 1000),
                },
                Timeline::new(0, 0),
            )],
        )
        .expect("寫入");

    let mut edited = revision(1, "使用者改過的內容", 0, 1000);
    edited.revision = 2;
    edited.origin = Origin::User;
    let seq = *store
        .append(
            meeting,
            &[(
                DomainEvent::TranscriptSegmentEdited { segment: edited },
                Timeline::new(0, 0),
            )],
        )
        .expect("寫入修訂")
        .last()
        .expect("事件序號");

    let stored = store.segments_through(meeting, seq).expect("讀取");
    assert_eq!(stored.len(), 1, "修訂應該更新同一個片段而不是新增");
    assert_eq!(stored[0].revision, 2);
    assert_eq!(stored[0].text, "使用者改過的內容");
    assert_eq!(
        stored[0].origin,
        Origin::User,
        "使用者修訂的來源標記遺失了，之後 Provider 的結果會蓋掉它"
    );
}

#[test]
fn test_the_export_survives_hostile_transcript_content() {
    // 會議裡有人念出 HTML，那段文字會走完整條管線進到匯出檔
    let mut store = Store::new(openmeetnote_lib::db::open_in_memory().expect("開資料庫"));
    let meeting = store.create_meeting("注入測試").expect("建立會議");
    let seq = *store
        .append(
            meeting,
            &[(
                DomainEvent::TranscriptSegmentFinalized {
                    segment: revision(1, "<script>alert('x')</script> 這是正常內容", 0, 1000),
                },
                Timeline::new(0, 0),
            )],
        )
        .expect("寫入")
        .last()
        .expect("事件序號");

    let stored = store.segments_through(meeting, seq).expect("讀取");
    let html = document::render_html(
        &RenderContext {
            title: "注入測試",
            version_no: 1,
            through_event_seq: seq,
            created_at: "2026-08-05T00:00:00Z",
            transcript: &stored,
            speakers: &store.speakers_through(meeting, seq).expect("讀取語者"),
        },
        &[] as &[Block],
    );
    assert!(!html.contains("<script>alert"), "逐字稿的腳本進了匯出檔");
    assert!(html.contains("這是正常內容"), "轉義把正常內容也弄丟了");
}

/// 真實音訊、真實模型、整條鏈走一遍：錄音 → 定稿 → 摘要 → 修訂 → 匯出 → 搜尋。
///
/// 上面那個測試用 FixturePlanner 守接縫，這一個用真的 CLI 守「在真實資料上
/// 串不串得起來」。兩者的失敗模式不同：合成證據永遠乾淨、長度可控、引文好對；
/// 真實逐字稿有錯字、有重複、有半句話，而引用驗證是逐字比對。
#[test]
#[ignore = "需要模型與音訊，還會真的呼叫 CLI 兩次，跑一次數分鐘。用 --ignored 執行"]
fn test_the_whole_chain_holds_on_real_audio() {
    use openmeetnote_lib::cli_planner::{CliKind, CliPlanner};

    let Some(dir) = assets() else {
        eprintln!("略過：找不到測試素材目錄");
        return;
    };
    let (model, wav) = (
        dir.join("models/ggml-large-v3-turbo-q5_0.bin"),
        dir.join("near.wav"),
    );
    if !model.exists() || !wav.exists() {
        eprintln!("略過：缺少模型或測試音訊");
        return;
    }
    let Some((exe, kind)) = (|| {
        for (name, k) in [("claude", CliKind::ClaudeCode), ("codex", CliKind::Codex)] {
            if let Some(p) = openmeetnote_lib::config::resolve_exe(name) {
                return Some((p, k));
            }
        }
        None
    })() else {
        eprintln!("略過：這台機器沒有 claude 或 codex");
        return;
    };

    // ── 1. 音訊 → 逐字稿 ──────────────────────────────────────────
    let samples = load_wav_16k_mono(wav.to_str().unwrap()).expect("讀取音訊");
    let segments = Whisper::load(model.to_str().unwrap(), 4)
        .expect("載入 whisper")
        .transcribe(&samples)
        .expect("轉錄");
    assert!(segments.len() >= 5, "轉錄結果太少");

    let mut store = Store::new(openmeetnote_lib::db::open_in_memory().expect("開資料庫"));
    let meeting = store.create_meeting("原住民基本法審查").expect("建立會議");
    let mut events: Vec<(DomainEvent, Timeline)> = segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            (
                DomainEvent::TranscriptSegmentFinalized {
                    segment: revision(i as u64 + 1, &s.text, s.start_ms, s.end_ms),
                },
                Timeline::new(s.start_ms, s.start_ms),
            )
        })
        .collect();
    // 人工筆記也要進得了成果（§17 第 5 點）
    events.push((
        DomainEvent::NoteAdded {
            note_id: 1,
            text: "散會前要確認下次開會時間".into(),
        },
        Timeline::new(segments.last().unwrap().end_ms, 0),
    ));
    let seq = *store
        .append(meeting, &events)
        .expect("寫入")
        .last()
        .unwrap();

    // ── 2. 第一版摘要（真實 CLI） ─────────────────────────────────
    let mut planner = CliPlanner::new(exe, kind).expect("建立 Planner");
    let run = |planner: &mut CliPlanner, store: &Store, prompt: &str, revise_of| {
        agent::generate(
            store,
            &GenerationRequest {
                meeting,
                through_event_seq: seq,
                prompt,
                budget: BudgetInputs::default(),
                limits: Limits::default(),
                revise_of,
            },
            planner,
            &CharTokenizer,
        )
    };

    let v1 = match run(&mut planner, &store, "整理這場會議", None) {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("登入") || msg.contains("login") || msg.contains("quota") {
                eprintln!("略過：CLI 尚未登入或額度用盡（{msg}）");
                return;
            }
            panic!("第一版失敗：{msg}");
        }
    };
    assert!(v1.failure_reason().is_none(), "第一版沒有可用內容");
    eprintln!(
        "v1：{} 個區塊、{} 筆引用、退回 {}、重複 {}",
        v1.blocks.len(),
        v1.blocks.iter().map(|b| b.source_refs.len()).sum::<usize>(),
        v1.rejected_blocks,
        v1.duplicates_removed
    );

    store
        .append(
            meeting,
            &[
                (
                    DomainEvent::SnapshotCreated {
                        document_id: 1,
                        run_id: 1,
                        parent_run_id: None,
                        version_no: 1,
                        purpose: "meeting-summary".into(),
                        title: "會議摘要".into(),
                        through_event_seq: seq,
                        prompt: "整理這場會議".into(),
                    },
                    Timeline::new(0, 0),
                ),
                (
                    DomainEvent::GenerationCompleted {
                        run_id: 1,
                        blocks: v1
                            .blocks
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
        .expect("寫入第一版");

    // ── 3. 修訂成第二版 ───────────────────────────────────────────
    let v2 = run(&mut planner, &store, "把決議的部分寫得更明確", Some(1)).expect("第二版失敗");
    assert!(v2.failure_reason().is_none(), "第二版沒有可用內容");
    assert!(
        v2.blocks.len() * 2 >= v1.blocks.len(),
        "修訂之後區塊數腰斬：{} → {}",
        v1.blocks.len(),
        v2.blocks.len()
    );

    // ── 4. 匯出 ───────────────────────────────────────────────────
    let stored = store.segments_through(meeting, seq).expect("讀取逐字稿");
    let html = document::render_html(
        &RenderContext {
            title: &store.meeting_title(meeting).unwrap(),
            version_no: 2,
            through_event_seq: seq,
            created_at: "2026-08-05T00:00:00Z",
            transcript: &stored,
            speakers: &store.speakers_through(meeting, seq).expect("讀取語者"),
        },
        &v2.blocks,
    );
    assert!(html.contains("原住民基本法審查"), "標題用的不是會議名稱");
    assert!(html.contains("id=\"s-transcript\""), "匯出沒有逐字稿");
    assert!(html.contains("版本 v2"));
    // 每一筆逐字稿引用都要在文件裡找得到落點
    for r in v2.blocks.iter().flat_map(|b| &b.source_refs) {
        if r.source_kind == "transcript_segment" {
            assert!(
                html.contains(&format!("id=\"seg-{}\"", r.source_id)),
                "引用 {} 在逐字稿裡沒有落點",
                r.source_id
            );
        }
    }
    // 真實逐字稿的內容不該在渲染過程中被改動
    let first = &stored[0].text;
    assert!(html.contains(&document::escape(first)), "逐字稿內容對不上");

    // ── 5. 搜尋找得回這場會議 ─────────────────────────────────────
    // 拿逐字稿裡真的出現過的字去搜，而不是我猜的字
    let needle: String = stored[0].text.chars().take(4).collect();
    let hits = store.search_meetings(needle.trim(), 3).expect("搜尋");
    assert!(
        hits.iter().any(|h| h.summary.id == meeting),
        "搜「{needle}」找不到這場會議"
    );
    let note_hits = store.search_meetings("下次開會時間", 3).expect("搜尋筆記");
    assert!(
        note_hits.iter().any(|h| h.summary.id == meeting),
        "搜不到人工筆記的內容"
    );
    eprintln!(
        "整條鏈走完：{} 段逐字稿、v2 {} 個區塊、匯出 {} 位元組",
        stored.len(),
        v2.blocks.len(),
        html.len()
    );
}
