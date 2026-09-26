//! Command line parsing. Flags are processed in order, so anything after
//! `--preset` overrides the preset. Unrecognised flags are tried as condition
//! keys (`--latency`, `--up:loss`, ...) via `Config::set`.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::conditions::Config;
use crate::parse;
use crate::presets;

pub const USAGE: &str = "\
lagproxy - a proxy that makes the network worse on purpose

USAGE:
    lagproxy --listen <addr:port> --target <addr:port> [--udp|--tcp] [conditions] [options]

CONDITIONS (prefix with up: or down: to apply in one direction only):
    --latency <dur>       Fixed one-way delay                          e.g. 80ms
    --jitter <dur>        Random delay added on top of latency         e.g. 30ms
    --jitter-dist <name>  uniform (default), normal or pareto
    --loss <pct>          Chance each packet is dropped                e.g. 2%
    --loss-burst <pct>    Chance the next packet is also dropped       e.g. 50%
    --dup <pct>           Chance each packet is sent twice             e.g. 0.5%
    --reorder <pct>       Chance a packet is held behind the next one  e.g. 1%
    --corrupt <pct>       Chance a random bit is flipped               e.g. 0.1%
    --bandwidth <rate>    Cap throughput (0 = unlimited)               e.g. 1mbit
    --queue <size>        Bytes buffered when over bandwidth           e.g. 64kb
    --preset <name>       Start from a preset, later flags override it

OPTIONS:
    --udp                 Forward UDP datagrams (default)
    --tcp                 Forward TCP streams; loss becomes delay
    --tcp-strict          TCP where loss/dup/corrupt/reorder really break the stream
    --scenario <file>     Change conditions over time from a YAML file
    --loop                Restart the scenario when it ends
    --api <addr:port>     HTTP control API address (default 127.0.0.1:7770)
    --no-api              Disable the HTTP API
    --tui                 Interactive terminal view
    --log <file>          Write every packet decision as JSON lines
    --help, --version

Durations accept us, ms and s. Percentages accept % or a fraction (0.02).
";

pub enum Cli {
    Run(Box<Args>),
    Help,
    Version,
}

#[derive(Debug)]
pub struct Args {
    pub listen: SocketAddr,
    pub target: SocketAddr,
    pub tcp: bool,
    pub tcp_strict: bool,
    pub config: Config,
    pub scenario: Option<PathBuf>,
    pub loop_scenario: bool,
    pub api: Option<SocketAddr>,
    pub tui: bool,
    pub log: Option<PathBuf>,
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, String> {
    let mut args = args.into_iter();
    let mut listen = None;
    let mut target = None;
    let mut tcp = false;
    let mut tcp_strict = false;
    let mut config = Config::default();
    let mut scenario = None;
    let mut loop_scenario = false;
    let mut api = Some("127.0.0.1:7770".parse().unwrap());
    let mut tui = false;
    let mut log = None;

    while let Some(arg) = args.next() {
        let Some(flag) = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')) else {
            return Err(format!("unexpected argument '{arg}'"));
        };
        let (flag, inline) = match flag.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (flag, None),
        };
        let mut value = || {
            inline.clone().or_else(|| args.next()).ok_or_else(|| format!("--{flag} needs a value"))
        };
        match flag {
            "help" | "h" => return Ok(Cli::Help),
            "version" | "V" => return Ok(Cli::Version),
            "udp" => (tcp, tcp_strict) = (false, false),
            "tcp" => tcp = true,
            "tcp-strict" => (tcp, tcp_strict) = (true, true),
            "loop" => loop_scenario = true,
            "tui" => tui = true,
            "no-api" => api = None,
            "listen" => listen = Some(parse::socket_addr(&value()?, "0.0.0.0")?),
            "target" => target = Some(parse::socket_addr(&value()?, "127.0.0.1")?),
            "api" => api = Some(parse::socket_addr(&value()?, "127.0.0.1")?),
            "scenario" => scenario = Some(PathBuf::from(value()?)),
            "log" => log = Some(PathBuf::from(value()?)),
            _ => config.set(flag, &value()?).map_err(|e| format!("--{flag}: {e}"))?,
        }
    }

    Ok(Cli::Run(Box::new(Args {
        listen: listen.ok_or("--listen is required")?,
        target: target.ok_or("--target is required")?,
        tcp,
        tcp_strict,
        config,
        scenario,
        loop_scenario,
        api,
        tui,
        log,
    })))
}

pub fn preset_help() -> String {
    let mut out = String::from("PRESETS:\n");
    for (name, settings) in presets::PRESETS {
        let desc: Vec<String> = settings.iter().map(|(k, v)| format!("{k} {v}")).collect();
        out += &format!("    {name:<14}{}\n", desc.join(", "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn run(s: &str) -> Result<Args, String> {
        match parse(s.split_whitespace().map(String::from))? {
            Cli::Run(a) => Ok(*a),
            _ => Err("not a run".into()),
        }
    }

    #[test]
    fn readme_example() {
        let a = run("--listen 0.0.0.0:7777 --target 127.0.0.1:7778 --udp --latency 120ms --jitter 40ms --loss 3% --reorder 1%")
            .unwrap();
        assert_eq!(a.listen, "0.0.0.0:7777".parse().unwrap());
        assert_eq!(a.target, "127.0.0.1:7778".parse().unwrap());
        assert!(!a.tcp);
        assert_eq!(a.config.up.latency, Duration::from_millis(120));
        assert_eq!(a.config.down.reorder, 0.01);
        assert_eq!(a.api, Some("127.0.0.1:7770".parse().unwrap()));
    }

    #[test]
    fn short_addresses_and_direction_flags() {
        let a = run("--listen :7777 --target :7778 --up:loss 5% --down:latency=10ms").unwrap();
        assert_eq!(a.listen, "0.0.0.0:7777".parse().unwrap());
        assert_eq!(a.target, "127.0.0.1:7778".parse().unwrap());
        assert_eq!(a.config.up.loss, 0.05);
        assert_eq!(a.config.down.loss, 0.0);
        assert_eq!(a.config.down.latency, Duration::from_millis(10));
        assert_eq!(a.config.up.latency, Duration::ZERO);
    }

    #[test]
    fn preset_then_override() {
        let a = run("--listen :1 --target :2 --preset mobile-3g --bandwidth 1mbit").unwrap();
        assert_eq!(a.config.up.latency, Duration::from_millis(250));
        assert_eq!(a.config.up.bandwidth, 1_000_000);
    }

    #[test]
    fn options() {
        let a = run("--listen :1 --target :2 --tcp-strict --scenario s.yaml --loop --no-api --tui --log p.jsonl").unwrap();
        assert!(a.tcp && a.tcp_strict && a.loop_scenario && a.tui);
        assert_eq!(a.api, None);
        assert_eq!(a.scenario.unwrap(), PathBuf::from("s.yaml"));
        assert_eq!(a.log.unwrap(), PathBuf::from("p.jsonl"));
    }

    #[test]
    fn last_protocol_flag_wins() {
        let a = run("--listen :1 --target :2 --tcp-strict --udp").unwrap();
        assert!(!a.tcp && !a.tcp_strict);
        let a = run("--listen :1 --target :2 --udp --tcp").unwrap();
        assert!(a.tcp && !a.tcp_strict);
    }

    #[test]
    fn errors() {
        assert!(run("--target :2").unwrap_err().contains("--listen"));
        assert!(run("--listen :1").unwrap_err().contains("--target"));
        assert!(run("--listen :1 --target :2 --latency").unwrap_err().contains("needs a value"));
        assert!(run("--listen :1 --target :2 --wobble 3").unwrap_err().contains("wobble"));
        assert!(run("--listen :1 --target :2 --loss 300%").is_err());
        assert!(run("stray").is_err());
    }

    #[test]
    fn help_and_version() {
        assert!(matches!(parse(["--help".to_string()]), Ok(Cli::Help)));
        assert!(matches!(parse(["--version".to_string()]), Ok(Cli::Version)));
    }
}
