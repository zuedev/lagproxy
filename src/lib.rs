//! lagproxy: a proxy that makes the network worse on purpose.
//!
//! Data flow for one packet:
//!
//! ```text
//! socket -> Shaper (loss, dup, corrupt, reorder, bandwidth, delay) -> Scheduler (min-heap + timer thread) -> socket
//! ```
//!
//! `Config` holds the live conditions behind an `RwLock` so the CLI, scenarios,
//! the HTTP API and the TUI can all change them while traffic is flowing.

pub mod api;
pub mod cli;
pub mod conditions;
pub mod packet_log;
pub mod parse;
pub mod presets;
pub mod scenario;
pub mod scheduler;
pub mod shaper;
pub mod stats;
pub mod tcp;
pub mod tui;
pub mod udp;

pub use conditions::{Conditions, Config, Direction};
pub use stats::Stats;
