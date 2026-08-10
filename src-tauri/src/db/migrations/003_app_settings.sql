-- 不屬於任何 Provider 的應用設定。
--
-- provider_settings 的主鍵是 kind（stt/llm），裝不下「這台機器要不要保留
-- 原音」這種與 Provider 無關的偏好，而把它塞進那張表的 options JSON 會讓
-- 一個 Provider 的設定決定另一件事的行為。
--
-- 刻意是 key-value 而不是每個偏好一個欄位：偏好會長，欄位每加一個就要一次
-- migration，而這裡沒有任何查詢需要對單一偏好做條件過濾。
CREATE TABLE app_settings (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

-- 預設保留原音。逐字稿沒有原音就無法被驗證：轉錯字、漏掉發言，事後只能
-- 憑印象爭論，也沒辦法換模型重跑同一段話比較。代價是磁碟（兩軌約
-- 230 MB/小時），使用者可以在設定頁關掉，或事後刪掉單場的音檔。
INSERT INTO app_settings (key, value) VALUES ('keep_audio', '1');
