//! Optional per-packet JSON Lines log (`--log packets.jsonl`).

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

pub struct PacketLog(Mutex<BufWriter<File>>);

/// One line per incoming packet.
#[derive(Serialize, Debug)]
pub struct Record<'a> {
    /// Seconds since the proxy started.
    pub t: f64,
    pub dir: &'a str,
    pub len: usize,
    /// "forward", "drop" or "queue-full".
    pub action: &'a str,
    pub delay_ms: f64,
    pub dup: bool,
    pub corrupt: bool,
    pub reorder: bool,
}

impl PacketLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self(Mutex::new(BufWriter::new(File::create(path)?))))
    }

    pub fn write(&self, record: &Record) {
        let mut w = self.0.lock().unwrap();
        let _ = serde_json::to_writer(&mut *w, record);
        let _ = w.write_all(b"\n");
    }

    pub fn flush(&self) {
        let _ = self.0.lock().unwrap().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conditions::{Config, Direction};
    use crate::shaper::Shaper;
    use crate::stats::Stats;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::sync::{Arc, RwLock};

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lagproxy-test-{}-{name}.jsonl", std::process::id()))
    }

    #[test]
    fn writes_one_json_line_per_packet_decision() {
        let path = temp_path("shaper");
        let log = Arc::new(PacketLog::open(&path).unwrap());
        let mut config = Config::default();
        config.set("latency", "25ms").unwrap();
        let config = Arc::new(RwLock::new(config));
        let mut shaper = Shaper::with_rng(
            Direction::Down,
            config.clone(),
            Arc::new(Stats::new()),
            Some(log.clone()),
            false,
            StdRng::seed_from_u64(1),
        );
        shaper.shape(vec![1, 2, 3]);
        config.write().unwrap().set("loss", "100%").unwrap();
        shaper.shape(vec![4, 5]);
        log.flush();

        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let lines: Vec<serde_json::Value> =
            text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["dir"], "down");
        assert_eq!(lines[0]["len"], 3);
        assert_eq!(lines[0]["action"], "forward");
        assert_eq!(lines[0]["delay_ms"], 25.0);
        assert_eq!(lines[0]["dup"], false);
        assert_eq!(lines[1]["action"], "drop");
        assert_eq!(lines[1]["len"], 2);
        assert!(lines[1]["t"].as_f64().unwrap() >= lines[0]["t"].as_f64().unwrap());
    }
}
