//! Scripted scenarios: a YAML list of steps that change conditions over time.
//!
//! ```yaml
//! - at: 0s
//!   latency: 40ms
//! - at: 10s
//!   loss: 100%
//! - at: 15s
//!   end: true
//! ```
//!
//! Any key accepted on the command line works in a step, including `preset`
//! and `up:`/`down:` prefixed keys.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::RwLock;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::conditions::Config;
use crate::parse;

#[derive(Deserialize)]
struct RawStep {
    at: serde_yaml::Value,
    #[serde(default)]
    end: bool,
    #[serde(flatten)]
    settings: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub at: Duration,
    pub end: bool,
    pub settings: Vec<(String, String)>,
}

pub fn load(path: &Path) -> Result<Vec<Step>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse_str(&text)
}

pub fn parse_str(yaml: &str) -> Result<Vec<Step>, String> {
    let raw: Vec<RawStep> = serde_yaml::from_str(yaml).map_err(|e| format!("invalid scenario: {e}"))?;
    let mut steps = Vec::new();
    let mut scratch = Config::default();
    for (i, r) in raw.into_iter().enumerate() {
        let settings: Vec<(String, String)> = r
            .settings
            .into_iter()
            .map(|(k, v)| Ok((k, scalar(&v)?)))
            .collect::<Result<_, String>>()
            .map_err(|e| format!("step {}: {e}", i + 1))?;
        for (k, v) in &settings {
            scratch.set(k, v).map_err(|e| format!("step {}: {e}", i + 1))?;
        }
        let at = parse::duration(&scalar(&r.at)?).map_err(|e| format!("step {}: {e}", i + 1))?;
        steps.push(Step { at, end: r.end, settings });
    }
    steps.sort_by_key(|s| s.at);
    Ok(steps)
}

fn scalar(v: &serde_yaml::Value) -> Result<String, String> {
    match v {
        serde_yaml::Value::String(s) => Ok(s.clone()),
        serde_yaml::Value::Number(n) => Ok(n.to_string()),
        serde_yaml::Value::Bool(b) => Ok(b.to_string()),
        other => Err(format!("expected a scalar value, got {other:?}")),
    }
}

/// Plays the scenario, blocking. Returns `true` when an `end: true` step was
/// reached (the caller should shut down), `false` if the steps simply ran out.
/// With `looping`, the scenario restarts from zero instead of returning.
pub fn run(steps: &[Step], config: &RwLock<Config>, looping: bool) -> bool {
    // A scenario with no time span would spin forever if looped.
    let looping = looping && steps.iter().any(|s| !s.at.is_zero());
    loop {
        let start = Instant::now();
        for step in steps {
            let Some(due) = start.checked_add(step.at) else { return false };
            thread::sleep(due.saturating_duration_since(Instant::now()));
            let mut cfg = config.write().unwrap();
            for (k, v) in &step.settings {
                let _ = cfg.set(k, v); // validated at load time
            }
            drop(cfg);
            if step.end {
                if looping {
                    break;
                }
                return true;
            }
        }
        if !looping {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const SPIKE: &str = "
- at: 0s
  latency: 40ms
  jitter: 5ms
- at: 10s
  latency: 400ms
  loss: 8%
- at: 20s
  loss: 100%
- at: 30s
  end: true
";

    #[test]
    fn parses_readme_example() {
        let steps = parse_str(SPIKE).unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].at, Duration::ZERO);
        assert_eq!(steps[1].at, Duration::from_secs(10));
        assert!(steps[3].end);
        assert!(steps[3].settings.is_empty());
        assert!(steps[1].settings.contains(&("loss".into(), "8%".into())));
    }

    #[test]
    fn accepts_numbers_and_direction_prefixes() {
        let steps = parse_str("- at: 1.5\n  loss: 0.5\n  up:latency: 10ms\n  preset: lan").unwrap();
        assert_eq!(steps[0].at, Duration::from_micros(1500));
        assert_eq!(steps[0].settings.len(), 3);
    }

    #[test]
    fn rejects_bad_keys_and_values_up_front() {
        assert!(parse_str("- at: 0s\n  colour: red").unwrap_err().contains("step 1"));
        assert!(parse_str("- at: 0s\n  loss: 200%").is_err());
        assert!(parse_str("- latency: 1ms").is_err());
        assert!(parse_str("not a list").is_err());
    }

    #[test]
    fn steps_are_sorted_by_time() {
        let steps = parse_str("- at: 5s\n  loss: 1%\n- at: 1s\n  loss: 2%").unwrap();
        assert_eq!(steps[0].at, Duration::from_secs(1));
    }

    #[test]
    fn run_applies_steps_and_reports_end() {
        let config = Arc::new(RwLock::new(Config::default()));
        let steps = parse_str("- at: 0s\n  latency: 5ms\n- at: 30ms\n  latency: 9ms\n  end: true").unwrap();
        let started = Instant::now();
        assert!(run(&steps, &config, false));
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert_eq!(config.read().unwrap().up.latency, Duration::from_millis(9));

        let steps = parse_str("- at: 0s\n  latency: 1ms").unwrap();
        assert!(!run(&steps, &config, false));
        assert_eq!(config.read().unwrap().down.latency, Duration::from_millis(1));
    }

    #[test]
    fn looping_restarts_after_end() {
        let config = Arc::new(RwLock::new(Config::default()));
        let steps = parse_str("- at: 0s\n  latency: 1ms\n- at: 20ms\n  latency: 2ms\n  end: true").unwrap();
        thread::spawn({
            let config = config.clone();
            move || run(&steps, &config, true)
        });
        let mut seen: Vec<Duration> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while seen.len() < 4 && Instant::now() < deadline {
            let latency = config.read().unwrap().up.latency;
            if latency != Duration::ZERO && seen.last() != Some(&latency) {
                seen.push(latency);
            }
            thread::sleep(Duration::from_millis(1));
        }
        let ms: Vec<u128> = seen.iter().map(|d| d.as_millis()).collect();
        assert_eq!(ms, [1, 2, 1, 2], "scenario did not loop");
    }

    #[test]
    fn looping_a_zero_length_scenario_returns_instead_of_spinning() {
        let config = RwLock::new(Config::default());
        let steps = parse_str("- at: 0s\n  latency: 3ms").unwrap();
        assert!(!run(&steps, &config, true));
        assert_eq!(config.read().unwrap().up.latency, Duration::from_millis(3));
    }
}
