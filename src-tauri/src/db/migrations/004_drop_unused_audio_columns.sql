-- 拿掉 audio_segments 兩個從來只寫得出常數的欄位。
--
-- `source_epoch` 想記的是「裝置重開過，跨這條線的樣本數不可以直接相加」，
-- `is_silence_fill` 想記的是「這一段代表被壓縮掉的靜音」。兩件事在寫入端
-- 都不存在：寫檔那一條看到擷取時間跳掉就切一段，暫停期間的 chunk 在分流
-- 就被丟掉，沒有任何路徑產生靜音段，於是兩欄一路寫 0 與 false。
--
-- 而且相鄰兩列已經把這兩件事說完了。captured 時間軸不含暫停，所以同一軌
-- 前一段的 captured_end 與下一段的 captured_start 相等就代表音訊接得起來，
-- 差值不為零就代表中間掉了音訊（裝置重開、寫入佇列滿）。多留兩個欄位只是
-- 讓「查得到」與「是真的」看起來一樣。BLUEPRINT §11 的欄位清單本來也沒有
-- 它們，那一節要求的正是拿實際查詢驗證 schema。
--
-- is_silence_fill 出現在 table check 裡，DROP COLUMN 動不了，只能重建。
CREATE TABLE audio_segments_new (
    id                INTEGER PRIMARY KEY,
    meeting_id        INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    track             TEXT    NOT NULL CHECK (track IN ('mic','system')),
    path              TEXT    NOT NULL UNIQUE,
    captured_start_ms INTEGER NOT NULL,
    captured_end_ms   INTEGER NOT NULL,
    meeting_start_ms  INTEGER NOT NULL,
    meeting_end_ms    INTEGER NOT NULL,
    checksum          TEXT    NOT NULL,
    created_event_seq INTEGER NOT NULL,
    CHECK (captured_end_ms >= captured_start_ms),
    CHECK (meeting_end_ms  >= meeting_start_ms)
);

INSERT INTO audio_segments_new
    (id, meeting_id, track, path, captured_start_ms, captured_end_ms,
     meeting_start_ms, meeting_end_ms, checksum, created_event_seq)
SELECT id, meeting_id, track, path, captured_start_ms, captured_end_ms,
       meeting_start_ms, meeting_end_ms, checksum, created_event_seq
  FROM audio_segments;

-- 連同 idx_audio_meeting 一起消失，改名之後再建回來
DROP TABLE audio_segments;
ALTER TABLE audio_segments_new RENAME TO audio_segments;
CREATE INDEX idx_audio_meeting ON audio_segments (meeting_id, track, meeting_start_ms);
