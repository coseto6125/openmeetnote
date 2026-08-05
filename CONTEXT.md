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

## 本機 STT 的失敗形狀

這些是實作 M1／M2 時實際踩過的，每一個都會讓系統看起來「壞掉」而原因不明顯。

**串流 API 一次餵整段。** sherpa-onnx 的 VAD 是串流偵測器，`accept_waveform`
要一次餵一個 `window_size`。整段丟進去它只處理第一個 window，其餘直接丟掉，
表現是不論音訊多長都固定回報同一個數字（實測 314 ms）。

**把 VAD 切出的語音剪接後送轉錄。** 接縫會產生原本不存在的音訊變化，whisper
對跳接的輸入會直接開始編（實測轉出「中文字幕志願者」「謝謝觀看」這類訓練資料
殘留）。VAD 只能當閘門用來決定「這段要不要送」，不能拿它重組音訊。這個錯誤犯了
兩次，第二次藏在「累積多段再送」那一層。

**拿 VAD 的累計語音長度當閘門。** VAD 的語音段只在結束時吐出，跨越批次邊界的
語音會讓前一批算到 0、後一批算到超過批次長度的數字。用它判斷「這批有沒有人聲」
會整段丟掉真實內容（實測開頭 15 秒被丟）。要擋幻覺請用 RMS，那是無狀態的。

**同一批轉錄結果共用時間戳。** `Transcript` 用音訊區間做跨串流去重，一批五句
若拿到相同的區間，就會被當成同一段的不同版本，一句覆蓋一句，最後只剩最後一句。
每一句都必須帶自己的 `audio_span`。

**即時稿的視窗沒有上限。** 離線模型每次都要看完整段，buffer 一直長下去等於
每 800 ms 重算整場會議，計算量隨錄音時間線性上升，畫面最後會停在幾十秒前不動。

**Windows GUI 子系統不接 stderr。** `eprintln!` 在打包後的 app 裡等於什麼都
沒做。引擎載入失敗會完全無聲無息，只能靠寫檔（`stt.log`）才看得到。

**交叉編譯不會開 CPU SIMD。** `GGML_NATIVE` 偵測的是編譯主機而不是目標，
whisper 因此慢九倍（RTF 3.96 對 0.55）。驗證方式是反組譯數 AVX 指令，
不是看有沒有編譯成功。

**外部綁定崩潰時，先分清楚是綁定的錯還是二進位的錯。** `sherpa-rs` 的 Diarize
有兩個 Rust 側的 bug（null 指標不檢查、失敗路徑跳過資源釋放），補在
`vendor/sherpa-rs`；但補完仍崩在 DLL 內部同一個位移，那就不是能修的東西了。
分辨方法是看崩潰位移變不變：補了 Rust 側之後位移完全沒動，代表根本沒走到
那幾行。這種情況要換路徑而不是繼續補 —— 語者分辨改走聲紋比對，同樣達得到
目的，只是切點精度較差。

## CI 的失敗形狀

**建置產物快取與下載快取不同步。** `sherpa-rs-sys` 的 build script 把預編的
onnxruntime 下載到 `~/.cache/sherpa-rs`，那個路徑不在 `target/` 底下。只快取
`target/` 的話，快取命中時 build script 被判定為不需重跑，它的 `-L` 旗標照樣
送給連結器，但指向的目錄已經不存在，於是 `unable to find library -lonnxruntime`。
症狀是冷快取全綠、熱快取必掛，看起來像隨機失敗。凡是 build script 會往
`target/` 以外的地方寫東西，那個位置就必須跟 `target/` 用同一把快取鑰匙。

**跨平台程式碼的唯一驗證是 CI 那一格。** 開發機是 Linux，`src/audio/macos.rs`
在本機連 type check 都跑不到。這代表兩件事：那一格不能設成選配，而且光有
`cargo clippy` 不夠 —— `clippy -D warnings` 的 dead code 檢查抓到過
`platform_capture()` 根本沒接上 macOS 實作（整個模組編得過但永遠不會被呼叫），
但模組內的單元測試從來沒有執行過。arm64 runner 是原生的，那一格要真的跑測試。
