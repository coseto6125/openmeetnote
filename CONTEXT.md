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
- **引用驗證(citation verification)**:程式執行的三項檢查,不交給模型自評。身分與版本在快照範圍內、locator 是非空區間且在範圍內、引文正規化後是 **locator 指到那一段** 的子字串且雜湊相符。
  - 引文要有實質內容(至少兩個非標點字元)。`"。"` 在任何一段中文逐字稿裡都找得到,它證明的是「這段話有標點」。
  - locator 必須框住引文。只拿它查範圍、卻對整段內容比對子字串的話,locator 就是一個沒有人查的數字,位置與引文互相矛盾也照樣通過。
  - 雜湊是紀錄的封緘,不是對模型的檢查:值由程式自己補,擋的是事後竄改。
  - 界限:逐字比對只證明引用存在於證據的那個位置,不證明區塊的論述被它支持。UI 不得讓引用標記看起來等於「已驗證為真」。
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

**列數契約擋不住「動到了別人那一列」。** 第一層防線問的是「有沒有動到一列」,
不是「有沒有動到對的那一列」。`document_id` 與 `run_id` 曾經有兩個配發者
（Session 快取 `MAX(id)+1`、歷史頁每次重讀）,撞號之後 `GenerationCompleted`
更新到另一場會議的版本上,而 `expect_touched` 因為確實動到一列而放行。
全域 id 只能有一個配發者,而且遞增與讀取要在同一個 statement 裡
（`id_sequences`,見 migration 002）—— 配發與寫入之間隔多久都不能被別人拿走。

**「回報成功」與「真的開始了」是兩件事。** 只要 `start()` 在真正的初始化之前
就回傳,失敗就會變成「畫面說在錄、實際上什麼都沒有」。踩過三個:
`AudioCapture::start` 只 spawn 執行緒就回 `Ok`（權限被拒在執行緒裡發生）、
STT 引擎在工作執行緒裡才載入（模型壞掉只留一行 log）、`start_meeting` 的四個
步驟只有最後一步在鎖裡（雙擊會換掉正在錄音的來源）。
一律的做法是**握手**:背景執行緒先回報初始化結果,`start` 等到全部回報才回傳。

**就地 UPDATE 是投影可重建性的漏洞。** 「所有投影都能從日誌重建」只要有一個
欄位是直接 UPDATE 的就不成立,而它會安靜地存在很久 —— 直到有人真的跑
`rebuild_projections`,那個欄位就此消失。踩過兩個:會議改名與 abandoned 會議的
`failed` 狀態。相對的,`meetings` 那一列的建立本身不是事件（事件要靠外鍵指向
它),所以建立當下給定的值屬於那一步,不屬於日誌。

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

**建置產物快取與 build script 的產出不同步。** `sherpa-rs-sys` 的 build script
把預編的函式庫下載到 `dirs::cache_dir()/sherpa-rs`（Linux 是 `~/.cache`、
macOS 是 `~/Library/Caches`、Windows 是 `%LOCALAPPDATA%`），那不在 `target/`
底下。快取命中時 build script 被判定為不需重跑，它的連結旗標照樣送出，但東西
不在了。

這件事以三種面貌出現過，一次比一次晚才炸：

1. Linux 連結失敗 `unable to find library -lonnxruntime`。
2. Windows 連結失敗 `LNK1181: cannot open input file cargs.lib` —— 同一件事，
   只是快取目錄在另一個位置，第一次只補了 Linux 那個。
3. Linux 連結成功但執行時 `libsherpa-onnx-c-api.so: cannot open shared object
   file`。三個目錄都列進快取之後仍然發生，這時已經不值得再猜第四種形狀。

結論不是「把快取路徑列完」，而是**會執行程式碼的那一格不要掛快取**。綠燈的
意義不該取決於快取剛好是對的。只做編譯檢查的格子（clippy 不執行任何二進位）
留著快取沒問題，代價與風險在那裡是相稱的。

**跨平台程式碼的唯一驗證是 CI 那一格。** 開發機是 Linux，`src/audio/macos.rs`
在本機連 type check 都跑不到。這代表兩件事：那一格不能設成選配，而且光有
`cargo clippy` 不夠 —— `clippy -D warnings` 的 dead code 檢查抓到過
`platform_capture()` 根本沒接上 macOS 實作（整個模組編得過但永遠不會被呼叫），
但模組內的單元測試從來沒有執行過。arm64 runner 是原生的，那一格要真的跑測試。
