//! Min-heap of packets keyed by release time, drained by a single timer thread.
//!
//! Reordering lives here too: a "held" packet is kept aside until the next
//! packet in the same direction is scheduled, then released just after it
//! (or after `MAX_HOLD` if nothing else shows up).

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::conditions::Direction;
use crate::stats::Stats;

/// Longest a reordered packet waits for a successor before being released anyway.
pub const MAX_HOLD: Duration = Duration::from_millis(100);

struct Entry<T> {
    at: Instant,
    seq: u64,
    dir: Direction,
    item: T,
}

// Reversed so BinaryHeap pops the earliest release time; seq keeps FIFO on ties.
impl<T> Ord for Entry<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        other.at.cmp(&self.at).then(other.seq.cmp(&self.seq))
    }
}
impl<T> PartialOrd for Entry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> PartialEq for Entry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.seq == other.seq
    }
}
impl<T> Eq for Entry<T> {}

struct State<T> {
    heap: BinaryHeap<Entry<T>>,
    /// Reordered packets waiting for a successor, per direction. `at` stays their
    /// natural release time; they are flushed regardless after `MAX_HOLD`.
    held: [Option<Entry<T>>; 2],
    seq: u64,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    wake: Condvar,
    stats: Arc<Stats>,
}

pub struct Scheduler<T>(Arc<Shared<T>>);

impl<T> Clone for Scheduler<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Send + 'static> Scheduler<T> {
    /// Starts the timer thread. `deliver` is called from that thread when a packet is due,
    /// so it must not block for long.
    pub fn start(stats: Arc<Stats>, deliver: impl Fn(T) + Send + 'static) -> Self {
        enable_high_resolution_timers();
        let shared = Arc::new(Shared {
            state: Mutex::new(State { heap: BinaryHeap::new(), held: [None, None], seq: 0 }),
            wake: Condvar::new(),
            stats,
        });
        let worker = shared.clone();
        thread::spawn(move || timer_loop(worker, deliver));
        Self(shared)
    }

    pub fn schedule(&self, dir: Direction, delay: Duration, hold: bool, item: T) {
        self.schedule_at(dir, Instant::now() + delay, hold, item);
    }

    pub fn schedule_at(&self, dir: Direction, at: Instant, hold: bool, item: T) {
        let mut st = self.0.state.lock().unwrap();
        st.seq += 1;
        let entry = Entry { at, seq: st.seq, dir, item };
        if let Some(prev) = st.held[dir.index()].take() {
            // Just after the packet that overtook it, but never before its own delay is up.
            st.heap.push(Entry { at: prev.at.max(at) + Duration::from_micros(1), ..prev });
        }
        if hold {
            st.held[dir.index()] = Some(entry);
        } else {
            st.heap.push(entry);
        }
        self.0.stats.dir(dir).in_flight.fetch_add(1, Relaxed);
        self.0.wake.notify_one();
    }
}

fn timer_loop<T>(shared: Arc<Shared<T>>, deliver: impl Fn(T)) {
    let mut st = shared.state.lock().unwrap();
    loop {
        let now = Instant::now();
        let State { heap, held, .. } = &mut *st;
        for slot in held.iter_mut() {
            if slot.as_ref().is_some_and(|e| e.at + MAX_HOLD <= now) {
                heap.push(slot.take().unwrap());
            }
        }
        let next_due = heap
            .peek()
            .map(|e| e.at)
            .into_iter()
            .chain(held.iter().flatten().map(|e| e.at + MAX_HOLD))
            .min();
        match next_due {
            None => st = shared.wake.wait(st).unwrap(),
            Some(t) if t > now => st = shared.wake.wait_timeout(st, t - now).unwrap().0,
            Some(_) => {
                let entry = st.heap.pop().unwrap();
                shared.stats.dir(entry.dir).in_flight.fetch_sub(1, Relaxed);
                drop(st);
                deliver(entry.item);
                st = shared.state.lock().unwrap();
            }
        }
    }
}

/// Windows timers tick every ~15ms unless a process asks for 1ms.
fn enable_high_resolution_timers() {
    #[cfg(windows)]
    {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(period: u32) -> u32;
        }
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe {
            timeBeginPeriod(1);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn scheduler() -> (Scheduler<u32>, mpsc::Receiver<(u32, Instant)>) {
        let (tx, rx) = mpsc::channel();
        let s = Scheduler::start(Arc::new(Stats::new()), move |n| {
            let _ = tx.send((n, Instant::now()));
        });
        (s, rx)
    }

    fn collect(rx: &mpsc::Receiver<(u32, Instant)>, n: usize) -> Vec<u32> {
        (0..n).map(|_| rx.recv_timeout(Duration::from_secs(2)).unwrap().0).collect()
    }

    #[test]
    fn releases_in_time_order_not_insertion_order() {
        let (s, rx) = scheduler();
        s.schedule(Direction::Up, Duration::from_millis(60), false, 3);
        s.schedule(Direction::Up, Duration::from_millis(20), false, 1);
        s.schedule(Direction::Up, Duration::from_millis(40), false, 2);
        assert_eq!(collect(&rx, 3), vec![1, 2, 3]);
    }

    #[test]
    fn equal_times_keep_fifo() {
        let (s, rx) = scheduler();
        let at = Instant::now() + Duration::from_millis(20);
        for i in 0..5 {
            s.schedule_at(Direction::Down, at, false, i);
        }
        assert_eq!(collect(&rx, 5), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn delay_is_respected() {
        let (s, rx) = scheduler();
        let start = Instant::now();
        s.schedule(Direction::Up, Duration::from_millis(50), false, 1);
        let (_, released) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let waited = released - start;
        assert!(waited >= Duration::from_millis(50), "{waited:?}");
        assert!(waited < Duration::from_millis(150), "{waited:?}");
    }

    #[test]
    fn held_packet_is_released_after_the_next_one() {
        let (s, rx) = scheduler();
        s.schedule(Direction::Up, Duration::from_millis(10), true, 1);
        s.schedule(Direction::Up, Duration::from_millis(10), false, 2);
        assert_eq!(collect(&rx, 2), vec![2, 1]);
    }

    #[test]
    fn held_packet_never_goes_out_before_its_own_delay() {
        let (s, rx) = scheduler();
        let start = Instant::now();
        s.schedule(Direction::Up, Duration::from_millis(80), true, 1);
        s.schedule(Direction::Up, Duration::from_millis(10), false, 2);
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().0, 2);
        let (n, released) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(n, 1);
        assert!(released - start >= Duration::from_millis(80), "{:?}", released - start);
    }

    #[test]
    fn held_packet_is_released_alone_after_max_hold() {
        let (s, rx) = scheduler();
        let start = Instant::now();
        s.schedule(Direction::Up, Duration::ZERO, true, 1);
        let (n, released) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(n, 1);
        assert!(released - start >= MAX_HOLD);
    }

    #[test]
    fn hold_slots_are_per_direction() {
        let (s, rx) = scheduler();
        s.schedule(Direction::Up, Duration::ZERO, true, 1);
        s.schedule(Direction::Down, Duration::ZERO, false, 2);
        // Down traffic must not release the held Up packet.
        assert_eq!(collect(&rx, 1), vec![2]);
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
    }

    #[test]
    fn in_flight_tracks_queue_depth() {
        let stats = Arc::new(Stats::new());
        let (tx, rx) = mpsc::channel();
        let s = Scheduler::start(stats.clone(), move |n: u32| {
            let _ = tx.send(n);
        });
        s.schedule(Direction::Up, Duration::from_millis(50), false, 1);
        s.schedule(Direction::Up, Duration::from_millis(50), false, 2);
        assert_eq!(stats.up.in_flight.load(Relaxed), 2);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(stats.up.in_flight.load(Relaxed), 0);
    }
}
