//! UDP forwarding. Every client address gets its own upstream socket so the
//! server sees one "connection" per client and replies can be routed back.

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::conditions::{Config, Direction};
use crate::packet_log::PacketLog;
use crate::scheduler::Scheduler;
use crate::shaper::Shaper;
use crate::stats::Stats;

/// Upstream sockets are closed after this long without traffic in either direction;
/// the next packet from the client recreates them.
pub const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

const MAX_DATAGRAM: usize = 65535;

enum Route {
    ToTarget(Arc<UdpSocket>),
    ToClient(SocketAddr),
}

struct Packet {
    data: Vec<u8>,
    route: Route,
}

struct Client {
    upstream: Arc<UdpSocket>,
    /// Last packet from the client. Only touched while holding the map lock, so the
    /// downstream pump cannot expire a client that is about to be used.
    last_seen: Instant,
}

type Clients = Arc<Mutex<HashMap<SocketAddr, Client>>>;

pub struct UdpProxy {
    socket: Arc<UdpSocket>,
    target: SocketAddr,
    config: Arc<RwLock<Config>>,
    stats: Arc<Stats>,
    log: Option<Arc<PacketLog>>,
}

impl UdpProxy {
    pub fn bind(
        listen: SocketAddr,
        target: SocketAddr,
        config: Arc<RwLock<Config>>,
        stats: Arc<Stats>,
        log: Option<Arc<PacketLog>>,
    ) -> io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind(listen)?);
        Ok(Self { socket, target, config, stats, log })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }

    /// Blocks forever, forwarding traffic.
    pub fn run(self) -> io::Result<()> {
        let listener = self.socket.clone();
        let scheduler = Scheduler::start(self.stats.clone(), move |p: Packet| {
            let _ = match &p.route {
                Route::ToTarget(upstream) => upstream.send(&p.data),
                Route::ToClient(client) => listener.send_to(&p.data, client),
            };
        });
        let mut up = Shaper::new(Direction::Up, self.config.clone(), self.stats.clone(), self.log.clone(), false);
        let down = Arc::new(Mutex::new(Shaper::new(
            Direction::Down,
            self.config.clone(),
            self.stats.clone(),
            self.log.clone(),
            false,
        )));
        let clients: Clients = Default::default();

        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            let (n, client) = match self.socket.recv_from(&mut buf) {
                Ok(x) => x,
                Err(e) if is_transient(&e) => continue,
                Err(e) => return Err(e),
            };
            let upstream = {
                let mut map = clients.lock().unwrap();
                match map.get_mut(&client) {
                    Some(c) => {
                        c.last_seen = Instant::now();
                        c.upstream.clone()
                    }
                    None => {
                        let upstream = match self.connect_upstream() {
                            Ok(s) => Arc::new(s),
                            Err(e) => {
                                eprintln!("lagproxy: cannot open upstream socket for {client}: {e}");
                                continue;
                            }
                        };
                        map.insert(client, Client { upstream: upstream.clone(), last_seen: Instant::now() });
                        thread::spawn({
                            let (upstream, down, sched, clients) =
                                (upstream.clone(), down.clone(), scheduler.clone(), clients.clone());
                            move || pump_down(upstream, client, down, sched, clients)
                        });
                        upstream
                    }
                }
            };
            for d in up.shape(buf[..n].to_vec()) {
                let route = Route::ToTarget(upstream.clone());
                scheduler.schedule(Direction::Up, d.delay, d.hold, Packet { data: d.data, route });
            }
        }
    }

    fn connect_upstream(&self) -> io::Result<UdpSocket> {
        let any: SocketAddr = if self.target.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse().unwrap();
        let socket = UdpSocket::bind(any)?;
        socket.connect(self.target)?;
        socket.set_read_timeout(Some(CLIENT_IDLE_TIMEOUT))?;
        Ok(socket)
    }
}

/// Reads server replies for one client and schedules them back to it.
fn pump_down(
    upstream: Arc<UdpSocket>,
    client: SocketAddr,
    down: Arc<Mutex<Shaper>>,
    scheduler: Scheduler<Packet>,
    clients: Clients,
) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        let n = match upstream.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if is_transient(&e) => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                // No replies for a while; expire the client unless it is still sending.
                let mut map = clients.lock().unwrap();
                let idle = map.get(&client).is_none_or(|c| c.last_seen.elapsed() >= CLIENT_IDLE_TIMEOUT);
                if !idle {
                    continue;
                }
                map.remove(&client);
                break;
            }
            Err(_) => {
                clients.lock().unwrap().remove(&client);
                break;
            }
        };
        for d in down.lock().unwrap().shape(buf[..n].to_vec()) {
            let route = Route::ToClient(client);
            scheduler.schedule(Direction::Down, d.delay, d.hold, Packet { data: d.data, route });
        }
    }
}

/// Errors a UDP socket reports that say nothing about the socket itself:
/// ICMP "port unreachable" from an earlier send (Windows: reset, Linux: refused) and signals.
fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionReset | ErrorKind::ConnectionRefused | ErrorKind::Interrupted
    )
}
