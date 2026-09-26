//! The condition chain. One `Shaper` per direction decides, for every incoming
//! packet, whether it is dropped, duplicated, corrupted, reordered, throttled
//! and how long it is delayed. It does not touch sockets or timers.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Normal, Pareto};

use crate::conditions::{Conditions, Config, Direction, JitterDist};
use crate::packet_log::{PacketLog, Record};
use crate::stats::Stats;

/// Stand-in for a TCP retransmission when a chunk would have been lost in stream mode.
pub const RETRANSMIT_DELAY: Duration = Duration::from_millis(200);

/// What to do with (one copy of) a packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub data: Vec<u8>,
    pub delay: Duration,
    /// Hold this packet back until the next one has been scheduled.
    pub hold: bool,
}

pub struct Shaper {
    dir: Direction,
    config: Arc<RwLock<Config>>,
    stats: Arc<Stats>,
    log: Option<Arc<PacketLog>>,
    /// Stream mode (plain TCP): never damage the byte stream. Loss becomes a
    /// retransmit delay and dup/corrupt/reorder are ignored.
    stream: bool,
    rng: StdRng,
    /// Whether the previous packet was lost, for `loss_burst`.
    last_dropped: bool,
    /// Bandwidth model: the moment the virtual link finishes sending everything queued so far.
    line_free_at: Instant,
}

impl Shaper {
    pub fn new(
        dir: Direction,
        config: Arc<RwLock<Config>>,
        stats: Arc<Stats>,
        log: Option<Arc<PacketLog>>,
        stream: bool,
    ) -> Self {
        Self::with_rng(dir, config, stats, log, stream, StdRng::from_entropy())
    }

    pub fn with_rng(
        dir: Direction,
        config: Arc<RwLock<Config>>,
        stats: Arc<Stats>,
        log: Option<Arc<PacketLog>>,
        stream: bool,
        rng: StdRng,
    ) -> Self {
        Self {
            dir,
            config,
            stats,
            log,
            stream,
            rng,
            last_dropped: false,
            line_free_at: Instant::now(),
        }
    }

    /// Runs one incoming packet through the chain. An empty result means it was dropped.
    pub fn shape(&mut self, data: Vec<u8>) -> Vec<Delivery> {
        let c = self.config.read().unwrap().dir(self.dir).clone();
        let stats = self.stats.clone();
        let stats = stats.dir(self.dir);
        let len = data.len();
        stats.packets.fetch_add(1, Relaxed);
        stats.bytes.fetch_add(len as u64, Relaxed);

        let mut extra = Duration::ZERO;

        // Loss, with optional bursts: after a drop the next packet uses loss_burst instead.
        let p_drop = if self.last_dropped && c.loss_burst > 0.0 { c.loss_burst } else { c.loss };
        let dropped = self.chance(p_drop);
        self.last_dropped = dropped;
        if dropped {
            stats.dropped.fetch_add(1, Relaxed);
            if self.stream {
                extra += RETRANSMIT_DELAY;
            } else {
                self.log(len, "drop", Duration::ZERO, false, false, false);
                return Vec::new();
            }
        }

        // Bandwidth: wait for the virtual link, or drop if the queue is full.
        match self.throttle(len, &c) {
            Some(wait) => extra += wait,
            None => {
                stats.dropped.fetch_add(1, Relaxed);
                self.log(len, "queue-full", Duration::ZERO, false, false, false);
                return Vec::new();
            }
        }

        let dup = !self.stream && self.chance(c.dup);
        let corrupt = !self.stream && len > 0 && self.chance(c.corrupt);
        let hold = !self.stream && self.chance(c.reorder);

        let mut data = data;
        if corrupt {
            stats.corrupted.fetch_add(1, Relaxed);
            let i = self.rng.gen_range(0..len);
            data[i] ^= 1 << self.rng.gen_range(0..8);
        }
        if hold {
            stats.reordered.fetch_add(1, Relaxed);
        }

        let delay = extra + self.delay(&c);
        stats.record_delay(delay);
        self.log(len, "forward", delay, dup, corrupt, hold);

        let mut out = vec![Delivery { data, delay, hold }];
        if dup {
            stats.dup.fetch_add(1, Relaxed);
            let delay = extra + self.delay(&c);
            stats.record_delay(delay);
            out.push(Delivery { data: out[0].data.clone(), delay, hold: false });
        }
        out
    }

    fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && self.rng.gen::<f64>() < p
    }

    /// Latency plus a jitter sample. Jitter is always added on top, never subtracted.
    fn delay(&mut self, c: &Conditions) -> Duration {
        let j = c.jitter.as_secs_f64();
        if j <= 0.0 {
            return c.latency;
        }
        let sample = match c.jitter_dist {
            JitterDist::Uniform => self.rng.gen_range(0.0..=j),
            // Bell curve centred in the jitter window, clipped to it.
            JitterDist::Normal => Normal::new(j / 2.0, j / 4.0)
                .unwrap()
                .sample(&mut self.rng)
                .clamp(0.0, j),
            // Heavy tail: mostly small, occasionally several times the jitter value.
            JitterDist::Pareto => (Pareto::new(j / 4.0, 2.0).unwrap().sample(&mut self.rng) - j / 4.0)
                .clamp(0.0, j * 10.0),
        };
        c.latency + Duration::from_secs_f64(sample)
    }

    /// Returns how long the packet waits for bandwidth, or `None` if the queue is full.
    /// The link is modelled as a virtual clock: each packet occupies it for len/bandwidth
    /// seconds, and anything arriving while it is busy is queued behind.
    /// Stream mode never drops here; see `backpressure`.
    fn throttle(&mut self, len: usize, c: &Conditions) -> Option<Duration> {
        if c.bandwidth == 0 {
            return Some(Duration::ZERO);
        }
        let now = Instant::now();
        let bytes_per_sec = c.bandwidth as f64 / 8.0;
        let backlog = self.line_free_at.saturating_duration_since(now).as_secs_f64();
        let queued = (backlog * bytes_per_sec).round() as u64;
        if !self.stream && queued > 0 && queued + len as u64 > c.queue {
            return None;
        }
        self.stats.dir(self.dir).max_queue.fetch_max(queued + len as u64, Relaxed);
        let start = self.line_free_at.max(now);
        self.line_free_at = start + Duration::from_secs_f64(len as f64 / bytes_per_sec);
        Some(self.line_free_at - now)
    }

    /// Stream mode: how long the reader should stop reading so the bandwidth
    /// backlog stays within the queue, like a TCP receive window filling up.
    pub fn backpressure(&self) -> Duration {
        let c = self.config.read().unwrap().dir(self.dir).clone();
        if c.bandwidth == 0 {
            return Duration::ZERO;
        }
        let queue_time = Duration::from_secs_f64(c.queue as f64 * 8.0 / c.bandwidth as f64);
        self.line_free_at.saturating_duration_since(Instant::now()).saturating_sub(queue_time)
    }

    fn log(&self, len: usize, action: &str, delay: Duration, dup: bool, corrupt: bool, reorder: bool) {
        if let Some(log) = &self.log {
            log.write(&Record {
                t: self.stats.started.elapsed().as_secs_f64(),
                dir: self.dir.name(),
                len,
                action,
                delay_ms: delay.as_secs_f64() * 1000.0,
                dup,
                corrupt,
                reorder,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shaper(settings: &[(&str, &str)], stream: bool) -> Shaper {
        let mut config = Config::default();
        for (k, v) in settings {
            config.set(k, v).unwrap();
        }
        Shaper::with_rng(
            Direction::Up,
            Arc::new(RwLock::new(config)),
            Arc::new(Stats::new()),
            None,
            stream,
            StdRng::seed_from_u64(7),
        )
    }

    fn packet() -> Vec<u8> {
        vec![0xAB; 100]
    }

    #[test]
    fn passthrough_by_default() {
        let mut s = shaper(&[], false);
        let out = s.shape(packet());
        assert_eq!(out, vec![Delivery { data: packet(), delay: Duration::ZERO, hold: false }]);
    }

    #[test]
    fn latency_is_applied_exactly_without_jitter() {
        let mut s = shaper(&[("latency", "80ms")], false);
        assert_eq!(s.shape(packet())[0].delay, Duration::from_millis(80));
    }

    #[test]
    fn jitter_stays_within_window_for_every_distribution() {
        for dist in ["uniform", "normal", "pareto"] {
            let mut s = shaper(&[("latency", "50ms"), ("jitter", "20ms"), ("jitter-dist", dist)], false);
            let mut seen_extra = false;
            for _ in 0..500 {
                let d = s.shape(packet())[0].delay;
                assert!(d >= Duration::from_millis(50), "{dist}: {d:?}");
                assert!(d <= Duration::from_millis(50 + 200), "{dist}: {d:?}");
                seen_extra |= d > Duration::from_millis(50);
            }
            assert!(seen_extra, "{dist} never added jitter");
        }
    }

    #[test]
    fn full_loss_drops_everything() {
        let mut s = shaper(&[("loss", "100%")], false);
        for _ in 0..20 {
            assert!(s.shape(packet()).is_empty());
        }
        assert_eq!(s.stats.up.dropped.load(Relaxed), 20);
    }

    #[test]
    fn loss_burst_drops_the_packet_after_a_drop() {
        // 100% burst: once one packet is lost, all following ones are too.
        let mut s = shaper(&[("loss", "50%"), ("loss-burst", "100%")], false);
        let mut dropped_before = false;
        for _ in 0..50 {
            let dropped = s.shape(packet()).is_empty();
            if dropped_before {
                assert!(dropped);
            }
            dropped_before = dropped;
        }
        assert!(dropped_before);
    }

    #[test]
    fn stream_mode_turns_loss_into_delay() {
        let mut s = shaper(&[("loss", "100%"), ("latency", "10ms")], true);
        let out = s.shape(packet());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].delay, RETRANSMIT_DELAY + Duration::from_millis(10));
        assert_eq!(out[0].data, packet());
    }

    #[test]
    fn stream_mode_never_damages_data() {
        let mut s = shaper(&[("dup", "100%"), ("corrupt", "100%"), ("reorder", "100%")], true);
        let out = s.shape(packet());
        assert_eq!(out, vec![Delivery { data: packet(), delay: Duration::ZERO, hold: false }]);
    }

    #[test]
    fn dup_sends_two_copies() {
        let mut s = shaper(&[("dup", "100%")], false);
        let out = s.shape(packet());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].data, out[1].data);
        assert_eq!(s.stats.up.dup.load(Relaxed), 1);
    }

    #[test]
    fn corrupt_flips_exactly_one_bit() {
        let mut s = shaper(&[("corrupt", "100%")], false);
        let out = s.shape(packet());
        let flipped: u32 = out[0].data.iter().zip(packet()).map(|(a, b)| (a ^ b).count_ones()).sum();
        assert_eq!(flipped, 1);
    }

    #[test]
    fn empty_datagrams_are_forwarded_and_never_counted_as_corrupted() {
        let mut s = shaper(&[("corrupt", "100%")], false);
        let out = s.shape(Vec::new());
        assert_eq!(out.len(), 1);
        assert!(out[0].data.is_empty());
        assert_eq!(s.stats.up.corrupted.load(Relaxed), 0);
    }

    #[test]
    fn reorder_marks_packet_for_holding() {
        let mut s = shaper(&[("reorder", "100%")], false);
        assert!(s.shape(packet())[0].hold);
        assert_eq!(s.stats.up.reordered.load(Relaxed), 1);
    }

    #[test]
    fn bandwidth_queues_then_drops() {
        // 8 kbit/s = 1000 bytes/s, so each 100 byte packet takes 100ms on the line.
        let mut s = shaper(&[("bandwidth", "8kbit"), ("queue", "250")], false);
        let first = s.shape(packet())[0].delay;
        let second = s.shape(packet())[0].delay;
        assert!(first >= Duration::from_millis(99) && first < Duration::from_millis(110), "{first:?}");
        assert!(second >= Duration::from_millis(199) && second < Duration::from_millis(210), "{second:?}");
        // 200 bytes are already queued, a third would exceed the 250 byte queue.
        assert!(s.shape(packet()).is_empty());
        assert_eq!(s.stats.up.dropped.load(Relaxed), 1);
        let max_queue = s.stats.up.max_queue.load(Relaxed);
        assert!((199..=200).contains(&max_queue), "{max_queue}");
    }

    #[test]
    fn stream_mode_applies_backpressure_instead_of_dropping() {
        let mut s = shaper(&[("bandwidth", "8kbit"), ("queue", "250")], true);
        for _ in 0..4 {
            assert_eq!(s.shape(packet()).len(), 1);
        }
        assert_eq!(s.stats.up.dropped.load(Relaxed), 0);
        // 400 bytes queued on a 250 byte queue at 1000 bytes/s: wait ~150ms.
        let wait = s.backpressure();
        assert!(wait > Duration::from_millis(100) && wait <= Duration::from_millis(150), "{wait:?}");
    }

    #[test]
    fn stats_count_packets_and_bytes() {
        let mut s = shaper(&[("latency", "30ms")], false);
        s.shape(packet());
        s.shape(packet());
        assert_eq!(s.stats.up.packets.load(Relaxed), 2);
        assert_eq!(s.stats.up.bytes.load(Relaxed), 200);
        assert_eq!(s.stats.up.avg_delay(), Duration::from_millis(30));
    }
}
