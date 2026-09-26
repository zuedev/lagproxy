//! Parsing and formatting of the human friendly values used on the command
//! line, in scenario files and over the API ("80ms", "2%", "1mbit", "64kb").

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

/// Splits "12.5ms" into (12.5, "ms").
fn split_unit(s: &str) -> Result<(f64, String), String> {
    let s = s.trim();
    let idx = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let num: f64 = s[..idx]
        .parse()
        .map_err(|_| format!("'{s}' does not start with a number"))?;
    Ok((num, s[idx..].trim().to_ascii_lowercase()))
}

/// "80ms", "1.5s", "250us". A bare number means milliseconds.
pub fn duration(s: &str) -> Result<Duration, String> {
    let (n, unit) = split_unit(s)?;
    let secs = match unit.as_str() {
        "" | "ms" => n / 1000.0,
        "s" => n,
        "us" | "µs" => n / 1_000_000.0,
        "m" | "min" => n * 60.0,
        _ => return Err(format!("unknown duration unit '{unit}' in '{s}'")),
    };
    // Keeps `Instant + Duration` arithmetic from overflowing on absurd input.
    const MAX_SECS: f64 = 30.0 * 24.0 * 3600.0;
    match Duration::try_from_secs_f64(secs) {
        Ok(d) if d.as_secs_f64() <= MAX_SECS => Ok(d),
        _ => Err(format!("'{s}' is not a usable duration (max 30 days)")),
    }
}

/// "2%" or "0.02" -> 0.02.
pub fn percent(s: &str) -> Result<f64, String> {
    let (n, unit) = split_unit(s)?;
    let p = match unit.as_str() {
        "%" => n / 100.0,
        "" => n,
        _ => return Err(format!("'{s}' is not a percentage")),
    };
    if (0.0..=1.0).contains(&p) {
        Ok(p)
    } else {
        Err(format!("'{s}' is outside 0-100%"))
    }
}

/// Bit rates use decimal prefixes: "1mbit" = 1_000_000 bits/s.
/// "0" or "unlimited" disables the limit.
pub fn bandwidth(s: &str) -> Result<u64, String> {
    if matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "0" | "unlimited" | "none" | "off"
    ) {
        return Ok(0);
    }
    let (n, unit) = split_unit(s)?;
    let unit = unit.trim_end_matches("ps").trim_end_matches("/s");
    let mult = match unit {
        "" | "bit" | "b" => 1.0,
        "kbit" | "kb" | "k" => 1e3,
        "mbit" | "mb" | "m" => 1e6,
        "gbit" | "gb" | "g" => 1e9,
        _ => return Err(format!("unknown bandwidth unit '{unit}' in '{s}'")),
    };
    Ok((n * mult) as u64)
}

/// Byte sizes use binary prefixes: "64kb" = 65536 bytes.
pub fn bytes(s: &str) -> Result<u64, String> {
    let (n, unit) = split_unit(s)?;
    let mult = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => return Err(format!("unknown size unit '{unit}' in '{s}'")),
    };
    Ok((n * mult) as u64)
}

/// "host:port", ":port" or "port". A missing host falls back to `default_host`.
pub fn socket_addr(s: &str, default_host: &str) -> Result<SocketAddr, String> {
    let s = s.trim();
    let full = if s.starts_with(':') {
        format!("{default_host}{s}")
    } else if s.chars().all(|c| c.is_ascii_digit()) {
        format!("{default_host}:{s}")
    } else {
        s.to_string()
    };
    full.to_socket_addrs()
        .map_err(|e| format!("cannot resolve '{s}': {e}"))?
        .next()
        .ok_or_else(|| format!("cannot resolve '{s}'"))
}

/// Formats a float with up to two decimals and no trailing zeros.
pub fn num(x: f64) -> String {
    let s = format!("{x:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub fn fmt_duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms >= 1000.0 {
        format!("{}s", num(ms / 1000.0))
    } else {
        format!("{}ms", num(ms))
    }
}

pub fn fmt_percent(p: f64) -> String {
    format!("{}%", num(p * 100.0))
}

pub fn fmt_bandwidth(bits: u64) -> String {
    let b = bits as f64;
    match bits {
        0 => "unlimited".to_string(),
        _ if b >= 1e9 => format!("{}gbit", num(b / 1e9)),
        _ if b >= 1e6 => format!("{}mbit", num(b / 1e6)),
        _ if b >= 1e3 => format!("{}kbit", num(b / 1e3)),
        _ => format!("{bits}bit"),
    }
}

pub fn fmt_bytes(n: u64) -> String {
    let b = n as f64;
    const K: f64 = 1024.0;
    match n {
        _ if b >= K * K * K => format!("{} GB", num(b / K / K / K)),
        _ if b >= K * K => format!("{} MB", num(b / K / K)),
        _ if b >= K => format!("{} KB", num(b / K)),
        _ => format!("{n} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration("80ms").unwrap(), Duration::from_millis(80));
        assert_eq!(duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(duration("250us").unwrap(), Duration::from_micros(250));
        assert_eq!(duration("40").unwrap(), Duration::from_millis(40));
        assert!(duration("fast").is_err());
        assert!(duration("10 parsecs").is_err());
        assert!(duration("-5ms").is_err());
        assert!(duration("99999999999999999999999999s").is_err());
        assert!(duration("31d").is_err());
        assert!(duration("50000000s").is_err());
        assert!(duration("2592000s").is_ok());
    }

    #[test]
    fn percents() {
        assert_eq!(percent("2%").unwrap(), 0.02);
        assert_eq!(percent("0.02").unwrap(), 0.02);
        assert_eq!(percent("100%").unwrap(), 1.0);
        assert!(percent("150%").is_err());
        assert!(percent("2").is_err());
    }

    #[test]
    fn bandwidths() {
        assert_eq!(bandwidth("1mbit").unwrap(), 1_000_000);
        assert_eq!(bandwidth("500kbit").unwrap(), 500_000);
        assert_eq!(bandwidth("2.5mbps").unwrap(), 2_500_000);
        assert_eq!(bandwidth("unlimited").unwrap(), 0);
        assert!(bandwidth("1 furlong").is_err());
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(bytes("64kb").unwrap(), 65536);
        assert_eq!(bytes("1mb").unwrap(), 1 << 20);
        assert_eq!(bytes("100").unwrap(), 100);
    }

    #[test]
    fn addresses() {
        assert_eq!(
            socket_addr(":7777", "0.0.0.0").unwrap(),
            "0.0.0.0:7777".parse().unwrap()
        );
        assert_eq!(
            socket_addr("7777", "127.0.0.1").unwrap(),
            "127.0.0.1:7777".parse().unwrap()
        );
        assert_eq!(
            socket_addr("[::1]:5", "0.0.0.0").unwrap(),
            "[::1]:5".parse().unwrap()
        );
        assert!(socket_addr("nope", "0.0.0.0").is_err());
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_duration(Duration::from_millis(120)), "120ms");
        assert_eq!(fmt_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(fmt_percent(0.005), "0.5%");
        assert_eq!(fmt_bandwidth(5_000_000), "5mbit");
        assert_eq!(fmt_bandwidth(0), "unlimited");
        assert_eq!(fmt_bytes(65536), "64 KB");
        assert_eq!(fmt_bytes(5_347_737), "5.1 MB");
    }
}
