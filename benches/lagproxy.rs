//! `cargo bench` - how much work lagproxy does per packet, and how much it
//! costs to put it in the path at all.
//!
//! - `shaper/*`     the per-packet decision chain in isolation
//! - `scheduler/*`  push + timer thread + delivery for a batch of packets
//! - `udp_round_trip/*` and `tcp_stream/*` real sockets on loopback, with and
//!   without the proxy in between and no conditions applied

use std::hint::black_box;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{mpsc, Arc, RwLock};
use std::thread;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use rand::rngs::StdRng;
use rand::SeedableRng;

use lagproxy::scheduler::Scheduler;
use lagproxy::shaper::Shaper;
use lagproxy::tcp::TcpProxy;
use lagproxy::udp::UdpProxy;
use lagproxy::{Config, Direction, Stats};

/// A typical game datagram.
const PACKET: usize = 1200;
const LOCAL: &str = "127.0.0.1:0";

fn config(settings: &[(&str, &str)]) -> Arc<RwLock<Config>> {
    let mut c = Config::default();
    for (k, v) in settings {
        c.set(k, v).unwrap();
    }
    Arc::new(RwLock::new(c))
}

fn bench_shaper(c: &mut Criterion) {
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("passthrough", &[]),
        ("latency", &[("latency", "50ms")]),
        ("jitter/uniform", &[("latency", "50ms"), ("jitter", "20ms")]),
        ("jitter/normal", &[("latency", "50ms"), ("jitter", "20ms"), ("jitter-dist", "normal")]),
        ("jitter/pareto", &[("latency", "50ms"), ("jitter", "20ms"), ("jitter-dist", "pareto")]),
        // Fast enough that the virtual link never backs up, so no packet takes the drop path.
        ("bandwidth", &[("bandwidth", "100gbit"), ("queue", "1gb")]),
        ("preset/wifi-bad", &[("preset", "wifi-bad")]),
        ("preset/hostile", &[("preset", "hostile")]),
    ];
    let mut g = c.benchmark_group("shaper");
    g.throughput(Throughput::Elements(1));
    let packet = vec![0xABu8; PACKET];
    for (name, settings) in cases {
        let mut shaper = Shaper::with_rng(
            Direction::Up,
            config(settings),
            Arc::new(Stats::new()),
            None,
            false,
            StdRng::seed_from_u64(1),
        );
        g.bench_function(*name, |b| b.iter(|| black_box(shaper.shape(packet.clone()))));
    }
    g.finish();
}

fn bench_scheduler(c: &mut Criterion) {
    const BATCH: u64 = 1000;
    let (tx, rx) = mpsc::channel::<u64>();
    let scheduler = Scheduler::start(Arc::new(Stats::new()), move |n| {
        let _ = tx.send(n);
    });
    let mut g = c.benchmark_group("scheduler");
    g.throughput(Throughput::Elements(BATCH));
    g.bench_function("schedule_and_deliver", |b| {
        b.iter(|| {
            for i in 0..BATCH {
                scheduler.schedule(Direction::Up, Duration::ZERO, false, i);
            }
            for _ in 0..BATCH {
                rx.recv().unwrap();
            }
        })
    });
    g.finish();
}

fn udp_echo_server() -> SocketAddr {
    let socket = UdpSocket::bind(LOCAL).unwrap();
    let addr = socket.local_addr().unwrap();
    thread::spawn(move || {
        let mut buf = [0u8; 65535];
        while let Ok((n, from)) = socket.recv_from(&mut buf) {
            let _ = socket.send_to(&buf[..n], from);
        }
    });
    addr
}

fn bench_udp(c: &mut Criterion) {
    let server = udp_echo_server();
    let proxy = UdpProxy::bind(LOCAL.parse().unwrap(), server, config(&[]), Arc::new(Stats::new()), None).unwrap();
    let proxy_addr = proxy.local_addr();
    thread::spawn(move || proxy.run());

    let client = UdpSocket::bind(LOCAL).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let packet = vec![0u8; PACKET];
    let mut buf = vec![0u8; PACKET];

    let mut g = c.benchmark_group("udp_round_trip");
    g.throughput(Throughput::Elements(1));
    for (name, target) in [("direct", server), ("through_proxy", proxy_addr)] {
        g.bench_function(name, |b| {
            b.iter(|| {
                client.send_to(&packet, target).unwrap();
                client.recv_from(&mut buf).unwrap()
            })
        });
    }
    g.finish();
}

/// Reads `chunk` bytes per request and answers with a single byte.
fn tcp_sink_server(chunk: usize) -> SocketAddr {
    let listener = TcpListener::bind(LOCAL).unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
                let mut buf = vec![0u8; chunk];
                while stream.read_exact(&mut buf).is_ok() && stream.write_all(&[1]).is_ok() {}
            });
        }
    });
    addr
}

fn bench_tcp(c: &mut Criterion) {
    const BYTES: usize = 4 << 20;
    let server = tcp_sink_server(BYTES);
    let proxy =
        TcpProxy::bind(LOCAL.parse().unwrap(), server, config(&[]), Arc::new(Stats::new()), None, false).unwrap();
    let proxy_addr = proxy.local_addr();
    thread::spawn(move || proxy.run());

    let payload = vec![0x5Au8; BYTES];
    let mut g = c.benchmark_group("tcp_stream");
    g.throughput(Throughput::Bytes(BYTES as u64));
    g.sample_size(20);
    for (name, target) in [("direct", server), ("through_proxy", proxy_addr)] {
        let mut stream = TcpStream::connect(target).unwrap();
        stream.set_nodelay(true).unwrap();
        g.bench_function(name, |b| {
            b.iter(|| {
                stream.write_all(&payload).unwrap();
                let mut ack = [0u8; 1];
                stream.read_exact(&mut ack).unwrap();
            })
        });
    }
    g.finish();
}

criterion_group!(benches, bench_shaper, bench_scheduler, bench_udp, bench_tcp);
criterion_main!(benches);
