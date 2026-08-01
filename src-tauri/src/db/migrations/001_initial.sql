-- Schema v1 — BLUEPRINT.md §11 初始資料模型
--
-- 兩條貫穿全域的規則寫在這裡，後續 migration 不得違反：
--
-- 1. meeting_events 是唯一真實來源，其餘表都是投影。任何寫入投影的交易
--    必須在同一個交易內附上對應事件，否則就出現「投影有、日誌沒有」的
--    狀態，災難復原時重播會漏掉它。
-- 2. 三種時間分開落在欄位名稱上：captured_audio_ms 只計有錄音資料的位置、
--    meeting_time_ms 含暫停與靜音、wall_clock 只供顯示。引用一律以
--    meeting_time_ms 定位。

CREATE TABLE meetings (
    id            INTEGER PRIMARY KEY,
    title         TEXT    NOT NULL,
    state         TEXT    NOT NULL CHECK (state IN
                      ('idle','recording','paused','stopping','finalizing','completed','failed')),
    started_at    TEXT,
    ended_at      TEXT,
    -- 會議結束時的兩條時間軸終值，列表頁不必重播事件就能顯示長度
    meeting_time_ms   INTEGER NOT NULL DEFAULT 0,
    captured_audio_ms INTEGER NOT NULL DEFAULT 0,
    -- 已配發到的最大 seq。重開會議時從這裡接續，不從 MAX(seq) 推算，
    -- 因為刪除投影不該讓 seq 倒退。
    high_seq      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT    NOT NULL
);

CREATE TABLE meeting_events (
    meeting_id      INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,
    kind            TEXT    NOT NULL,
    entity_id       TEXT,
    entity_revision INTEGER,
    payload         TEXT    NOT NULL,
    meeting_time_ms   INTEGER NOT NULL,
    captured_audio_ms INTEGER NOT NULL,
    created_at      TEXT    NOT NULL,
    PRIMARY KEY (meeting_id, seq)
) WITHOUT ROWID;

CREATE INDEX idx_events_kind ON meeting_events (meeting_id, kind, seq);

CREATE TABLE audio_segments (
    id                INTEGER PRIMARY KEY,
    meeting_id        INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    track             TEXT    NOT NULL CHECK (track IN ('mic','system')),
    source_epoch      INTEGER NOT NULL,
    path              TEXT    NOT NULL UNIQUE,
    captured_start_ms INTEGER NOT NULL,
    captured_end_ms   INTEGER NOT NULL,
    meeting_start_ms  INTEGER NOT NULL,
    meeting_end_ms    INTEGER NOT NULL,
    -- 壓縮靜音自成的 segment：captured 長度為零、meeting 長度不為零。
    -- §11 要求這個組合可辨識並納入測試，因此獨立成欄位而不是靠長度推斷。
    is_silence_fill   INTEGER NOT NULL DEFAULT 0 CHECK (is_silence_fill IN (0,1)),
    checksum          TEXT    NOT NULL,
    created_event_seq INTEGER NOT NULL,
    CHECK (captured_end_ms >= captured_start_ms),
    CHECK (meeting_end_ms  >= meeting_start_ms),
    CHECK (is_silence_fill = 0 OR captured_end_ms = captured_start_ms)
);

CREATE INDEX idx_audio_meeting ON audio_segments (meeting_id, track, meeting_start_ms);

CREATE TABLE speakers (
    id              TEXT    NOT NULL,
    meeting_id      INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    ordinal         INTEGER NOT NULL,
    proposed_name   TEXT,
    confirmed_name  TEXT,
    status          TEXT    NOT NULL CHECK (status IN ('unconfirmed','proposed','confirmed','merged')),
    -- Provider 對同一位語者用過的標籤，重連後標籤會變，保留供追溯
    provider_labels TEXT    NOT NULL DEFAULT '[]',
    merged_into     TEXT,
    PRIMARY KEY (meeting_id, id)
) WITHOUT ROWID;

-- 不可變的逐字稿片段版本。永不就地更新或刪除。
CREATE TABLE transcript_segment_revisions (
    meeting_id        INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    segment_id        INTEGER NOT NULL,
    revision          INTEGER NOT NULL,
    text              TEXT    NOT NULL,
    speaker_id        TEXT,
    track             TEXT    NOT NULL CHECK (track IN ('mic','system')),
    meeting_start_ms  INTEGER NOT NULL,
    meeting_end_ms    INTEGER NOT NULL,
    captured_start_ms INTEGER NOT NULL,
    captured_end_ms   INTEGER NOT NULL,
    echo_likelihood   REAL,
    overlap_group_id  TEXT,
    -- 以下三個 Provider 識別碼只供診斷，不參與判重（§5.3.2）。
    -- 去重鍵是正規化後的 (track, captured_start_ms, captured_end_ms)。
    provider_stream_id  TEXT,
    provider_result_id  TEXT,
    rollover_generation INTEGER NOT NULL DEFAULT 0,
    origin            TEXT    NOT NULL CHECK (origin IN ('provider','user')),
    created_event_seq INTEGER NOT NULL,
    created_at        TEXT    NOT NULL,
    PRIMARY KEY (meeting_id, segment_id, revision)
) WITHOUT ROWID;

CREATE INDEX idx_rev_dedupe ON transcript_segment_revisions
    (meeting_id, track, captured_start_ms, captured_end_ms);

-- 詞或語句層級的語者指派（§18 重疊發言）。
--
-- 沒有列 = 該版本用片段層級的 speaker_id；有列 = 細粒度指派。
-- 兩種粒度都以 meeting_time_ms 區間定位，引用模型因此不隨粒度改變，
-- 之後補上細粒度是加資料，不是改 schema。
CREATE TABLE transcript_segment_speaker_spans (
    meeting_id       INTEGER NOT NULL,
    segment_id       INTEGER NOT NULL,
    revision         INTEGER NOT NULL,
    span_index       INTEGER NOT NULL,
    speaker_id       TEXT    NOT NULL,
    meeting_start_ms INTEGER NOT NULL,
    meeting_end_ms   INTEGER NOT NULL,
    char_start       INTEGER NOT NULL,
    char_end         INTEGER NOT NULL,
    PRIMARY KEY (meeting_id, segment_id, revision, span_index),
    FOREIGN KEY (meeting_id, segment_id, revision)
        REFERENCES transcript_segment_revisions (meeting_id, segment_id, revision)
        ON DELETE CASCADE
) WITHOUT ROWID;

-- 片段目前狀態的查詢投影。歷史內容一律取自 revisions。
CREATE TABLE transcript_segments (
    meeting_id       INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    id               INTEGER NOT NULL,
    current_revision INTEGER NOT NULL,
    stability        TEXT    NOT NULL CHECK (stability IN ('partial','final')),
    user_edited      INTEGER NOT NULL DEFAULT 0 CHECK (user_edited IN (0,1)),
    meeting_start_ms INTEGER NOT NULL,
    PRIMARY KEY (meeting_id, id)
) WITHOUT ROWID;

CREATE INDEX idx_segments_time ON transcript_segments (meeting_id, meeting_start_ms);

CREATE TABLE notes (
    meeting_id        INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    id                INTEGER NOT NULL,
    event_seq         INTEGER NOT NULL,
    text              TEXT    NOT NULL,
    meeting_time_ms   INTEGER NOT NULL,
    captured_audio_ms INTEGER NOT NULL,
    removed           INTEGER NOT NULL DEFAULT 0 CHECK (removed IN (0,1)),
    created_at        TEXT    NOT NULL,
    PRIMARY KEY (meeting_id, id)
) WITHOUT ROWID;

CREATE TABLE attachments (
    id         INTEGER PRIMARY KEY,
    meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    event_seq  INTEGER NOT NULL,
    path       TEXT    NOT NULL,
    mime       TEXT    NOT NULL,
    sha256     TEXT    NOT NULL,
    status     TEXT    NOT NULL CHECK (status IN ('pending','extracted','failed','removed')),
    created_at TEXT    NOT NULL
);

CREATE TABLE attachment_chunks (
    id                  INTEGER PRIMARY KEY,
    attachment_id       INTEGER NOT NULL REFERENCES attachments(id) ON DELETE CASCADE,
    extraction_revision INTEGER NOT NULL,
    page_no             INTEGER,
    start_offset        INTEGER NOT NULL,
    end_offset          INTEGER NOT NULL,
    text                TEXT    NOT NULL
);

CREATE INDEX idx_chunks_attachment ON attachment_chunks (attachment_id, extraction_revision);

-- 成果文件本身，與它的生成版本分離。
CREATE TABLE documents (
    id         INTEGER PRIMARY KEY,
    meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    purpose    TEXT    NOT NULL,
    title      TEXT    NOT NULL,
    created_at TEXT    NOT NULL
);

CREATE TABLE generation_runs (
    id               INTEGER PRIMARY KEY,
    document_id      INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    parent_run_id    INTEGER REFERENCES generation_runs(id),
    version_no       INTEGER NOT NULL,
    -- 快照游標：固定本輪涵蓋範圍，生成期間後續錄音繼續寫入不影響本輪
    through_event_seq INTEGER NOT NULL,
    prompt           TEXT    NOT NULL DEFAULT '',
    status           TEXT    NOT NULL CHECK (status IN ('queued','running','completed','failed')),
    usage            TEXT    NOT NULL DEFAULT '{}',
    failure_reason   TEXT,
    created_at       TEXT    NOT NULL,
    UNIQUE (document_id, version_no)
);

CREATE TABLE document_blocks (
    id         INTEGER PRIMARY KEY,
    run_id     INTEGER NOT NULL REFERENCES generation_runs(id) ON DELETE CASCADE,
    position   INTEGER NOT NULL,
    -- §10 的受控區塊種類。與 claim_kind 正交：一個段落可以是 Fact，
    -- 一張表格也可以是 Gap。這份清單必須與 document::BlockKind 完全一致，
    -- 有測試把兩邊釘在一起。
    kind       TEXT    NOT NULL CHECK (kind IN
                   ('heading','paragraph','bulletList','table','mermaidDiagram',
                    'callout','decision','actionItem','gap','suggestion',
                    'sourceLink','transcriptExcerpt')),
    -- §3.4 的內容分類。刻意 NOT NULL 且無 DEFAULT：
    -- 缺這個欄位是 schema 驗證失敗，不是可以猜的東西。要退也只能退 fact
    -- （fail-closed，逼出引用義務），絕不退 inference。
    claim_kind TEXT    NOT NULL CHECK (claim_kind IN ('fact','inference','suggestion','gap')),
    content    TEXT    NOT NULL,
    UNIQUE (run_id, position)
);

CREATE TABLE source_refs (
    id                 INTEGER PRIMARY KEY,
    block_id           INTEGER NOT NULL REFERENCES document_blocks(id) ON DELETE CASCADE,
    source_kind        TEXT    NOT NULL CHECK (source_kind IN
                           ('transcript_segment','note','attachment_chunk')),
    source_id          TEXT    NOT NULL,
    -- 固定版本。單有版本不足以重現引用，所以引文與雜湊一起存，
    -- §9.6 的逐字比對才有東西可比。
    source_revision    INTEGER NOT NULL,
    locator            TEXT    NOT NULL,
    quoted_text        TEXT    NOT NULL,
    quoted_text_sha256 TEXT    NOT NULL,
    validation_status  TEXT    NOT NULL CHECK (validation_status IN
                           ('valid','stale','invalid','unverified'))
);

CREATE INDEX idx_refs_block ON source_refs (block_id);

-- 只放非敏感設定。密鑰走 OS keychain，不進這張表（§5.6、§14）。
CREATE TABLE provider_settings (
    kind      TEXT NOT NULL CHECK (kind IN ('stt','llm')),
    provider  TEXT NOT NULL,
    model     TEXT NOT NULL DEFAULT '',
    base_url  TEXT NOT NULL DEFAULT '',
    options   TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (kind)
) WITHOUT ROWID;
