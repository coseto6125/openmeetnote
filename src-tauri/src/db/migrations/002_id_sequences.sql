-- 全域 id 的配發器。
--
-- `documents` 與 `generation_runs` 的 id 由事件自帶（否則重播時 rowid 會隨插入
-- 順序改變），因此配發者在程式這邊。原本的做法是各自讀一次 `MAX(id)+1`，而那
-- 在兩個配發者同時存在時必然撞號：錄音中的 Session 在會議開始時就把種子讀進
-- 記憶體，歷史頁的摘要每次重讀，兩邊都算出同一個號碼。撞上之後 `SnapshotCreated`
-- 的 ON CONFLICT DO NOTHING 靜默吞掉，後續的 GenerationCompleted 就更新到另一場
-- 會議的版本上，把它的內容刪掉換成這一場的。
--
-- 這張表不是投影：`rebuild_projections` 清掉 documents 與 generation_runs 再重播
-- 時，它必須留著。號碼只前進不倒退，刪掉會議留下的空號永遠不再使用 —— 空號是
-- 沒有代價的，重複使用的號碼有。
CREATE TABLE id_sequences (
  name TEXT PRIMARY KEY,
  next INTEGER NOT NULL
);

-- 從既有資料續號，不是從 1 開始：這個 migration 會跑在已經有內容的資料庫上。
INSERT INTO id_sequences (name, next)
SELECT 'documents', COALESCE((SELECT MAX(id) FROM documents), 0) + 1;
INSERT INTO id_sequences (name, next)
SELECT 'generation_runs', COALESCE((SELECT MAX(id) FROM generation_runs), 0) + 1;
