//! TCP forwarding. Each accepted connection is paired with one upstream
//! connection and pumped in both directions through the shaper.
//!
//! In normal mode the byte stream is kept intact: chunks never overtake each
//! other and loss becomes a retransmit delay. `--tcp-strict` lifts that and
//! lets drops, duplicates, corruption and reordering hit the raw stream.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::conditions::{Config, Direction};
use crate::packet_log::PacketLog;
use crate::scheduler::Scheduler;
use crate::shaper::Shaper;
use crate::stats::Stats;

const READ_CHUNK: usize = 16 * 1024;

/// A chunk plus the writer it belongs to. An empty chunk closes the writer.
type Chunk = (Vec<u8>, mpsc::Sender<Vec<u8>>);

pub struct TcpProxy {
    listener: TcpListener,
    target: SocketAddr,
    config: Arc<RwLock<Config>>,
    stats: Arc<Stats>,
    log: Option<Arc<PacketLog>>,
    strict: bool,
}

impl TcpProxy {
    pub fn bind(
        listen: SocketAddr,
        target: SocketAddr,
        config: Arc<RwLock<Config>>,
        stats: Arc<Stats>,
        log: Option<Arc<PacketLog>>,
        strict: bool,
    ) -> io::Result<Self> {
        Ok(Self { listener: TcpListener::bind(listen)?, target, config, stats, log, strict })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr().unwrap()
    }

    /// Blocks forever, accepting connections.
    pub fn run(self) -> io::Result<()> {
        let scheduler = Scheduler::start(self.stats.clone(), |(data, tx): Chunk| {
            let _ = tx.send(data);
        });
        let shaper = |dir| {
            Arc::new(Mutex::new(Shaper::new(
                dir,
                self.config.clone(),
                self.stats.clone(),
                self.log.clone(),
                !self.strict,
            )))
        };
        let (up, down) = (shaper(Direction::Up), shaper(Direction::Down));

        for client in self.listener.incoming() {
            let Ok(client) = client else {
                thread::sleep(Duration::from_millis(10)); // e.g. out of file descriptors; don't spin
                continue;
            };
            let upstream = match TcpStream::connect(self.target) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("lagproxy: connect to {} failed: {e}", self.target);
                    continue;
                }
            };
            let _ = client.set_nodelay(true);
            let _ = upstream.set_nodelay(true);
            let (Ok(client2), Ok(upstream2)) = (client.try_clone(), upstream.try_clone()) else {
                eprintln!("lagproxy: cannot clone sockets for new connection");
                continue;
            };
            pump(client, upstream, Direction::Up, up.clone(), scheduler.clone(), self.strict);
            pump(upstream2, client2, Direction::Down, down.clone(), scheduler.clone(), self.strict);
        }
        Ok(())
    }
}

/// Reads chunks from `from`, shapes them, and writes them to `to` once released.
fn pump(
    mut from: TcpStream,
    mut to: TcpStream,
    dir: Direction,
    shaper: Arc<Mutex<Shaper>>,
    scheduler: Scheduler<Chunk>,
    strict: bool,
) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    // Writer: runs off the timer thread so a slow peer cannot stall other packets.
    let from_for_writer = from.try_clone().ok();
    thread::spawn(move || {
        for chunk in rx {
            if chunk.is_empty() || to.write_all(&chunk).is_err() {
                break;
            }
        }
        let _ = to.shutdown(Shutdown::Write);
        // Unblock the reader if the other side went away first.
        if let Some(f) = from_for_writer {
            let _ = f.shutdown(Shutdown::Read);
        }
    });

    thread::spawn(move || {
        let mut buf = vec![0u8; READ_CHUNK];
        let mut last_release = Instant::now();
        loop {
            let n = match from.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => 0,
            };
            if n == 0 {
                // Deliver the close after everything already queued.
                scheduler.schedule_at(dir, last_release, false, (Vec::new(), tx.clone()));
                break;
            }
            let (deliveries, pause) = {
                let mut shaper = shaper.lock().unwrap();
                (shaper.shape(buf[..n].to_vec()), shaper.backpressure())
            };
            for d in deliveries {
                let mut at = Instant::now() + d.delay;
                if !strict {
                    at = at.max(last_release); // jitter must not reorder a byte stream
                }
                last_release = at;
                scheduler.schedule_at(dir, at, d.hold, (d.data, tx.clone()));
            }
            thread::sleep(pause); // stall the sender while the bandwidth queue is full
        }
    });
}
