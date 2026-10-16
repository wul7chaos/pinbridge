//! 时间工具。
//!
//! 单点说明：PinBridge 内部存在两套时间语义，混用会导致重放错乱。
//!
//! * **单调时钟**（[`Instant`]）：用于超时、租约续期、脚本预算。目标进程可能被
//!   系统时间改动影响，单调时钟不受影响。
//! * **墙钟**（[`now_unix_millis`]）：只用于落盘元数据（.pbt 头部、审计记录），
//!   让"这份轨迹是什么时候录的"可读。
//!
//! 轨迹内部的顺序**一律靠事件序号**，不靠时间戳。这样离线重放不依赖采样精度，
//! 也不会因为在虚拟机里被拖慢而丢事件。

pub use std::time::{Duration, Instant};

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 Unix 毫秒时间戳。系统时钟早于 1970 时返回 0（不 panic）。
#[must_use]
pub fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 把毫秒时间戳格式化为 RFC 3339（UTC，秒级精度，无外部依赖实现）。
///
/// 只做"给人看"的输出，不用于解析，所以实现保持在最小区间内。
#[must_use]
pub fn format_unix_millis_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant 的 `civil_from_days`，把"1970 起的天数"换成公历年月日。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
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
    fn epoch_and_known_instants() {
        assert_eq!(format_unix_millis_utc(0), "1970-01-01T00:00:00Z");
        // 2026-10-16T07:56:23Z（本仓库初始化时刻的 UTC 表达）
        assert_eq!(
            format_unix_millis_utc(1_792_137_383_000),
            "2026-10-16T07:56:23Z"
        );
        assert_eq!(
            format_unix_millis_utc(1_792_137_383_999),
            "2026-10-16T07:56:23Z"
        );
    }

    #[test]
    fn wall_clock_is_after_2020() {
        assert!(now_unix_millis() > 1_577_836_800_000);
    }

    #[test]
    fn monotonic_clock_advances() {
        let a = Instant::now();
        std::thread::sleep(Duration::from_millis(2));
        assert!(a.elapsed() >= Duration::from_millis(1));
    }
}
