//! 日期与"多久以前"。只为了两件事：默认存档名，和存档列表里的修改时间。
//!
//! ## 为什么不用 UTC
//!
//! 用户在中国。晚上十点存的世界，UTC 已经是第二天了——文件名写成
//! `world-20261023` 会让人怀疑自己记错了日子。所以**默认文件名取本地日期**。
//!
//! ## 为什么不为它引一个日期库
//!
//! 要的只有"今天是几号"和"这是几小时前"。为两个字符串拉进 `chrono`
//! （连带 `iana-time-zone` 之类的依赖）不划算，而 Windows 上取本地日期
//! 只要一次 `GetLocalTime`。
//!
//! ## 为什么修改时间显示"多久以前"而不是绝对时刻
//!
//! 绝对时刻要时区，而时区要一张随系统设置变化的表——那是 `chrono` 的活。
//! 而"3 分钟前"恰好是玩家真正想问的问题：**哪个是最新的**。
//! 用 `SystemTime::now() - mtime` 算，两边都是 UTC，不碰时区，也不会
//! 因为夏令时出错。

use std::time::{Duration, SystemTime};

/// 本地日期（年月日）。时间部分不取——默认存档名只用到日期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Date {
    /// 年（四位）。
    pub year: u16,
    /// 月（1..=12）。
    pub month: u16,
    /// 日（1..=31）。
    pub day: u16,
}

impl Date {
    /// 存档名里那种紧凑写法：`20261022`。
    pub fn compact(&self) -> String {
        format!("{:04}{:02}{:02}", self.year, self.month, self.day)
    }
}

/// 今天的**本地**日期。
#[cfg(windows)]
pub fn today() -> Date {
    // `GetLocalTime` 是 user32/kernel32 里最便宜的一个调用之一：
    // 一次系统调用，没有任何分配。比"读时区环境变量再自己算"可靠得多。
    #[repr(C)]
    struct SystemTimeRaw {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    extern "system" {
        fn GetLocalTime(out: *mut SystemTimeRaw);
    }
    let mut raw = SystemTimeRaw {
        year: 1970,
        month: 1,
        day_of_week: 0,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    // SAFETY: `GetLocalTime` 只写一个 16 字节的 POD 结构体，指针来自本栈帧，
    // 生命周期覆盖整个调用。它不分配、不回调、不保存指针。
    unsafe { GetLocalTime(&mut raw) };
    Date {
        year: raw.year,
        month: raw.month,
        day: raw.day,
    }
}

/// 今天的日期（非 Windows 退化成 UTC）。
#[cfg(not(windows))]
pub fn today() -> Date {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    date_from_unix_days(secs.div_euclid(86_400))
}

/// 从"1970-01-01 起的天数"算出公历日期。
///
/// Howard Hinnant 的 `civil_from_days`：纯整数、无查表、无闰年特判表。
/// 抄一遍而不是引库，是因为它只有十行，而且这个算法五十年内不会变。
#[cfg(not(windows))]
fn date_from_unix_days(days: i64) -> Date {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    Date {
        year: (y + i64::from(m <= 2)) as u16,
        month: m as u16,
        day: d as u16,
    }
}

/// "多久以前"，给存档列表用。
///
/// 刻意不显示"3 天前"以外的东西：超过一周的档，具体是几天前没有意义，
/// 而"上个月"这种说法要么需要日历、要么会骗人。
pub fn ago(t: SystemTime) -> String {
    let Ok(elapsed) = SystemTime::now().duration_since(t) else {
        // 文件时间在未来（改过系统时钟、或者时区跳变）。说"刚刚"比说
        // "负三小时前"诚实——两种情况都是"这个数不可信"。
        return "刚刚".to_owned();
    };
    humanize(elapsed)
}

/// 把一段时长写成中文。
pub fn humanize(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..=59 => "刚刚".to_owned(),
        60..=3599 => format!("{} 分钟前", secs / 60),
        3600..=86_399 => format!("{} 小时前", secs / 3600),
        86_400..=604_799 => format!("{} 天前", secs / 86_400),
        _ => format!("{} 周前", secs / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本地日期必须是一个**像样的**日期。
    ///
    /// 判据不能写成"等于今天"——那需要另一份日历实现。这里只钉住
    /// "它没有退化成 1970" 和 "月日在合法范围内"：`GetLocalTime` 调用
    /// 失败时结构体保持初始值，那会让所有存档名都叫 `world-19700101`，
    /// 而那种 bug 在界面上看起来完全正常。
    #[test]
    fn today_is_a_plausible_date() {
        let d = today();
        assert!(d.year >= 2024, "年份不像话：{}", d.year);
        assert!((1..=12).contains(&d.month), "月份越界：{}", d.month);
        assert!((1..=31).contains(&d.day), "日期越界：{}", d.day);
        assert_eq!(d.compact().len(), 8, "紧凑写法必须是 8 位");
    }

    #[test]
    fn durations_read_like_a_person_wrote_them() {
        assert_eq!(humanize(Duration::from_secs(0)), "刚刚");
        assert_eq!(humanize(Duration::from_secs(59)), "刚刚");
        assert_eq!(humanize(Duration::from_secs(60)), "1 分钟前");
        assert_eq!(humanize(Duration::from_secs(3599)), "59 分钟前");
        assert_eq!(humanize(Duration::from_secs(3600)), "1 小时前");
        assert_eq!(humanize(Duration::from_secs(86_400)), "1 天前");
        assert_eq!(humanize(Duration::from_secs(604_800)), "1 周前");
    }

    /// 未来的时间戳说"刚刚"，不说负数。
    #[test]
    fn a_timestamp_in_the_future_does_not_go_negative() {
        let future = SystemTime::now() + Duration::from_secs(600);
        assert_eq!(ago(future), "刚刚");
    }

    #[cfg(not(windows))]
    #[test]
    fn the_civil_calendar_matches_known_dates() {
        assert_eq!(
            date_from_unix_days(0),
            Date { year: 1970, month: 1, day: 1 }
        );
        // 2024-02-29（闰日）：从 1970-01-01 起 19782 天。
        assert_eq!(
            date_from_unix_days(19_782),
            Date { year: 2024, month: 2, day: 29 }
        );
    }
}
