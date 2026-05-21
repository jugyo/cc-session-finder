//! Compact relative-time and count formatting.
//!
//! - `format_age` → `<1h`, `Nh`, `Nd`, `Nw`, `Nm` (month), `Ny`. Minutes are
//!   intentionally omitted so the `m` suffix is unambiguous.
//! - `format_count` → `999`, `1.2k`, `49.2M`, `1.5B`. Single decimal, trimmed
//!   when whole (e.g. `1k` not `1.0k`).

pub fn format_age(mtime: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age = now - mtime;
    if age < 0 {
        return "now".to_string();
    }
    let hours = age / 3600;
    if hours < 1 {
        return "<1h".to_string();
    }
    if hours < 24 {
        return format!("{}h", hours);
    }
    let days = age / 86_400;
    if days < 7 {
        return format!("{}d", days);
    }
    if days < 30 {
        return format!("{}w", days / 7);
    }
    if days < 365 {
        return format!("{}m", days / 30);
    }
    format!("{}y", days / 365)
}

pub fn format_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let (val, unit) = if n < 1_000_000 {
        (n as f64 / 1_000.0, "k")
    } else if n < 1_000_000_000 {
        (n as f64 / 1_000_000.0, "M")
    } else {
        (n as f64 / 1_000_000_000.0, "B")
    };
    let s = format!("{:.1}", val);
    let trimmed = s.strip_suffix(".0").unwrap_or(&s);
    format!("{}{}", trimmed, unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[test]
    fn counts() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1k");
        assert_eq!(format_count(1_234), "1.2k");
        assert_eq!(format_count(49_220_508), "49.2M");
        assert_eq!(format_count(1_500_000_000), "1.5B");
    }

    #[test]
    fn buckets() {
        let t = now();
        assert_eq!(format_age(t + 60), "now"); // future
        assert_eq!(format_age(t - 10 * 60), "<1h");
        assert_eq!(format_age(t - 3 * 3600), "3h");
        assert_eq!(format_age(t - 3 * 86_400), "3d");
        assert_eq!(format_age(t - 14 * 86_400), "2w");
        assert_eq!(format_age(t - 60 * 86_400), "2m");
        assert_eq!(format_age(t - 800 * 86_400), "2y");
    }
}
