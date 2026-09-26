use std::net::TcpListener;
use std::process::exit;
use std::sync::{Arc, RwLock};
use std::thread;

use lagproxy::cli::{self, Cli};
use lagproxy::packet_log::PacketLog;
use lagproxy::{api, scenario, tcp, tui, udp, Stats};

fn main() {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(Cli::Run(args)) => args,
        Ok(Cli::Help) => {
            print!("{}\n{}", cli::USAGE, cli::preset_help());
            return;
        }
        Ok(Cli::Version) => {
            println!("lagproxy {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{}", cli::USAGE);
            exit(2);
        }
    };

    let config = Arc::new(RwLock::new(args.config.clone()));
    let stats = Arc::new(Stats::new());
    let log = args.log.as_deref().map(|p| match PacketLog::open(p) {
        Ok(l) => Arc::new(l),
        Err(e) => fail(&format!("cannot open log {}: {e}", p.display())),
    });

    let shutdown = {
        let (stats, log) = (stats.clone(), log.clone());
        move || {
            tui::restore();
            if let Some(log) = &log {
                log.flush();
            }
            eprintln!("\n{}", stats.table());
            exit(0);
        }
    };
    ctrlc::set_handler(shutdown.clone()).expect("ctrl-c handler");

    let api = args.api.and_then(|addr| match TcpListener::bind(addr) {
        Ok(listener) => {
            let (config, stats) = (config.clone(), stats.clone());
            thread::spawn(move || api::serve(listener, config, stats));
            Some(addr)
        }
        Err(e) => {
            eprintln!("lagproxy: api disabled, cannot bind {addr}: {e}");
            None
        }
    });

    if let Some(path) = &args.scenario {
        let steps = scenario::load(path).unwrap_or_else(|e| fail(&e));
        let (config, shutdown, looping) = (config.clone(), shutdown.clone(), args.loop_scenario);
        thread::spawn(move || {
            if scenario::run(&steps, &config, looping) {
                shutdown();
            }
        });
    }

    let proto = if args.tcp_strict { "tcp-strict" } else if args.tcp { "tcp" } else { "udp" };
    eprintln!("lagproxy: {proto} {} -> {}", args.listen, args.target);
    if let Some(addr) = api {
        eprintln!("lagproxy: api on http://{addr}");
    }
    if args.tcp {
        let proxy = tcp::TcpProxy::bind(args.listen, args.target, config.clone(), stats.clone(), log, args.tcp_strict)
            .unwrap_or_else(|e| fail(&format!("cannot listen on {}: {e}", args.listen)));
        thread::spawn(move || proxy.run().unwrap_or_else(|e| fail(&format!("proxy failed: {e}"))));
    } else {
        let proxy = udp::UdpProxy::bind(args.listen, args.target, config.clone(), stats.clone(), log)
            .unwrap_or_else(|e| fail(&format!("cannot listen on {}: {e}", args.listen)));
        thread::spawn(move || proxy.run().unwrap_or_else(|e| fail(&format!("proxy failed: {e}"))));
    }

    if args.tui {
        tui::run(config, stats).unwrap_or_else(|e| fail(&format!("tui: {e}")));
        shutdown();
    }
    loop {
        thread::park();
    }
}

fn fail(msg: &str) -> ! {
    tui::restore();
    eprintln!("error: {msg}");
    exit(1);
}
