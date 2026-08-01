//! SQLite 連線與 schema migration。
//!
//! 三個決定寫在這裡，因為它們影響所有上層模組：
//!
//! 1. **前向 migration，不做 down。** 桌面應用的降版路徑是還原備份，不是
//!    反向 SQL。寫 down migration 只會製造「跑得過但資料已經錯了」的假象。
//!    比實際版本新的資料庫直接拒絕開啟，而不是嘗試相容。
//! 2. **每個 migration 在自己的交易內。** 中途失敗時已套用的版本保持一致，
//!    重跑從失敗那一版接續。
//! 3. **`foreign_keys` 與 `WAL` 在連線層開啟。** SQLite 預設不檢查外鍵，
//!    這件事必須每條連線各自宣告，寫在 schema 裡沒有用。

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// 目前的 schema 版本。新增 migration 時同步 +1。
pub const SCHEMA_VERSION: i64 = 1;

/// 等鎖的時限。生成與錄音在不同連線上競爭，瞬間撞上時預設行為是立刻回
/// SQLITE_BUSY，而等一下就好的事不該變成錯誤。讀寫兩條連線用同一個值。
const BUSY_TIMEOUT_MS: i64 = 5_000;

/// migration 以 `(version, sql)` 排序套用。版本必須連續。
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("migrations/001_initial.sql"))];

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// 資料庫比程式新。相容它就是猜未知欄位的語意，因此拒絕開啟。
    #[error("資料庫 schema 版本 {found} 高於本程式支援的 {supported}，請升級應用程式")]
    TooNew { found: i64, supported: i64 },
}

pub type Result<T> = std::result::Result<T, DbError>;

/// 開啟（必要時建立）資料庫並套用所有未套用的 migration。
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    prepare(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// 記憶體資料庫，供測試使用。走的是同一條 migration 路徑。
#[cfg(test)]
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    prepare(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// 唯讀連線，供背景工作在不阻塞寫入的情況下讀取證據。
///
/// 不跑 migration：schema 由持有寫入連線的那一方負責，讀取者只是跟上。
/// 開成唯讀也是刻意的，讓「背景工作不寫資料庫」變成連線層的事實而不是約定。
pub fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)?;
    Ok(conn)
}

fn prepare(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version    INTEGER PRIMARY KEY,
             applied_at TEXT NOT NULL
         )",
    )?;

    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )?;

    if current > SCHEMA_VERSION {
        return Err(DbError::TooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }

    for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
        // 一版一交易：中途失敗時已套用的版本仍然一致，重跑從這裡接續
        conn.execute_batch("BEGIN")?;
        let applied = conn.execute_batch(sql).and_then(|()| {
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                rusqlite::params![version, crate::clock::now_utc()],
            )
            .map(|_| ())
        });
        match applied {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(e) => {
                conn.execute_batch("ROLLBACK")?;
                return Err(e.into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_brings_a_fresh_database_to_the_current_version() {
        let conn = open_in_memory().unwrap();
        let v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn migrate_is_idempotent_when_run_twice() {
        let conn = open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, MIGRATIONS.len() as i64);
    }

    #[test]
    fn open_refuses_a_database_newer_than_this_build() {
        let conn = open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, 'x')",
            [SCHEMA_VERSION + 1],
        )
        .unwrap();
        assert!(matches!(migrate(&conn), Err(DbError::TooNew { .. })));
    }

    #[test]
    fn foreign_keys_are_enforced_on_every_connection() {
        let conn = open_in_memory().unwrap();
        // meeting 999 不存在，外鍵未開啟的話這行會成功
        let r = conn.execute(
            "INSERT INTO notes (meeting_id, id, event_seq, text, meeting_time_ms,
                                captured_audio_ms, created_at)
             VALUES (999, 1, 1, 't', 0, 0, 'now')",
            [],
        );
        assert!(r.is_err(), "外鍵未生效，孤兒列被寫進去了");
    }

    #[test]
    fn claim_kind_has_no_default_and_rejects_omission() {
        let conn = open_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO meetings (id, title, state, created_at) VALUES (1,'m','idle','now');
             INSERT INTO documents (id, meeting_id, purpose, title, created_at)
                 VALUES (1,1,'p','t','now');
             INSERT INTO generation_runs (id, document_id, version_no, through_event_seq,
                                          status, created_at)
                 VALUES (1,1,1,0,'completed','now');",
        )
        .unwrap();
        // 省略 claim_kind：沒有 DEFAULT 就必須失敗，不能靜默補成 inference
        let r = conn.execute(
            "INSERT INTO document_blocks (run_id, position, kind, content)
             VALUES (1, 0, 'paragraph', 'x')",
            [],
        );
        assert!(r.is_err(), "claim_kind 被靜默補值了");
    }

    #[test]
    fn silence_fill_segment_must_have_zero_captured_length() {
        let conn = open_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO meetings (id, title, state, created_at) VALUES (1,'m','idle','now')",
        )
        .unwrap();
        let insert = |is_fill: i64, cap_end: i64| {
            conn.execute(
                "INSERT INTO audio_segments (meeting_id, track, source_epoch, path,
                     captured_start_ms, captured_end_ms, meeting_start_ms, meeting_end_ms,
                     is_silence_fill, checksum, created_event_seq)
                 VALUES (1,'mic',0,?1, 100, ?2, 100, 900, ?3, 'sha', 1)",
                rusqlite::params![format!("p{is_fill}{cap_end}"), cap_end, is_fill],
            )
        };
        // captured 長度為零、meeting 長度不為零：合法的壓縮靜音段
        assert!(insert(1, 100).is_ok());
        // 標成靜音卻有 captured 長度：矛盾，必須擋下
        assert!(insert(1, 500).is_err());
    }
}
