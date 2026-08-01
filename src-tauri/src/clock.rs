//! 掛鐘時間。
//!
//! §11 的三種時間裡，只有 `wall_clock_utc` 來自這裡，而且只供顯示。
//! `meeting_time_ms` 與 `captured_audio_ms` 一律由 `SessionHandle` 的
//! 單調時鐘推進，不從掛鐘換算，否則使用者調時間或 NTP 校正就會讓
//! 已經寫進日誌的時間軸倒退。

use std::time::{SystemTime, UNIX_EPOCH};

/// RFC 3339 UTC 字串，秒以下保留毫秒。
///
/// 存字串而不是 epoch 整數，是因為這個欄位的唯一用途是顯示與人工比對，
/// 排序用的是 `seq`。
pub fn now_utc() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_utc_millis(d.as_millis() as i64)
}

/// 把 Unix 毫秒格式化成 `YYYY-MM-DDTHH:MM:SS.mmmZ`。
///
/// 自己算而不是拉 chrono：需要的只有這一個方向的轉換，
/// 而 civil-from-days 是有標準解的整數運算。
pub fn format_utc_millis(ms: i64) -> String {
    let (secs, millis) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// Howard Hinnant 的 civil_from_days：以 1970-01-01 為第 0 天。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_utc_millis_matches_known_instants() {
        assert_eq!(format_utc_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_utc_millis(1_000), "1970-01-01T00:00:01.000Z");
        // 2026-08-01T00:00:00Z
        assert_eq!(
            format_utc_millis(1_785_542_400_000),
            "2026-08-01T00:00:00.000Z"
        );
        // 閏日
        assert_eq!(
            format_utc_millis(1_709_164_800_123),
            "2024-02-29T00:00:00.123Z"
        );
    }

    #[test]
    fn now_utc_is_sortable_as_a_plain_string() {
        let a = format_utc_millis(1_785_542_400_000);
        let b = format_utc_millis(1_785_542_401_000);
        assert!(a < b, "字典序必須等同時間序，否則欄位排序會錯");
        assert_eq!(now_utc().len(), a.len());
    }
}
