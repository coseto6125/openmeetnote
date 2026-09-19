//! 事件 sink：把送往 WebView 的批次同時轉發到外部 WebSocket 消費者。
//!
//! 這條路是旁路，不是主線。錄音、轉錄、落地與 WebView emit 的正確性完全
//! 不依賴它，因此整個模組只有一條紅線：**sink 的任何狀態都不得讓呼叫端
//! 等待或崩潰**。落實成三個結構性決定：
//!
//! 1. **呼叫端只做 `try_send`。** 有界通道滿了就丟掉最新的一筆並記數，
//!    絕不阻塞事件泵。消費者從 `prevHighSeq` 就看得出缺號，它自己知道
//!    該重新同步 —— 這比讓錄音等一個慢掉的 socket 便宜太多。
//!
//! 2. **關閉時的成本是一次 relaxed 讀。** `enabled` 為 false 時事件泵
//!    連序列化都不做，也沒有背景任務存在：任務要到第一次設定目標才建立。
//!
//! 3. **斷線期間不排空通道。** 一條死掉的連線後面積著的批次都是過期的，
//!    重連之後補送它們，等於讓消費者先套用一段舊狀態再修正。做法相反：
//!    重連時先把通道清空，接著送快照，之後送出去的第一筆就是當下的事件。
//!
//! 消費者也可以反方向送命令（開始／暫停／繼續／結束）。同一條紅線照樣成立：
//! 命令本體是同步的，而且開始錄音要開裝置與載入模型，因此它跑在
//! `spawn_blocking` 上，結果經由一條小通道回到轉發迴圈。轉發迴圈全程不等它，
//! 命令執行期間批次照送、Ping 照回。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

/// 通道容量。以 100 ms 一批算，滿載代表消費者落後超過六秒 ——
/// 那時補歷史沒有意義，重新同步才有。
const CAP: usize = 64;
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// 閒置時的 flush 週期。tungstenite 收到 Ping 會自動把 Pong 排進寫入端，
/// 但那筆要等下一次寫出才真的送走。長時間沒有事件時，這個計時器負責把它
/// 推出去，否則伺服器會判定逾時而關掉連線。
const IDLE_FLUSH: Duration = Duration::from_secs(20);
/// 命令結果的回送容量。命令是人按出來的，同時在途的不會有幾筆；這個數字
/// 只是不讓一條寫不出去的連線把結果無限堆起來。
const CMD_CAP: usize = 16;

/// 重連時要送出的完整投影（已序列化）。回傳 None 代表這次拿不到，
/// 不是錯誤：拿不到快照也還有增量可以送。
pub type Snapshot = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// 執行一筆消費者送來的命令。參數是命令名稱，`Err` 的內容會原樣回給消費者。
///
/// 與 `Snapshot` 一樣包成 closure：sink 不該知道 `SessionHandle`，命令的
/// 前置條件（哪些名字合法、什麼狀態才能暫停）也全部留在 `session` 那一側。
pub type CommandHandler = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// 呼叫端與背景任務共用的部分。
struct Shared {
    /// 有沒有設定目標。事件泵每批次讀一次，所以用最便宜的順序。
    enabled: AtomicBool,
    /// 連線已建立、通道已清空、hello 已送出。從這一刻起 `try_send` 進來的
    /// 批次都會被轉發，不會落在重連時的清空範圍裡。UI 與測試都靠它。
    connected: AtomicBool,
    dropped: AtomicU64,
    tx: mpsc::Sender<String>,
}

pub struct SinkHandle {
    shared: Arc<Shared>,
    /// 背景任務啟動時取走。還在這裡代表任務還沒建立過。
    rx: Mutex<Option<mpsc::Receiver<String>>>,
    /// 目標位址。換位址時寫進去，跑著的任務在下一個決策點看到新值。
    /// 這個 Sender 同時是關機訊號：handle 消失時任務跟著結束。
    url: watch::Sender<Option<String>>,
}

impl Default for SinkHandle {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(CAP);
        let (url, _) = watch::channel(None);
        Self {
            shared: Arc::new(Shared {
                enabled: AtomicBool::new(false),
                connected: AtomicBool::new(false),
                dropped: AtomicU64::new(0),
                tx,
            }),
            rx: Mutex::new(Some(rx)),
            url,
        }
    }
}

impl SinkHandle {
    /// 事件泵用它決定要不要多序列化一次批次。
    #[inline]
    pub fn enabled(&self) -> bool {
        self.shared.enabled.load(Ordering::Relaxed)
    }

    /// 轉發一筆已序列化的批次。這個函式不阻塞、不 panic，也不回報失敗：
    /// sink 的狀態不是呼叫端的問題。
    pub fn try_send(&self, text: String) {
        if !self.enabled() {
            return;
        }
        match self.shared.tx.try_send(text) {
            Ok(()) => {}
            // 滿了就丟最新的。丟最舊的需要額外同步，而且兩種都會產生缺號，
            // 消費者的處理方式一樣。
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            }
            // 任務已經結束（handle 正在拆除），沒有人會收。
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// 目前是否連著消費者。
    pub fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }

    /// 設定或清除轉發目標。`None`（或空字串）代表關閉。
    ///
    /// 第一次設定目標時才建立背景任務，因此「沒設定」這個狀態下沒有任何
    /// 執行中的東西。之後改目標只是換一個值，任務會自己重連。
    pub fn set_url(
        &self,
        url: Option<String>,
        snapshot: Option<Snapshot>,
        commands: Option<CommandHandler>,
    ) {
        let url = url.map(|u| u.trim().to_owned()).filter(|u| !u.is_empty());
        let on = url.is_some();
        // 先公布目標再開啟旗標：反過來的話中間那一瞬間會把批次送進一個
        // 還不知道要連去哪裡的任務。
        self.url.send_replace(url);
        self.shared.enabled.store(on, Ordering::Relaxed);
        if !on {
            return;
        }
        // 鎖損毀時不啟動任務。sink 沒跑起來只是少了轉發，
        // 而在損毀的狀態上硬開一條任務是在賭它的內容。
        let taken = self.rx.lock().ok().and_then(|mut g| g.take());
        if let Some(rx) = taken {
            let shared = Arc::clone(&self.shared);
            let url_rx = self.url.subscribe();
            tauri::async_runtime::spawn(run(rx, url_rx, shared, snapshot, commands));
        }
    }
}

fn next_backoff(cur: Duration) -> Duration {
    // 乘二再夾上限。上限存在的理由是重試本身要便宜：消費者可能整天不在。
    (cur * 2).min(BACKOFF_MAX)
}

/// 背景任務。永遠不會因為連線失敗而結束，只有 handle 消失時才結束。
async fn run(
    mut rx: mpsc::Receiver<String>,
    mut url_rx: watch::Receiver<Option<String>>,
    shared: Arc<Shared>,
    snapshot: Option<Snapshot>,
    commands: Option<CommandHandler>,
) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let target = url_rx.borrow_and_update().clone();
        let Some(url) = target else {
            // 目標被清掉了。停在這裡等下一次設定，期間不碰通道 ——
            // 呼叫端那邊 `enabled` 已經是 false，本來就不會再送東西進來。
            if url_rx.changed().await.is_err() {
                return;
            }
            backoff = BACKOFF_MIN;
            continue;
        };

        match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((ws, _)) => {
                backoff = BACKOFF_MIN;
                let keep_going = forward(
                    ws,
                    &mut rx,
                    &mut url_rx,
                    &shared,
                    snapshot.as_ref(),
                    commands.as_ref(),
                )
                .await;
                shared.connected.store(false, Ordering::Release);
                if !keep_going {
                    return;
                }
            }
            Err(_) => {
                // 錯誤內容不記：連不上是預期狀態（消費者還沒開），
                // 每 250 ms 寫一行日誌只會淹掉真正的訊息。
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {
                        backoff = next_backoff(backoff);
                    }
                    changed = url_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        backoff = BACKOFF_MIN;
                    }
                }
            }
        }
    }
}

/// 連上之後的轉發迴圈。回傳 false 代表該結束整個任務，true 代表重連。
async fn forward(
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    rx: &mut mpsc::Receiver<String>,
    url_rx: &mut watch::Receiver<Option<String>>,
    shared: &Shared,
    snapshot: Option<&Snapshot>,
    commands: Option<&CommandHandler>,
) -> bool {
    // 讀寫分開才能在同一個 select 裡同時等「有新批次」與「對端說話」。
    // 讀取端一定要有人驅動：Ping 的自動回覆是在讀的時候排進去的。
    let (mut write, mut read) = ws.split();

    // 斷線期間累積的批次在這裡丟掉。理由見模組開頭第 3 點。
    while rx.try_recv().is_ok() {}

    let hello = format!(
        r#"{{"kind":"sinkHello","droppedSinceStart":{},"app":"openmeetnote"}}"#,
        shared.dropped.load(Ordering::Relaxed)
    );
    if write.send(Message::Text(hello.into())).await.is_err() {
        return true;
    }

    // 快照在清空通道之後才取，所以它涵蓋的範圍不會落在被丟掉的批次前面。
    // 之後送出的增量可能與快照重疊一批，消費者靠 seq 去重即可 ——
    // 重疊是安全的，缺號才不是。
    if let Some(frame) = snapshot.and_then(|f| f()) {
        if write.send(Message::Text(frame.into())).await.is_err() {
            return true;
        }
    }

    // 通道已清空、hello 與快照已送出：之後進通道的每一筆都會被轉發。
    shared.connected.store(true, Ordering::Release);

    // 命令結果走這條回來。送端在 `on_text` spawn 出去的工作裡，
    // 收端在下面的 select 裡 —— 轉發迴圈因此不必等命令跑完。
    let (res_tx, mut res_rx) = mpsc::channel::<String>(CMD_CAP);
    // 壞掉的訊框每條連線只記一行：對端的 bug 會一直重送，記滿日誌之後
    // 真正的訊息就找不到了。
    let mut logged_bad_frame = false;

    let mut idle = tokio::time::interval(IDLE_FLUSH);
    idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    idle.tick().await; // interval 的第一下是立即觸發的，跳過

    loop {
        tokio::select! {
            batch = rx.recv() => match batch {
                Some(text) => {
                    if write.send(Message::Text(text.into())).await.is_err() {
                        return true;
                    }
                }
                // 所有送端都不見了：handle 被拆掉，任務該結束。
                None => return false,
            },
            result = res_rx.recv() => {
                // 送端活在這個函式裡，所以 recv 只會在真的有結果時回來。
                if let Some(text) = result {
                    if write.send(Message::Text(text.into())).await.is_err() {
                        return true;
                    }
                }
            },
            frame = read.next() => match frame {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return true,
                Some(Ok(Message::Text(text))) => {
                    on_text(text.as_str(), commands, &res_tx, &mut logged_bad_frame)
                }
                // Pong 與其他型別一律忽略；Ping 由 tungstenite 自動回覆。
                Some(Ok(_)) => {}
            },
            _ = idle.tick() => {
                // 把自動排進去的 Pong 推出去，順便偵測已經死掉的連線。
                if write.flush().await.is_err() {
                    return true;
                }
            }
            changed = url_rx.changed() => {
                if changed.is_err() {
                    return false;
                }
                // 目標換了（或被清掉），交給外層決定下一步。
                return true;
            }
        }
    }
}

/// 消費者送進來的命令。認不出來的形狀一律忽略，因此欄位全部是必要的：
/// 少一個就解析失敗，而解析失敗與「不是命令」的處理方式一樣。
#[derive(Deserialize)]
struct Inbound {
    kind: String,
    id: u64,
    name: String,
}

/// 回給消費者的結果。`ok` 為真時不帶 `error` 欄位。
#[derive(Serialize)]
struct CommandResult<'a> {
    kind: &'a str,
    id: u64,
    name: &'a str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// 處理一筆進來的文字訊框。這個函式不等命令跑完。
///
/// 命令本體是同步的：它會鎖 Session、寫資料庫，開始錄音那一筆還要開音訊
/// 裝置並載入模型，動輒好幾秒。在轉發迴圈裡等它，等於那段時間不讀 socket、
/// 不送批次，通道會塞滿而批次開始被丟掉。所以丟到 blocking 執行緒，
/// 結果從 `res_tx` 回到迴圈再送出去。
fn on_text(
    text: &str,
    commands: Option<&CommandHandler>,
    res_tx: &mpsc::Sender<String>,
    logged_bad_frame: &mut bool,
) {
    let Ok(inbound) = serde_json::from_str::<Inbound>(text) else {
        if !*logged_bad_frame {
            *logged_bad_frame = true;
            crate::stt::live::log("事件 sink 收到解析不了的訊框，已忽略（這條連線只記這一次）");
        }
        return;
    };
    if inbound.kind != "command" {
        return;
    }
    let Inbound { id, name, .. } = inbound;
    // 沒有處理器代表這個 app 沒有把命令接起來。回一張失敗的收據而不是沉默：
    // 消費者等不到回覆時分不出是沒接起來還是還在跑。
    let Some(handler) = commands.cloned() else {
        reply(
            res_tx,
            id,
            name.as_str(),
            Err("命令處理器沒有設定".to_owned()),
        );
        return;
    };
    let tx = res_tx.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // catch_unwind 是最後一道。命令本體回 `Err` 不回 panic，但真的 panic
        // 逃出去時消費者只會看到一筆永遠不回的命令 —— 它沒有別的方式知道。
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(name.as_str())))
            .unwrap_or_else(|_| Err("命令執行時發生未預期的錯誤".to_owned()));
        reply(&tx, id, name.as_str(), out);
    });
}

/// 把一筆結果排進回送通道。滿了就丟：那代表已經有 `CMD_CAP` 筆結果送不出去，
/// 這條連線的問題不是再排一筆能解決的。
fn reply(tx: &mpsc::Sender<String>, id: u64, name: &str, out: Result<(), String>) {
    let frame = CommandResult {
        kind: "commandResult",
        id,
        name,
        ok: out.is_ok(),
        error: out.err(),
    };
    if let Ok(text) = serde_json::to_string(&frame) {
        let _ = tx.try_send(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 通道滿了之後，`try_send` 立刻回來並且記下丟掉的數量。
    #[test]
    fn test_try_send_full_channel_drops_and_counts() {
        let handle = SinkHandle::default();
        // 直接開啟旗標，不設目標：這個測試要的是通道行為，不是連線行為。
        handle.shared.enabled.store(true, Ordering::Relaxed);

        for i in 0..CAP {
            handle.try_send(format!("{i}"));
        }
        assert_eq!(handle.dropped(), 0, "容量以內不該丟");

        for _ in 0..5 {
            handle.try_send("overflow".to_owned());
        }
        assert_eq!(handle.dropped(), 5, "滿了之後每一筆都要記數");

        // 通道裡留下的仍然是最早的 CAP 筆：丟的是最新的那幾筆。
        let mut rx = handle.rx.lock().unwrap().take().unwrap();
        assert_eq!(rx.try_recv().unwrap(), "0");
    }

    /// 關閉狀態下連通道都不碰。
    #[test]
    fn test_try_send_disabled_is_noop() {
        let handle = SinkHandle::default();
        for _ in 0..(CAP * 2) {
            handle.try_send("x".to_owned());
        }
        assert!(!handle.enabled());
        assert_eq!(handle.dropped(), 0);
        let mut rx = handle.rx.lock().unwrap().take().unwrap();
        assert!(rx.try_recv().is_err(), "關閉時不該有東西進通道");
    }

    /// 退避倍增到 5 秒就停住。
    #[test]
    fn test_next_backoff_caps_at_five_seconds() {
        let seq: Vec<Duration> =
            std::iter::successors(Some(BACKOFF_MIN), |d| Some(next_backoff(*d)))
                .take(8)
                .collect();
        assert_eq!(
            seq,
            vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_millis(1000),
                Duration::from_millis(2000),
                Duration::from_millis(4000),
                BACKOFF_MAX,
                BACKOFF_MAX,
                BACKOFF_MAX,
            ]
        );
    }

    /// 空字串等於關閉，不會被當成一個目標。
    #[test]
    fn test_set_url_blank_disables() {
        let handle = SinkHandle::default();
        handle.set_url(Some("   ".to_owned()), None, None);
        assert!(!handle.enabled());
        assert!(handle.rx.lock().unwrap().is_some(), "不該啟動任務");
    }

    /// 端到端：真的起一台 WebSocket 伺服器，確認 sinkHello 先到，
    /// 接著是兩筆批次，順序不變。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_sink_forwards_hello_then_batches_in_order() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut got = Vec::new();
            while got.len() < 3 {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) => got.push(t.to_string()),
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
            let _ = done_tx.send(got);
        });

        let handle = SinkHandle::default();
        handle.set_url(Some(format!("ws://{addr}")), None, None);

        // 連上之前送的批次會在連線建立時被清掉（模組開頭第 3 點），
        // 所以要等 `connected` 為真才送；那之後的每一筆都保證轉發。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !handle.connected() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "5 秒內沒連上測試伺服器"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.try_send(r#"{"n":1}"#.to_owned());
        handle.try_send(r#"{"n":2}"#.to_owned());

        let got = tokio::time::timeout(Duration::from_secs(10), done_rx)
            .await
            .expect("伺服器沒有在時限內收滿三筆")
            .unwrap();

        assert_eq!(got.len(), 3);
        assert!(
            got[0].contains(r#""kind":"sinkHello""#),
            "第一筆必須是 sinkHello，實際是 {}",
            got[0]
        );
        assert!(got[0].contains(r#""droppedSinceStart":0"#));
        assert_eq!(got[1], r#"{"n":1}"#);
        assert_eq!(got[2], r#"{"n":2}"#);
    }

    /* ── 命令 ─────────────────────────────────────────────────────── */

    /// 測試用的處理器：記下收到的名字，並照 `session::sink_command_handler`
    /// 的規則回答 —— 認得的名字成功，其餘回 `unknown command`。
    fn test_handler(seen: Arc<Mutex<Vec<String>>>) -> CommandHandler {
        Arc::new(move |name: &str| -> Result<(), String> {
            seen.lock().unwrap().push(name.to_owned());
            match name {
                "startMeeting" | "pauseMeeting" | "resumeMeeting" | "endMeeting" => Ok(()),
                _ => Err("unknown command".to_owned()),
            }
        })
    }

    /// 起一台測試伺服器：接受連線、讀掉 sinkHello、照腳本送出訊框，
    /// 再收集 `want` 筆 commandResult。
    async fn command_server(
        script: Vec<Message>,
        want: usize,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::oneshot::Receiver<Vec<String>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await; // sinkHello
            for frame in script {
                if ws.send(frame).await.is_err() {
                    break;
                }
            }
            let mut got = Vec::new();
            while got.len() < want {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) if t.contains("commandResult") => {
                        got.push(t.to_string())
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
            let _ = done_tx.send(got);
        });
        (addr, done_rx)
    }

    async fn collected(rx: tokio::sync::oneshot::Receiver<Vec<String>>) -> Vec<String> {
        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("伺服器沒有在時限內收滿 commandResult")
            .unwrap()
    }

    /// 一筆 command 進來，處理器收到名字，消費者收到 ok:true。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_sink_command_runs_handler_and_replies_ok() {
        let (addr, done_rx) = command_server(
            vec![Message::Text(
                r#"{"kind":"command","id":7,"name":"pauseMeeting"}"#.into(),
            )],
            1,
        )
        .await;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let handle = SinkHandle::default();
        handle.set_url(
            Some(format!("ws://{addr}")),
            None,
            Some(test_handler(Arc::clone(&seen))),
        );

        let got = collected(done_rx).await;
        assert_eq!(got.len(), 1);
        assert!(
            got[0].contains(r#""kind":"commandResult""#),
            "實際是 {}",
            got[0]
        );
        assert!(got[0].contains(r#""id":7"#), "實際是 {}", got[0]);
        assert!(
            got[0].contains(r#""name":"pauseMeeting""#),
            "實際是 {}",
            got[0]
        );
        assert!(got[0].contains(r#""ok":true"#), "實際是 {}", got[0]);
        assert!(
            !got[0].contains("error"),
            "成功的收據不該帶 error：{}",
            got[0]
        );
        assert_eq!(*seen.lock().unwrap(), vec!["pauseMeeting".to_owned()]);
    }

    /// 不認得的名字回 ok:false，錯誤訊息原樣送回去。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_sink_unknown_command_replies_error() {
        let (addr, done_rx) = command_server(
            vec![Message::Text(
                r#"{"kind":"command","id":9,"name":"selfDestruct"}"#.into(),
            )],
            1,
        )
        .await;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let handle = SinkHandle::default();
        handle.set_url(
            Some(format!("ws://{addr}")),
            None,
            Some(test_handler(Arc::clone(&seen))),
        );

        let got = collected(done_rx).await;
        assert_eq!(got.len(), 1);
        assert!(got[0].contains(r#""id":9"#), "實際是 {}", got[0]);
        assert!(
            got[0].contains(r#""name":"selfDestruct""#),
            "實際是 {}",
            got[0]
        );
        assert!(got[0].contains(r#""ok":false"#), "實際是 {}", got[0]);
        assert!(
            got[0].contains(r#""error":"unknown command""#),
            "實際是 {}",
            got[0]
        );
    }

    /// 壞掉的訊框只是被忽略：連線還在，下一筆命令照樣拿得到收據。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_sink_malformed_frame_keeps_connection() {
        let (addr, done_rx) = command_server(
            vec![
                Message::Text("這不是 JSON".into()),
                Message::Text(r#"{"kind":"command","id":2,"name":"endMeeting"}"#.into()),
            ],
            1,
        )
        .await;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let handle = SinkHandle::default();
        handle.set_url(
            Some(format!("ws://{addr}")),
            None,
            Some(test_handler(Arc::clone(&seen))),
        );

        let got = collected(done_rx).await;
        assert_eq!(got.len(), 1, "壞訊框之後的命令必須還有收據");
        assert!(got[0].contains(r#""id":2"#), "實際是 {}", got[0]);
        assert!(got[0].contains(r#""ok":true"#), "實際是 {}", got[0]);
        // 壞訊框沒有進到處理器
        assert_eq!(*seen.lock().unwrap(), vec!["endMeeting".to_owned()]);
    }
}
