//! End-to-end tests: real sockets, real echo servers, a real proxy in a thread.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use lagproxy::packet_log::PacketLog;
use lagproxy::tcp::TcpProxy;
use lagproxy::udp::UdpProxy;
use lagproxy::{api, Config, Stats};

const LOCAL: &str = "127.0.0.1:0";

fn config(settings: &[(&str, &str)]) -> Arc<RwLock<Config>> {
    let mut c = Config::default();
    for (k, v) in settings {
        c.set(k, v).unwrap();
    }
    Arc::new(RwLock::new(c))
}

fn udp_echo_server() -> SocketAddr {
    let sock = UdpSocket::bind(LOCAL).unwrap();
    let addr = sock.local_addr().unwrap();
    thread::spawn(move || {
        let mut buf = [0u8; 65535];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            let _ = sock.send_to(&buf[..n], from);
        }
    });
    addr
}

fn tcp_echo_server() -> SocketAddr {
    let listener = TcpListener::bind(LOCAL).unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut reader = stream.try_clone().unwrap();
                let mut writer = stream;
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 || writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

fn udp_proxy(target: SocketAddr, config: Arc<RwLock<Config>>) -> (SocketAddr, Arc<Stats>) {
    let stats = Arc::new(Stats::new());
    let proxy = UdpProxy::bind(LOCAL.parse().unwrap(), target, config, stats.clone(), None).unwrap();
    let addr = proxy.local_addr();
    thread::spawn(move || proxy.run());
    (addr, stats)
}

fn tcp_proxy(target: SocketAddr, config: Arc<RwLock<Config>>, strict: bool) -> (SocketAddr, Arc<Stats>) {
    let stats = Arc::new(Stats::new());
    let proxy = TcpProxy::bind(LOCAL.parse().unwrap(), target, config, stats.clone(), None, strict).unwrap();
    let addr = proxy.local_addr();
    thread::spawn(move || proxy.run());
    (addr, stats)
}

fn udp_client(timeout: Duration) -> UdpSocket {
    let sock = UdpSocket::bind(LOCAL).unwrap();
    sock.set_read_timeout(Some(timeout)).unwrap();
    sock
}

#[test]
fn udp_round_trip_with_latency() {
    let server = udp_echo_server();
    let (proxy, stats) = udp_proxy(server, config(&[("latency", "50ms")]));
    let client = udp_client(Duration::from_secs(2));

    let start = Instant::now();
    client.send_to(b"ping", proxy).unwrap();
    let mut buf = [0u8; 16];
    let (n, from) = client.recv_from(&mut buf).unwrap();
    let rtt = start.elapsed();

    assert_eq!(&buf[..n], b"ping");
    assert_eq!(from, proxy);
    assert!(rtt >= Duration::from_millis(100), "rtt {rtt:?} shorter than 2 x 50ms");
    assert!(rtt < Duration::from_millis(500), "rtt {rtt:?} unreasonably long");
    assert_eq!(stats.up.packets.load(Relaxed), 1);
    assert_eq!(stats.down.packets.load(Relaxed), 1);
}

#[test]
fn udp_per_direction_conditions_and_live_change() {
    let server = udp_echo_server();
    let cfg = config(&[("up:loss", "100%")]);
    let (proxy, _) = udp_proxy(server, cfg.clone());
    let client = udp_client(Duration::from_millis(300));
    let mut buf = [0u8; 16];

    client.send_to(b"lost", proxy).unwrap();
    assert!(client.recv_from(&mut buf).is_err(), "packet should have been dropped");

    cfg.write().unwrap().set("up:loss", "0%").unwrap();
    client.send_to(b"ok", proxy).unwrap();
    let (n, _) = client.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ok");
}

#[test]
fn udp_handles_many_clients_and_keeps_them_apart() {
    let server = udp_echo_server();
    let (proxy, _) = udp_proxy(server, config(&[]));
    let clients: Vec<UdpSocket> = (0..5).map(|_| udp_client(Duration::from_secs(2))).collect();
    for (i, c) in clients.iter().enumerate() {
        c.send_to(&[i as u8], proxy).unwrap();
    }
    for (i, c) in clients.iter().enumerate() {
        let mut buf = [0u8; 4];
        let (n, _) = c.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &[i as u8]);
    }
}

#[test]
fn udp_duplication_delivers_two_copies() {
    let server = udp_echo_server();
    let (proxy, _) = udp_proxy(server, config(&[("up:dup", "100%")]));
    let client = udp_client(Duration::from_secs(1));
    client.send_to(b"twice", proxy).unwrap();
    let mut buf = [0u8; 16];
    assert!(client.recv_from(&mut buf).is_ok());
    assert!(client.recv_from(&mut buf).is_ok());
    assert!(client.recv_from(&mut buf).is_err());
}

#[test]
fn udp_corruption_changes_the_payload() {
    let server = udp_echo_server();
    let (proxy, stats) = udp_proxy(server, config(&[("up:corrupt", "100%")]));
    let client = udp_client(Duration::from_secs(1));
    client.send_to(&[0u8; 32], proxy).unwrap();
    let mut buf = [0xFFu8; 32];
    let (n, _) = client.recv_from(&mut buf).unwrap();
    assert_eq!(n, 32);
    let flipped: u32 = buf.iter().map(|b| b.count_ones()).sum();
    assert_eq!(flipped, 1, "exactly one bit should be flipped");
    assert_eq!(stats.up.corrupted.load(Relaxed), 1);
}

#[test]
fn udp_reordering_swaps_a_held_packet_with_the_next_one() {
    let server = udp_echo_server();
    let cfg = config(&[("up:reorder", "100%")]);
    let (proxy, stats) = udp_proxy(server, cfg.clone());
    let client = udp_client(Duration::from_secs(1));

    client.send_to(b"first", proxy).unwrap();
    thread::sleep(Duration::from_millis(20));
    cfg.write().unwrap().set("up:reorder", "0%").unwrap();
    client.send_to(b"second", proxy).unwrap();

    let mut buf = [0u8; 16];
    let (n, _) = client.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"second");
    let (n, _) = client.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"first");
    assert_eq!(stats.up.reordered.load(Relaxed), 1);
}

#[test]
fn udp_bandwidth_queues_then_drops_a_burst() {
    // 80 kbit/s = 10 KB/s: a 1000 byte packet occupies the link for 100ms.
    // Queue of 2000 bytes fits two packets; the rest of the burst is dropped.
    let server = udp_echo_server();
    let (proxy, stats) = udp_proxy(server, config(&[("up:bandwidth", "80kbit"), ("up:queue", "2000")]));
    let client = udp_client(Duration::from_millis(500));
    let start = Instant::now();
    for i in 0..10u8 {
        client.send_to(&[i; 1000], proxy).unwrap();
    }
    let mut got = Vec::new();
    let mut buf = [0u8; 1000];
    while let Ok((n, _)) = client.recv_from(&mut buf) {
        assert_eq!(n, 1000);
        got.push((buf[0], start.elapsed()));
    }
    assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![0, 1], "{got:?}");
    assert!(got[1].1 >= Duration::from_millis(190), "{got:?}");
    assert_eq!(stats.up.dropped.load(Relaxed), 8);
    assert!(stats.up.max_queue.load(Relaxed) >= 1900);
}

#[test]
fn udp_survives_an_unreachable_target() {
    let dead: SocketAddr = {
        let s = UdpSocket::bind(LOCAL).unwrap();
        s.local_addr().unwrap()
    }; // socket dropped, port now closed
    let (proxy, stats) = udp_proxy(dead, config(&[]));
    let client = udp_client(Duration::from_millis(200));
    let mut buf = [0u8; 8];
    for _ in 0..3 {
        client.send_to(b"x", proxy).unwrap();
        let _ = client.recv_from(&mut buf);
    }
    // ICMP port-unreachable errors must not kill the proxy loop.
    assert_eq!(stats.up.packets.load(Relaxed), 3);
}

#[test]
fn udp_stats_and_packet_log_end_to_end() {
    let server = udp_echo_server();
    let path = std::env::temp_dir().join(format!("lagproxy-e2e-{}.jsonl", std::process::id()));
    let log = Arc::new(PacketLog::open(&path).unwrap());
    let stats = Arc::new(Stats::new());
    let proxy = UdpProxy::bind(LOCAL.parse().unwrap(), server, config(&[("latency", "10ms")]), stats.clone(), Some(log.clone())).unwrap();
    let addr = proxy.local_addr();
    thread::spawn(move || proxy.run());

    let client = udp_client(Duration::from_secs(1));
    let mut buf = [0u8; 8];
    for _ in 0..3 {
        client.send_to(b"abc", addr).unwrap();
        client.recv_from(&mut buf).unwrap();
    }
    log.flush();
    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 6);
    assert_eq!(lines.iter().filter(|l| l["dir"] == "up").count(), 3);
    assert!(lines.iter().all(|l| l["action"] == "forward" && l["len"] == 3 && l["delay_ms"] == 10.0));

    let table = stats.table();
    assert!(table.contains("up          3"), "{table}");
    assert!(table.contains("down        3"), "{table}");
    let json = stats.to_json();
    assert_eq!(json["up"]["bytes"], 9);
    assert_eq!(json["down"]["avg_delay_ms"], 10.0);
}

#[test]
fn tcp_stream_survives_jitter_loss_and_bandwidth() {
    let server = tcp_echo_server();
    let (proxy, stats) = tcp_proxy(
        server,
        config(&[("latency", "5ms"), ("jitter", "20ms"), ("loss", "20%"), ("bandwidth", "20mbit")]),
        false,
    );

    let payload: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut stream = TcpStream::connect(proxy).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let sent = payload.clone();
    thread::spawn(move || {
        writer.write_all(&sent).unwrap();
        writer.shutdown(std::net::Shutdown::Write).unwrap();
    });

    let mut got = Vec::new();
    stream.read_to_end(&mut got).unwrap();
    assert_eq!(got.len(), payload.len());
    assert!(got == payload, "stream was corrupted or reordered");
    assert!(stats.up.dropped.load(Relaxed) > 0, "expected some simulated loss");
}

#[test]
fn tcp_multiple_connections_stay_separate() {
    let server = tcp_echo_server();
    let (proxy, _) = tcp_proxy(server, config(&[("latency", "5ms")]), false);
    let handles: Vec<_> = (0..4u8)
        .map(|i| {
            thread::spawn(move || {
                let mut s = TcpStream::connect(proxy).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let payload = vec![i; 5000];
                s.write_all(&payload).unwrap();
                let mut got = vec![0u8; 5000];
                s.read_exact(&mut got).unwrap();
                assert_eq!(got, payload);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn tcp_close_propagates_through_the_proxy() {
    let server = tcp_echo_server();
    let (proxy, _) = tcp_proxy(server, config(&[("latency", "5ms")]), false);
    let mut s = TcpStream::connect(proxy).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"bye").unwrap();
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut got = Vec::new();
    s.read_to_end(&mut got).unwrap(); // returns only once the echo server's FIN arrives
    assert_eq!(got, b"bye");
}

#[test]
fn tcp_survives_an_unreachable_target() {
    let dead: SocketAddr = {
        let l = TcpListener::bind(LOCAL).unwrap();
        l.local_addr().unwrap()
    };
    let (proxy, _) = tcp_proxy(dead, config(&[]), false);
    for _ in 0..2 {
        let mut s = TcpStream::connect(proxy).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 4];
        // The proxy closes the client because it cannot reach the target, but keeps accepting.
        assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)));
    }
}

#[test]
fn tcp_strict_mode_really_drops_bytes() {
    let server = tcp_echo_server();
    let (proxy, _) = tcp_proxy(server, config(&[("up:loss", "100%")]), true);
    let mut stream = TcpStream::connect(proxy).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    stream.write_all(b"never arrives").unwrap();
    let mut buf = [0u8; 32];
    assert!(stream.read(&mut buf).is_err());
}

#[test]
fn tcp_latency_is_applied() {
    let server = tcp_echo_server();
    let (proxy, _) = tcp_proxy(server, config(&[("latency", "40ms")]), false);
    let mut stream = TcpStream::connect(proxy).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let start = Instant::now();
    stream.write_all(b"hi").unwrap();
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hi");
    assert!(start.elapsed() >= Duration::from_millis(80));
}

fn http(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp.split_whitespace().nth(1).unwrap().parse().unwrap();
    let json = serde_json::from_str(resp.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    (status, json)
}

#[test]
fn api_set_get_and_stats() {
    let cfg = config(&[]);
    let stats = Arc::new(Stats::new());
    let listener = TcpListener::bind(LOCAL).unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn({
        let (cfg, stats) = (cfg.clone(), stats.clone());
        move || api::serve(listener, cfg, stats)
    });

    let (status, body) = http(addr, "POST", "/set", r#"{"latency":"200ms","up:loss":"5%"}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["config"]["up"]["latency"], "200ms");
    assert_eq!(body["config"]["up"]["loss"], "5%");
    assert_eq!(body["config"]["down"]["loss"], "0%");
    assert_eq!(cfg.read().unwrap().down.latency, Duration::from_millis(200));

    let (status, body) = http(addr, "GET", "/config", "");
    assert_eq!(status, 200);
    assert_eq!(body["up"]["latency"], "200ms");

    let (status, body) = http(addr, "GET", "/stats", "");
    assert_eq!(status, 200);
    assert!(body["up"]["packets"].is_number());

    let (status, body) = http(addr, "POST", "/set", r#"{"loss":"lots"}"#);
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("lots"));

    let (status, _) = http(addr, "GET", "/nope", "");
    assert_eq!(status, 404);

    // Oversized bodies are refused up front instead of being read.
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "POST /set HTTP/1.1\r\nHost: x\r\nContent-Length: 99999999\r\n\r\n").unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");

    // A client that connects and says nothing must not block other requests.
    let _idle = TcpStream::connect(addr).unwrap();
    let (status, _) = http(addr, "GET", "/stats", "");
    assert_eq!(status, 200);
}
