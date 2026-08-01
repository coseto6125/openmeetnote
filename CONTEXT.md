# 領域語彙

這份檔案給的是名字,不是規格。規格在 `BLUEPRINT.md`,衝突時以藍圖為準。
列在這裡的詞,是讀程式碼時如果理解錯就會寫出錯誤程式的那些。

## 時間

三種時間必須分開,而且要落在欄位名稱上。混用是本專案最容易犯也最難發現的錯。

| 詞 | 意義 | 用在哪 |
|---|---|---|
| **meeting_time_ms** | 含暫停與靜音的會議時間軸 | 筆記、逐字稿與所有引用的定位基準 |
| **captured_audio_ms** | 只計有錄音資料的位置 | 音訊檔內定位 |
| **wall_clock** | RFC 3339 UTC 字串 | 只供顯示,不作為引用基準 |

**Timeline** 是把前兩者綁在一起的值型別。只帶其中一條的呼叫端遲早會用錯基準。

## 事件與投影

- **決定性事件(decisive event)**:會進入 `meeting_events` 的事件。partial 逐字稿不是決定性事件。
- **事件日誌(event log)**:`meeting_events`,唯一真實來源。其餘資料表都是它的投影。
- **投影(projection)**:能從日誌完全重建的查詢用資料表。寫入路徑與重建路徑共用同一個 `project()`。
- **seq**:決定性事件的序號,由 `meetings.high_seq` 配發,只前進不倒退。
- **快照游標(snapshot cursor)**:`through_event_seq`。凍結一輪生成的涵蓋範圍,生成期間後續錄音繼續寫入。

## 逐字稿

- **片段(segment)**:一段連續發言。以 `segment_id` 識別,內容以不可變的 **revision** 累積。
- **origin**:這個 revision 來自 `provider` 還是 `user`。使用者修訂永遠不被 Provider 覆蓋,版本號再高也不行。
- **stability**:`partial` 或 `final`。已定稿的片段不接受 partial。
- **音訊區間(audio interval)**:去重身分。重連與輪替之後 Provider 的識別碼全會變,唯一不變的是 `(track, captured_start_ms, captured_end_ms)`,正規化量子 20 ms。
- **軌道(track)**:`mic` 或 `system`。先驗只到「本機 vs 遠端」,不能再往「遠端只有一人」推。

## 語者

- **提出(proposed)**:第一次聽到某位語者。必須先提出才能確認,否則確認會更新 0 列而無聲消失。
- **暫定名稱(proposed_name)**:自我介紹推定出來的名字。目前沒有生產者,那是 M3 的工作。
- **確認名稱(confirmed_name)**:使用者給的名字。優先於暫定名稱。
- **ordinal**:全域的出現序。顯示用的「語者 N」只數遠端語者,因為麥克風軌會佔掉一格。

## 成果文件

- **claim_kind**:`Fact` / `Inference` / `Suggestion` / `Gap`。與區塊的結構種類正交,沒有預設值,缺漏即驗證失敗。
- **引用義務**:只有 `Fact` 必須帶引用。`Inference` 的義務是被清楚標示,不是提供逐字出處。
- **引用驗證(citation verification)**:程式執行的三項檢查,不交給模型自評。身分與版本在快照範圍內、locator 在範圍內、引文正規化後是子字串且雜湊相符。
  - 界限:逐字比對只證明引用存在於證據中,不證明區塊的論述被它支持。UI 不得讓引用標記看起來等於「已驗證為真」。
- **生成版本(generation run)**:文件的一個版本。`documents` 是文件本身,`generation_runs` 是它的版本鏈。

## 模組

- **Store**:事件日誌與投影。唯一的 `project()` 執行點。
- **StoreHandle**:`write()` 取獨佔連線(持鎖必須短),`reader()` 另開唯讀連線給背景工作。
- **Session**:會議狀態機。命令進、批次事件出,不直接碰 Audio、STT 或 SQLite。
- **TranscriptSource**:STT 的接縫。Fixture 與真實 Adapter 實作同一個 trait。
- **Planner**:生成草稿的接縫。規劃、驗證與停止條件都在它外面,換 Provider 不改變任何規則。
- **SecretStore**:OS 憑證庫的接縫。密鑰不進 SQLite、設定檔、逐字稿或日誌。

## 反覆出現的失敗形狀

**畫面說有、磁碟說沒有。** 決定性事件會先送到 UI 再寫入磁碟,中間任何一段失敗都會製造這個狀態。
目前已經踩過三次:`document_blocks.kind` 的 CHECK 清單對不上、`SpeakerConfirmed` 更新 0 列、
以及投影靜默忽略事件。

防線有三層,新增任何寫入路徑時都要確認三層都在:

1. `project()` 的每個 UPDATE 分支宣告預期影響列數,不符即 `StoreError::Corrupt`。
2. 寫入失敗設定 `journal_error`,之後所有產生新事件的命令都被拒絕。
3. 每個事件批次帶著 `journalError` 送到 UI,畫面立刻說實話,不等下一個命令被拒。
