//! Provider 設定與密鑰（BLUEPRINT.md §5.6、§14）。
//!
//! 優先順序固定為：作業系統環境變數 > GUI 選擇 > OS 憑證庫的密鑰。
//! 呼叫端拿到的是已解析的 `ResolvedProvider`，不需要理解這三層。
//!
//! 兩條紅線寫在型別上，不只寫在註解裡：
//!
//! 1. **密鑰不進 SQLite、設定檔、逐字稿、日誌或錯誤回報。** `ResolvedProvider`
//!    根本沒有存密鑰的欄位，只有 `secret: SecretPresence`。要拿真正的值必須
//!    另外呼叫 `secret_value`，那個回傳型別不實作 `Debug` 也不實作 `Serialize`，
//!    因此不會意外被 `{:?}` 或 JSON 序列化帶出去。
//! 2. **環境變數贏過 GUI。** 企業環境用環境變數統一派送設定，GUI 改動不該
//!    悄悄蓋掉它。UI 因此需要知道某個欄位是否被環境變數鎖住，
//!    `ResolvedField` 帶著來源。

use std::fmt;

use serde::Serialize;

// 這兩個型別 store 也要用。放在 model 而不是這裡，
// 否則 store 與 config 會互相引用。
pub use crate::model::{ProviderKind, StoredProvider};

/// 某個欄位的最終值與它的來源。
///
/// UI 需要來源才能正確呈現：被環境變數鎖住的欄位改了也沒用，
/// 讓使用者對著一個無效的輸入框調整是更糟的體驗。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedField {
    pub value: String,
    pub source: FieldSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FieldSource {
    /// 由環境變數決定，GUI 不可覆寫
    Environment,
    /// 由 GUI 設定
    Settings,
    /// 兩者皆無，用內建預設
    Default,
}

/// 密鑰的存在狀態。這是 UI 能知道的全部，值本身不往外送。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SecretPresence {
    /// 由環境變數提供
    Environment,
    /// 存在 OS 憑證庫
    Keychain,
    /// 尚未設定
    Missing,
    /// 憑證庫無法存取，因此無法判斷。與 Missing 分開：
    /// 前者要使用者去設定，後者要使用者去修環境。
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedProvider {
    pub kind: ProviderKind,
    pub provider: ResolvedField,
    pub model: ResolvedField,
    pub base_url: ResolvedField,
    pub secret: SecretPresence,
}

/// 密鑰的值。
///
/// 刻意不實作 `Debug`、`Display` 與 `Serialize`：這三個 trait 是密鑰外洩的
/// 主要途徑，少了它們，`{:?}`、`format!` 與 JSON 序列化都會編譯失敗，
/// 而不是安靜地把密鑰寫進日誌。
// 密鑰的取值路徑目前只有測試在走：真實 Provider Adapter 是 M5 到 M6 的工作。
// 契約先立在這裡，因為它決定了密鑰能不能被序列化出去，而那是 §14 的紅線。
#[allow(dead_code)]
pub struct Secret(String);

impl Secret {
    pub fn new(v: String) -> Self {
        Self(v)
    }

    /// 唯一的取值出口。呼叫點刻意顯眼，方便稽核。
    #[allow(dead_code)]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 就算有人硬加了 Debug 呼叫，輸出也不含密鑰
        f.write_str("Secret(<redacted>)")
    }
}

/* ── 憑證庫接縫 ─────────────────────────────────────────────────── */

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    // resolve 會產生它，OsSecretStore 在非目標平台不會
    #[allow(dead_code)]
    #[error("找不到這個密鑰")]
    NotFound,
    #[error("憑證庫無法存取：{0}")]
    Unavailable(String),
}

/// OS 憑證庫的抽象。測試用記憶體實作，正式環境用平台原生後端。
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Secret, SecretError>;
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
const SERVICE: &str = "OpenMeetNote";

/// Windows Credential Manager 與 macOS Keychain。
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub struct OsSecretStore;

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl SecretStore for OsSecretStore {
    fn get(&self, key: &str) -> Result<Secret, SecretError> {
        let entry = keyring::Entry::new(SERVICE, key)
            .map_err(|e| SecretError::Unavailable(e.to_string()))?;
        match entry.get_password() {
            Ok(v) => Ok(Secret::new(v)),
            Err(keyring::Error::NoEntry) => Err(SecretError::NotFound),
            Err(e) => Err(SecretError::Unavailable(e.to_string())),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        keyring::Entry::new(SERVICE, key)
            .and_then(|e| e.set_password(value))
            .map_err(|e| SecretError::Unavailable(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let entry = keyring::Entry::new(SERVICE, key)
            .map_err(|e| SecretError::Unavailable(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Unavailable(e.to_string())),
        }
    }
}

/// 目標平台以外的佔位實作。
///
/// 明確回報「這個平台沒有憑證庫」而不是靜默失敗，也絕不退回檔案儲存：
/// 把密鑰寫進檔案正是 §14 禁止的事，而「開發方便」不是放行的理由。
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub struct OsSecretStore;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
impl SecretStore for OsSecretStore {
    fn get(&self, _key: &str) -> Result<Secret, SecretError> {
        Err(SecretError::Unavailable(
            "此平台沒有支援的憑證庫，請改用環境變數提供密鑰".into(),
        ))
    }
    fn set(&self, _key: &str, _value: &str) -> Result<(), SecretError> {
        Err(SecretError::Unavailable(
            "此平台沒有支援的憑證庫，請改用環境變數提供密鑰".into(),
        ))
    }
    fn delete(&self, _key: &str) -> Result<(), SecretError> {
        Ok(())
    }
}

/* ── 解析 ───────────────────────────────────────────────────────── */

/// 環境變數的讀取來源。測試注入假的 map，正式環境讀真的 env。
pub trait Env {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct SystemEnv;

impl Env for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        // 空字串視同未設定：CI 常把未填的變數送成空值，
        // 讓它蓋掉 GUI 設定只會製造「明明設了卻沒生效」的問題
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }
}

fn field(env: &dyn Env, env_key: &str, stored: &str, fallback: &str) -> ResolvedField {
    match env.get(env_key) {
        Some(value) => ResolvedField {
            value,
            source: FieldSource::Environment,
        },
        None if !stored.is_empty() => ResolvedField {
            value: stored.to_owned(),
            source: FieldSource::Settings,
        },
        None => ResolvedField {
            value: fallback.to_owned(),
            source: FieldSource::Default,
        },
    }
}

/// 密鑰在憑證庫裡的鍵名，例如 `stt-api-key`。
pub fn secret_key(kind: ProviderKind) -> String {
    format!("{}-api-key", kind.as_str())
}

fn secret_env_key(kind: ProviderKind) -> String {
    format!("{}_API_KEY", kind.env_prefix())
}

/// 依 §5.6 的優先順序解析出最終設定。
/// 沒有任何設定時的預設 Provider 由呼叫端給：llm 的預設要看這台機器上
/// 有沒有 Agent CLI，那是執行期才知道的事，寫死在這裡就無法決定性地測試。
pub fn resolve_with_default(
    kind: ProviderKind,
    stored: &StoredProvider,
    env: &dyn Env,
    secrets: &dyn SecretStore,
    default_provider: &str,
) -> ResolvedProvider {
    let p = kind.env_prefix();
    ResolvedProvider {
        kind,
        provider: field(
            env,
            &format!("{p}_PROVIDER"),
            &stored.provider,
            default_provider,
        ),
        model: field(env, &format!("{p}_MODEL"), &stored.model, ""),
        base_url: field(env, &format!("{p}_BASE_URL"), &stored.base_url, ""),
        secret: if env.get(&secret_env_key(kind)).is_some() {
            SecretPresence::Environment
        } else {
            match secrets.get(&secret_key(kind)) {
                Ok(_) => SecretPresence::Keychain,
                Err(SecretError::NotFound) => SecretPresence::Missing,
                Err(SecretError::Unavailable(_)) => SecretPresence::Unavailable,
            }
        },
    }
}

/// 取出實際的密鑰值。環境變數優先，其次憑證庫。
///
/// 目前只有測試呼叫。真實 Adapter 接上時走的就是這一條，
/// 而它是唯一能取到密鑰的出口。
#[allow(dead_code)]
///
/// 與 `resolve` 分開，因為絕大多數呼叫端只需要知道密鑰在不在。
/// 真正要用的地方只有送出 Provider 請求的那一處。
pub fn secret_value(
    kind: ProviderKind,
    env: &dyn Env,
    secrets: &dyn SecretStore,
) -> Result<Secret, SecretError> {
    match env.get(&secret_env_key(kind)) {
        Some(v) => Ok(Secret::new(v)),
        None => secrets.get(&secret_key(kind)),
    }
}

/* ── 摘要與 Agent 後端目錄（BLUEPRINT.md §5.5.1） ───────────────── */

/// 後端的取得方式。UI 用它決定要不要顯示模型、Base URL 與 API Key 欄位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BackendKind {
    /// 跟隨這台機器上偵測到的 Agent CLI，不綁定特定一支
    System,
    /// 指定某一支本機 Agent CLI
    AgentCli,
    /// 直接呼叫 LLM API，需要金鑰
    Api,
    /// 不呼叫任何 Provider 的內建規劃器
    Fixture,
}

/// 一個可選後端，連同它在這台機器上是否真的能用。
///
/// `available` 為 false 的項目仍然回傳，因為「沒安裝」與「不存在這個選項」
/// 對使用者要做的事不同：前者去安裝，後者改選別的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendOption {
    pub id: String,
    pub label: String,
    pub kind: BackendKind,
    pub available: bool,
    /// 版本字串，或不可用的原因。UI 直接顯示。
    pub detail: String,
    pub needs_secret: bool,
}

/// 本機 Agent CLI 的候選。id 同時是存進 `provider_settings` 的值。
const AGENT_CLIS: [(&str, &str, &str); 2] = [
    ("claude-code", "Claude Code", "claude"),
    ("codex", "Codex CLI", "codex"),
];

/// 在 PATH 上找可執行檔。
///
/// Windows 的 npm 安裝出來的是 `claude.cmd`，直接 `Command::new("claude")`
/// 找不到它，因此這裡自己走一次 PATH 與 PATHEXT。
pub fn resolve_exe(name: &str) -> Option<std::path::PathBuf> {
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };

    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| {
        exts.iter().find_map(|ext| {
            let p = dir.join(format!("{name}{ext}"));
            p.is_file().then_some(p)
        })
    })
}

/// 跑一次 `--version`。有輸出才算真的可用：檔案存在但跑不起來
/// （缺 node、權限不足）與沒安裝要分開講。
fn probe_version(exe: &std::path::Path) -> Result<String, String> {
    let out = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .map_err(|e| format!("無法執行：{e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "回報失敗：{}",
            err.lines().next().unwrap_or("未提供原因").trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().next().unwrap_or_default().trim().to_owned())
}

/// 列出摘要與 Agent Provider 可選的後端。
///
/// 登入狀態不在這裡驗證：那需要真的送出一次請求，會產生費用與延遲。
/// 這一層只回答「這支 CLI 裝了沒、跑不跑得起來」，登入失敗在首次生成時
/// 以 Provider 錯誤回報。
pub fn llm_backends() -> Vec<BackendOption> {
    let cli: Vec<BackendOption> = AGENT_CLIS
        .iter()
        .map(|(id, label, exe)| {
            let (available, detail) = match resolve_exe(exe) {
                None => (false, format!("PATH 上找不到 {exe}，尚未安裝")),
                Some(path) => match probe_version(&path) {
                    Ok(v) if v.is_empty() => (true, path.display().to_string()),
                    Ok(v) => (true, v),
                    Err(e) => (false, e),
                },
            };
            BackendOption {
                id: (*id).to_owned(),
                label: (*label).to_owned(),
                kind: BackendKind::AgentCli,
                available,
                detail,
                needs_secret: false,
            }
        })
        .collect();

    let ready: Vec<&str> = cli
        .iter()
        .filter(|o| o.available)
        .map(|o| o.label.as_str())
        .collect();

    let system = BackendOption {
        id: "system".to_owned(),
        label: "系統 LLM 配置".to_owned(),
        kind: BackendKind::System,
        available: !ready.is_empty(),
        detail: if ready.is_empty() {
            "這台機器沒有偵測到 Claude Code 或 Codex CLI".to_owned()
        } else {
            format!("使用已安裝的 {}", ready.join("、"))
        },
        needs_secret: false,
    };

    let mut out = vec![system];
    out.extend(cli);
    out.push(BackendOption {
        id: "api".to_owned(),
        label: "LLM API（自備金鑰）".to_owned(),
        kind: BackendKind::Api,
        available: true,
        detail: "填入模型、Base URL 與 API Key".to_owned(),
        needs_secret: true,
    });
    out.push(BackendOption {
        id: "fixture".to_owned(),
        label: "內建測試規劃器".to_owned(),
        kind: BackendKind::Fixture,
        available: true,
        detail: "不呼叫任何 Provider，用於離線驗證流程".to_owned(),
        needs_secret: false,
    });
    out
}

/// llm 的預設選項。偵測不到任何 CLI 時退到 fixture，不預設 api：
/// 預設一個必須填金鑰才會動的選項，等於一開啟就是壞的。
fn default_llm_provider(backends: &[BackendOption]) -> &'static str {
    let usable = backends
        .iter()
        .any(|o| o.kind == BackendKind::System && o.available);
    if usable {
        "system"
    } else {
        "fixture"
    }
}

/* ── 命令 ─────────────────────────────────────────────────────────── */

use tauri::State;

/// 憑證庫與環境變數的共用把手。
///
/// 兩者都是 trait object，因此整條設定路徑可以在測試裡完整驗證，
/// 不需要真的去動使用者的 Keychain。
pub struct ConfigHandle {
    pub secrets: Box<dyn SecretStore>,
    pub env: Box<dyn Env + Send + Sync>,
}

impl Default for ConfigHandle {
    fn default() -> Self {
        Self {
            secrets: Box::new(OsSecretStore),
            env: Box::new(SystemEnv),
        }
    }
}

#[tauri::command]
pub fn get_settings(
    store: State<crate::store::StoreHandle>,
    config: State<ConfigHandle>,
) -> Result<Vec<ResolvedProvider>, String> {
    let st = store.exclusive().map_err(|e| e.to_string())?;
    let backends = llm_backends();
    [ProviderKind::Stt, ProviderKind::Llm]
        .into_iter()
        .map(|kind| {
            let stored = st.provider_settings(kind).map_err(|e| e.to_string())?;
            let fallback = match kind {
                ProviderKind::Stt => "fixture",
                ProviderKind::Llm => default_llm_provider(&backends),
            };
            Ok(resolve_with_default(
                kind,
                &stored,
                config.env.as_ref(),
                config.secrets.as_ref(),
                fallback,
            ))
        })
        .collect()
}

/// 這台機器上摘要與 Agent Provider 可選的後端。
///
/// 每次開啟設定頁重掃，不快取：使用者可能剛裝好 CLI 就切回來看，
/// 顯示一份過期的清單比多花兩百毫秒更糟。
#[tauri::command]
pub async fn list_llm_backends() -> Result<Vec<BackendOption>, String> {
    // 偵測會啟動子行程，放到 blocking pool，不讓 UI 執行緒等它
    tauri::async_runtime::spawn_blocking(llm_backends)
        .await
        .map_err(|e| format!("偵測失敗：{e}"))
}

#[tauri::command]
pub fn save_provider(
    store: State<crate::store::StoreHandle>,
    kind: String,
    provider: String,
    model: String,
    base_url: String,
) -> Result<(), String> {
    let kind = ProviderKind::parse(&kind).ok_or("未知的 Provider 類別")?;
    let mut st = store.exclusive().map_err(|e| e.to_string())?;
    st.set_provider_settings(
        kind,
        &StoredProvider {
            provider: provider.trim().to_owned(),
            model: model.trim().to_owned(),
            base_url: base_url.trim().to_owned(),
            options: String::new(),
        },
    )
    .map_err(|e| e.to_string())
}

/// 把密鑰寫進 OS 憑證庫。值不經過 SQLite，也不寫進日誌。
#[tauri::command]
pub fn save_secret(config: State<ConfigHandle>, kind: String, value: String) -> Result<(), String> {
    let kind = ProviderKind::parse(&kind).ok_or("未知的 Provider 類別")?;
    if value.trim().is_empty() {
        return Err("密鑰不得為空".into());
    }
    config
        .secrets
        .set(&secret_key(kind), value.trim())
        // 錯誤訊息只帶原因，不帶值
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn clear_secret(config: State<ConfigHandle>, kind: String) -> Result<(), String> {
    let kind = ProviderKind::parse(&kind).ok_or("未知的 Provider 類別")?;
    config
        .secrets
        .delete(&secret_key(kind))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
pub mod testing {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    pub struct FakeEnv(pub HashMap<String, String>);

    impl FakeEnv {
        pub fn with(pairs: &[(&str, &str)]) -> Self {
            Self(
                pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            )
        }
    }

    impl Env for FakeEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned().filter(|v| !v.trim().is_empty())
        }
    }

    #[derive(Default)]
    pub struct MemorySecrets(pub Mutex<HashMap<String, String>>);

    impl SecretStore for MemorySecrets {
        fn get(&self, key: &str) -> Result<Secret, SecretError> {
            self.0
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .map(Secret::new)
                .ok_or(SecretError::NotFound)
        }
        fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_owned(), value.to_owned());
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<(), SecretError> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    /// 憑證庫壞掉的情境，例如 Linux 沒有 secret service。
    pub struct BrokenSecrets;

    impl SecretStore for BrokenSecrets {
        fn get(&self, _: &str) -> Result<Secret, SecretError> {
            Err(SecretError::Unavailable("沒有可用的憑證庫".into()))
        }
        fn set(&self, _: &str, _: &str) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("沒有可用的憑證庫".into()))
        }
        fn delete(&self, _: &str) -> Result<(), SecretError> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn stored() -> StoredProvider {
        StoredProvider {
            provider: "openai".into(),
            model: "gui-model".into(),
            base_url: "https://gui.example".into(),
            options: String::new(),
        }
    }

    /// 這些案例驗證的是三層優先順序，與執行期的 CLI 偵測無關，
    /// 因此固定用 fixture 當預設。
    fn resolve(
        kind: ProviderKind,
        stored: &StoredProvider,
        env: &dyn Env,
        secrets: &dyn SecretStore,
    ) -> ResolvedProvider {
        resolve_with_default(kind, stored, env, secrets, "fixture")
    }

    #[test]
    fn environment_wins_over_the_gui_setting() {
        let env = FakeEnv::with(&[("OPENMEETNOTE_LLM_MODEL", "env-model")]);
        let r = resolve(
            ProviderKind::Llm,
            &stored(),
            &env,
            &MemorySecrets::default(),
        );
        assert_eq!(r.model.value, "env-model");
        assert_eq!(r.model.source, FieldSource::Environment);
        // 沒被環境變數指定的欄位仍然由 GUI 決定
        assert_eq!(r.provider.value, "openai");
        assert_eq!(r.provider.source, FieldSource::Settings);
    }

    #[test]
    fn an_empty_environment_variable_does_not_override_the_gui() {
        let env = FakeEnv::with(&[("OPENMEETNOTE_LLM_MODEL", "   ")]);
        let r = resolve(
            ProviderKind::Llm,
            &stored(),
            &env,
            &MemorySecrets::default(),
        );
        assert_eq!(r.model.value, "gui-model", "空值把設定蓋掉了");
    }

    #[test]
    fn unset_fields_fall_back_to_the_built_in_default() {
        let r = resolve(
            ProviderKind::Stt,
            &StoredProvider::default(),
            &FakeEnv::default(),
            &MemorySecrets::default(),
        );
        assert_eq!(r.provider.value, "fixture");
        assert_eq!(r.provider.source, FieldSource::Default);
    }

    #[test]
    fn secret_presence_distinguishes_missing_from_unavailable() {
        let env = FakeEnv::default();
        let missing = resolve(
            ProviderKind::Llm,
            &stored(),
            &env,
            &MemorySecrets::default(),
        );
        assert_eq!(missing.secret, SecretPresence::Missing);

        // 憑證庫壞掉不等於使用者沒設密鑰，兩者要做的事不一樣
        let broken = resolve(ProviderKind::Llm, &stored(), &env, &BrokenSecrets);
        assert_eq!(broken.secret, SecretPresence::Unavailable);
    }

    #[test]
    fn a_keychain_secret_is_reported_as_present_but_never_returned() {
        let secrets = MemorySecrets::default();
        secrets
            .set(&secret_key(ProviderKind::Llm), "sk-real-key")
            .unwrap();
        let r = resolve(ProviderKind::Llm, &stored(), &FakeEnv::default(), &secrets);
        assert_eq!(r.secret, SecretPresence::Keychain);
        // 整個要送給 UI 的結構序列化之後不得出現密鑰
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("sk-real-key"), "密鑰被送到 UI 了");
    }

    #[test]
    fn secret_value_prefers_the_environment_over_the_keychain() {
        let secrets = MemorySecrets::default();
        secrets
            .set(&secret_key(ProviderKind::Stt), "from-keychain")
            .unwrap();
        let env = FakeEnv::with(&[("OPENMEETNOTE_STT_API_KEY", "from-env")]);
        let v = secret_value(ProviderKind::Stt, &env, &secrets).unwrap();
        assert_eq!(v.expose(), "from-env");
    }

    #[test]
    fn debug_formatting_a_secret_does_not_leak_it() {
        let s = Secret::new("sk-should-not-appear".into());
        assert!(!format!("{s:?}").contains("sk-should-not-appear"));
    }

    /* ── 後端目錄 ────────────────────────────────────────────────── */

    fn backend(id: &str, kind: BackendKind, available: bool) -> BackendOption {
        BackendOption {
            id: id.into(),
            label: id.into(),
            kind,
            available,
            detail: String::new(),
            needs_secret: kind == BackendKind::Api,
        }
    }

    #[test]
    fn the_default_backend_is_system_when_an_agent_cli_is_present() {
        let backends = [
            backend("system", BackendKind::System, true),
            backend("api", BackendKind::Api, true),
        ];
        assert_eq!(default_llm_provider(&backends), "system");
    }

    #[test]
    fn the_default_backend_falls_back_to_fixture_not_api() {
        // 預設成 api 等於一開啟就是壞的：沒有金鑰，第一次生成就失敗
        let backends = [
            backend("system", BackendKind::System, false),
            backend("api", BackendKind::Api, true),
        ];
        assert_eq!(default_llm_provider(&backends), "fixture");
    }

    #[test]
    fn the_catalogue_always_offers_api_and_both_agent_clis() {
        // 沒安裝的 CLI 仍要出現在清單裡：「沒安裝」與「沒有這個選項」
        // 要使用者做的事不一樣
        let ids: Vec<String> = llm_backends().into_iter().map(|o| o.id).collect();
        assert_eq!(ids, ["system", "claude-code", "codex", "api", "fixture"]);
    }

    #[test]
    fn an_unavailable_backend_still_explains_why() {
        for o in llm_backends() {
            assert!(
                o.available || !o.detail.trim().is_empty(),
                "{} 不可用卻沒有說明原因",
                o.id
            );
        }
    }

    #[test]
    fn a_missing_executable_is_reported_as_absent_not_as_an_error() {
        assert!(resolve_exe("openmeetnote-no-such-binary").is_none());
    }
}
