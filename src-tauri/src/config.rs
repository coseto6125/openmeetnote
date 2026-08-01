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

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Stt,
    Llm,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Stt => "stt",
            ProviderKind::Llm => "llm",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stt" => Some(ProviderKind::Stt),
            "llm" => Some(ProviderKind::Llm),
            _ => None,
        }
    }

    /// 環境變數前綴，例如 `OPENMEETNOTE_STT_PROVIDER`。
    fn env_prefix(self) -> &'static str {
        match self {
            ProviderKind::Stt => "OPENMEETNOTE_STT",
            ProviderKind::Llm => "OPENMEETNOTE_LLM",
        }
    }
}

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

/// GUI 存下來的非敏感設定，對應 `provider_settings` 資料表。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredProvider {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    #[serde(default)]
    pub options: String,
}

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
pub fn resolve(
    kind: ProviderKind,
    stored: &StoredProvider,
    env: &dyn Env,
    secrets: &dyn SecretStore,
) -> ResolvedProvider {
    let p = kind.env_prefix();
    let default_provider = match kind {
        ProviderKind::Stt => "fixture",
        ProviderKind::Llm => "fixture",
    };
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
    store: State<crate::session::StoreHandle>,
    config: State<ConfigHandle>,
) -> Result<Vec<ResolvedProvider>, String> {
    let st = store
        .0
        .lock()
        .map_err(|_| "資料庫連線狀態已損毀".to_owned())?;
    [ProviderKind::Stt, ProviderKind::Llm]
        .into_iter()
        .map(|kind| {
            let stored = st.provider_settings(kind).map_err(|e| e.to_string())?;
            Ok(resolve(
                kind,
                &stored,
                config.env.as_ref(),
                config.secrets.as_ref(),
            ))
        })
        .collect()
}

#[tauri::command]
pub fn save_provider(
    store: State<crate::session::StoreHandle>,
    kind: String,
    provider: String,
    model: String,
    base_url: String,
) -> Result<(), String> {
    let kind = ProviderKind::parse(&kind).ok_or("未知的 Provider 類別")?;
    let mut st = store
        .0
        .lock()
        .map_err(|_| "資料庫連線狀態已損毀".to_owned())?;
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
}
