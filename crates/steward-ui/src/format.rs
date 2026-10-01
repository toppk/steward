use std::collections::HashMap;
use std::sync::OnceLock;

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `YYYY-MM-DD` in UTC, via Howard Hinnant's days-to-civil.
pub fn date(unix: i64) -> String {
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn user(uid: u32) -> String {
    static USERS: OnceLock<HashMap<u32, String>> = OnceLock::new();
    USERS
        .get_or_init(|| {
            std::fs::read_to_string("/etc/passwd")
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    let mut f = l.split(':');
                    let name = f.next()?;
                    let uid = f.nth(1)?.parse().ok()?;
                    Some((uid, name.to_string()))
                })
                .collect()
        })
        .get(&uid)
        .cloned()
        .unwrap_or_else(|| uid.to_string())
}

pub fn mode(kind_char: char, mode: u32) -> String {
    let mut s = String::with_capacity(10);
    s.push(kind_char);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Local wall-clock time of a Unix timestamp: `14:03:27`, with the date
/// when it isn't today.
pub fn clock(unix: f64) -> String {
    use chrono::TimeZone as _;
    let Some(t) = chrono::Local.timestamp_opt(unix as i64, 0).single() else {
        return "?".into();
    };
    if t.date_naive() == chrono::Local::now().date_naive() {
        t.format("%H:%M:%S").to_string()
    } else {
        t.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// A span of time, coarse: `45s`, `12m 03s`, `3h 07m`, `2d 04h`.
pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        3600..86_400 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {:02}h", s / 86_400, s % 86_400 / 3600),
    }
}

/// Seconds since the Unix epoch, now.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_790_739_340), "2026-09-30");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(mode('d', 0o755), "drwxr-xr-x");
    }
}
