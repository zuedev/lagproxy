//! The tweakable network conditions, and the `Config` that holds one set per
//! direction. Everything is set through string key/value pairs so the CLI,
//! scenario files, the HTTP API and the TUI all share one code path.

use std::time::Duration;

use serde_json::{json, Value};

use crate::{parse, presets};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum JitterDist {
    #[default]
    Uniform,
    Normal,
    Pareto,
}

impl JitterDist {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "uniform" => Ok(Self::Uniform),
            "normal" | "gaussian" => Ok(Self::Normal),
            "pareto" => Ok(Self::Pareto),
            _ => Err(format!(
                "unknown jitter distribution '{s}', use uniform, normal or pareto"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Uniform => "uniform",
            Self::Normal => "normal",
            Self::Pareto => "pareto",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Client -> server.
    Up,
    /// Server -> client.
    Down,
}

impl Direction {
    pub fn name(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

/// Conditions applied to one direction of traffic.
#[derive(Clone, Debug, PartialEq)]
pub struct Conditions {
    pub latency: Duration,
    pub jitter: Duration,
    pub jitter_dist: JitterDist,
    /// Probabilities are fractions in 0..=1.
    pub loss: f64,
    pub loss_burst: f64,
    pub dup: f64,
    pub reorder: f64,
    pub corrupt: f64,
    /// Bits per second, 0 = unlimited.
    pub bandwidth: u64,
    /// Bytes that may wait for bandwidth before packets get dropped.
    pub queue: u64,
}

impl Default for Conditions {
    fn default() -> Self {
        Self {
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
            jitter_dist: JitterDist::Uniform,
            loss: 0.0,
            loss_burst: 0.0,
            dup: 0.0,
            reorder: 0.0,
            corrupt: 0.0,
            bandwidth: 0,
            queue: 64 * 1024,
        }
    }
}

/// Every settable key, in display order.
pub const KEYS: &[&str] = &[
    "latency",
    "jitter",
    "jitter-dist",
    "loss",
    "loss-burst",
    "dup",
    "reorder",
    "corrupt",
    "bandwidth",
    "queue",
];

impl Conditions {
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "latency" => self.latency = parse::duration(value)?,
            "jitter" => self.jitter = parse::duration(value)?,
            "jitter-dist" => self.jitter_dist = JitterDist::parse(value)?,
            "loss" => self.loss = parse::percent(value)?,
            "loss-burst" => self.loss_burst = parse::percent(value)?,
            "dup" => self.dup = parse::percent(value)?,
            "reorder" => self.reorder = parse::percent(value)?,
            "corrupt" => self.corrupt = parse::percent(value)?,
            "bandwidth" => self.bandwidth = parse::bandwidth(value)?,
            "queue" => self.queue = parse::bytes(value)?,
            "preset" => presets::apply(value, self)?,
            _ => return Err(format!("unknown setting '{key}'")),
        }
        Ok(())
    }

    /// The value of `key` formatted the same way it is written.
    pub fn get(&self, key: &str) -> Option<String> {
        Some(match key {
            "latency" => parse::fmt_duration(self.latency),
            "jitter" => parse::fmt_duration(self.jitter),
            "jitter-dist" => self.jitter_dist.name().to_string(),
            "loss" => parse::fmt_percent(self.loss),
            "loss-burst" => parse::fmt_percent(self.loss_burst),
            "dup" => parse::fmt_percent(self.dup),
            "reorder" => parse::fmt_percent(self.reorder),
            "corrupt" => parse::fmt_percent(self.corrupt),
            "bandwidth" => parse::fmt_bandwidth(self.bandwidth),
            "queue" => parse::fmt_bytes(self.queue),
            _ => return None,
        })
    }

    pub fn to_json(&self) -> Value {
        Value::Object(
            KEYS.iter()
                .map(|k| (k.to_string(), Value::String(self.get(k).unwrap())))
                .collect(),
        )
    }
}

/// Live configuration for both directions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub up: Conditions,
    pub down: Conditions,
}

impl Config {
    /// Applies a key like `latency`, `up:loss` or `down:preset`.
    /// Leading dashes and underscores are tolerated so CLI flags map 1:1.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let key = key.trim().trim_start_matches('-').replace('_', "-");
        match key.split_once(':') {
            Some(("up", k)) => self.up.set(k, value),
            Some(("down", k)) => self.down.set(k, value),
            Some((d, _)) => Err(format!("unknown direction '{d}', use up: or down:")),
            None => {
                self.up.set(&key, value)?;
                self.down.set(&key, value)
            }
        }
    }

    pub fn dir(&self, dir: Direction) -> &Conditions {
        match dir {
            Direction::Up => &self.up,
            Direction::Down => &self.down,
        }
    }

    pub fn dir_mut(&mut self, dir: Direction) -> &mut Conditions {
        match dir {
            Direction::Up => &mut self.up,
            Direction::Down => &mut self.down,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({ "up": self.up.to_json(), "down": self.down.to_json() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_applies_to_both_directions_by_default() {
        let mut c = Config::default();
        c.set("latency", "80ms").unwrap();
        assert_eq!(c.up.latency, Duration::from_millis(80));
        assert_eq!(c.down.latency, Duration::from_millis(80));
    }

    #[test]
    fn set_with_direction_prefix() {
        let mut c = Config::default();
        c.set("--up:loss", "5%").unwrap();
        c.set("down:jitter_dist", "pareto").unwrap();
        assert_eq!(c.up.loss, 0.05);
        assert_eq!(c.down.loss, 0.0);
        assert_eq!(c.down.jitter_dist, JitterDist::Pareto);
        assert!(c.set("sideways:loss", "1%").is_err());
    }

    #[test]
    fn flags_after_preset_override_it() {
        let mut c = Config::default();
        c.set("preset", "wifi-bad").unwrap();
        c.set("loss", "0%").unwrap();
        assert_eq!(c.up.latency, Duration::from_millis(120));
        assert_eq!(c.up.loss, 0.0);
        assert_eq!(c.up.reorder, 0.02);
    }

    #[test]
    fn unknown_keys_and_bad_values_error() {
        let mut c = Config::default();
        assert!(c.set("colour", "blue").is_err());
        assert!(c.set("loss", "lots").is_err());
        assert!(c.set("jitter-dist", "chaotic").is_err());
    }

    #[test]
    fn get_round_trips() {
        let mut c = Conditions::default();
        for (k, v) in [("latency", "120ms"), ("loss", "3%"), ("bandwidth", "5mbit"), ("queue", "64 KB")] {
            c.set(k, v).unwrap();
            assert_eq!(c.get(k).unwrap(), v);
        }
        assert!(c.get("nothing").is_none());
    }
}
