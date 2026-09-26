//! Named starting points for common real world connections.
//! Applying a preset resets the direction to defaults first, so a preset is a
//! complete description rather than a patch.

use crate::conditions::Conditions;

pub const PRESETS: &[(&str, &[(&str, &str)])] = &[
    ("lan", &[("latency", "1ms")]),
    ("fibre", &[("latency", "15ms"), ("jitter", "2ms")]),
    ("cable", &[("latency", "40ms"), ("jitter", "10ms"), ("loss", "0.1%")]),
    ("wifi-ok", &[("latency", "50ms"), ("jitter", "20ms"), ("loss", "0.5%")]),
    ("wifi-bad", &[("latency", "120ms"), ("jitter", "60ms"), ("loss", "3%"), ("reorder", "2%")]),
    ("mobile-4g", &[("latency", "90ms"), ("jitter", "40ms"), ("loss", "1%"), ("bandwidth", "5mbit")]),
    ("mobile-3g", &[("latency", "250ms"), ("jitter", "100ms"), ("loss", "3%"), ("bandwidth", "500kbit")]),
    ("satellite", &[("latency", "600ms"), ("jitter", "30ms"), ("loss", "0.5%")]),
    ("cross-region", &[("latency", "180ms"), ("jitter", "15ms"), ("loss", "0.2%")]),
    (
        "hostile",
        &[
            ("latency", "300ms"),
            ("jitter", "200ms"),
            ("loss", "10%"),
            ("dup", "5%"),
            ("reorder", "5%"),
            ("corrupt", "1%"),
        ],
    ),
];

pub fn names() -> Vec<&'static str> {
    PRESETS.iter().map(|(n, _)| *n).collect()
}

pub fn apply(name: &str, conditions: &mut Conditions) -> Result<(), String> {
    let name = name.trim().to_ascii_lowercase();
    let (_, settings) = PRESETS
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| format!("unknown preset '{name}', available: {}", names().join(", ")))?;
    *conditions = Conditions::default();
    for (key, value) in settings.iter() {
        conditions.set(key, value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn every_preset_parses() {
        for name in names() {
            let mut c = Conditions::default();
            apply(name, &mut c).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn values_match_the_readme_table() {
        let get = |name: &str, key: &str| {
            let mut c = Conditions::default();
            apply(name, &mut c).unwrap();
            c.get(key).unwrap()
        };
        assert_eq!(get("lan", "latency"), "1ms");
        assert_eq!(get("lan", "loss"), "0%");
        assert_eq!(get("fibre", "jitter"), "2ms");
        assert_eq!(get("cable", "loss"), "0.1%");
        assert_eq!(get("wifi-bad", "reorder"), "2%");
        assert_eq!(get("mobile-4g", "bandwidth"), "5mbit");
        assert_eq!(get("mobile-3g", "bandwidth"), "500kbit");
        assert_eq!(get("satellite", "latency"), "600ms");
        assert_eq!(get("cross-region", "loss"), "0.2%");
        assert_eq!(get("hostile", "dup"), "5%");
        assert_eq!(get("hostile", "corrupt"), "1%");
        assert_eq!(names().len(), 10);
    }

    #[test]
    fn preset_replaces_previous_values() {
        let mut c = Conditions::default();
        c.set("corrupt", "50%").unwrap();
        apply("mobile-3g", &mut c).unwrap();
        assert_eq!(c.corrupt, 0.0);
        assert_eq!(c.latency, Duration::from_millis(250));
        assert_eq!(c.bandwidth, 500_000);
    }

    #[test]
    fn unknown_preset_lists_options() {
        let err = apply("dialup", &mut Conditions::default()).unwrap_err();
        assert!(err.contains("wifi-bad"));
    }
}
