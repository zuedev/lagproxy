//! Lock-free counters, one set per direction, plus the summary table printed
//! on exit and the JSON served by `/stats`.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::conditions::Direction;
use crate::parse;

#[derive(Default)]
pub struct DirStats {
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    pub dropped: AtomicU64,
    pub dup: AtomicU64,
    pub reordered: AtomicU64,
    pub corrupted: AtomicU64,
    /// Packets handed to the scheduler (includes duplicates).
    pub delivered: AtomicU64,
    pub delay_total_us: AtomicU64,
    /// Largest bandwidth queue backlog seen, in bytes.
    pub max_queue: AtomicU64,
    /// Packets currently waiting in the scheduler.
    pub in_flight: AtomicU64,
}

impl DirStats {
    pub fn record_delay(&self, delay: Duration) {
        self.delivered.fetch_add(1, Relaxed);
        self.delay_total_us.fetch_add(delay.as_micros() as u64, Relaxed);
    }

    pub fn avg_delay(&self) -> Duration {
        let n = self.delivered.load(Relaxed).max(1);
        Duration::from_micros(self.delay_total_us.load(Relaxed) / n)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "packets": self.packets.load(Relaxed),
            "bytes": self.bytes.load(Relaxed),
            "dropped": self.dropped.load(Relaxed),
            "dup": self.dup.load(Relaxed),
            "reordered": self.reordered.load(Relaxed),
            "corrupted": self.corrupted.load(Relaxed),
            "avg_delay_ms": self.avg_delay().as_secs_f64() * 1000.0,
            "max_queue_bytes": self.max_queue.load(Relaxed),
            "in_flight": self.in_flight.load(Relaxed),
        })
    }
}

pub struct Stats {
    pub up: DirStats,
    pub down: DirStats,
    pub started: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    pub fn new() -> Self {
        Self { up: DirStats::default(), down: DirStats::default(), started: Instant::now() }
    }

    pub fn dir(&self, dir: Direction) -> &DirStats {
        match dir {
            Direction::Up => &self.up,
            Direction::Down => &self.down,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "uptime_s": self.started.elapsed().as_secs_f64(),
            "up": self.up.to_json(),
            "down": self.down.to_json(),
        })
    }

    /// The summary shown on exit.
    pub fn table(&self) -> String {
        let header = ["Direction", "Packets", "Bytes", "Dropped", "Dup", "Reordered", "Avg delay", "Max queue"];
        let row = |name: &str, s: &DirStats| {
            [
                name.to_string(),
                thousands(s.packets.load(Relaxed)),
                parse::fmt_bytes(s.bytes.load(Relaxed)),
                thousands(s.dropped.load(Relaxed)),
                thousands(s.dup.load(Relaxed)),
                thousands(s.reordered.load(Relaxed)),
                parse::fmt_duration(s.avg_delay()),
                parse::fmt_bytes(s.max_queue.load(Relaxed)),
            ]
        };
        let rows = [
            header.map(String::from),
            row("up", &self.up),
            row("down", &self.down),
        ];
        let widths: Vec<usize> = (0..header.len())
            .map(|i| rows.iter().map(|r| r[i].len()).max().unwrap_or(0))
            .collect();
        rows.iter()
            .map(|r| {
                r.iter()
                    .zip(&widths)
                    .map(|(cell, w)| format!("{cell:<w$}"))
                    .collect::<Vec<_>>()
                    .join("   ")
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 48211 -> "48,211"
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(48211), "48,211");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn table_has_aligned_columns() {
        let s = Stats::new();
        s.up.packets.fetch_add(48211, Relaxed);
        s.up.bytes.fetch_add(5_347_737, Relaxed);
        s.up.record_delay(Duration::from_millis(121));
        let table = s.table();
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("Direction   Packets"));
        assert!(lines[1].contains("48,211"));
        assert!(lines[1].contains("5.1 MB"));
        assert!(lines[1].contains("121ms"));
        assert!(lines[2].starts_with("down"));
    }

    #[test]
    fn avg_delay_ignores_zero_division() {
        let s = DirStats::default();
        assert_eq!(s.avg_delay(), Duration::ZERO);
        s.record_delay(Duration::from_millis(100));
        s.record_delay(Duration::from_millis(200));
        assert_eq!(s.avg_delay(), Duration::from_millis(150));
    }
}
