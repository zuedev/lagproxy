//! Interactive terminal view (`--tui`): live counters plus sliders for the
//! most useful knobs in each direction.

use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Gauge, Paragraph, Row, Table};
use ratatui::DefaultTerminal;

use crate::conditions::{Conditions, Config, Direction};
use crate::parse;
use crate::stats::{thousands, Stats};

/// One slider. Values are in the knob's own unit (ms, %, mbit).
struct Knob {
    label: &'static str,
    step: f64,
    max: f64,
    get: fn(&Conditions) -> f64,
    set: fn(&mut Conditions, f64),
    fmt: fn(f64) -> String,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
fn from_ms(v: f64) -> Duration {
    Duration::from_secs_f64(v / 1000.0)
}
fn pct(v: f64) -> String {
    parse::fmt_percent(v / 100.0)
}

const KNOBS: &[Knob] = &[
    Knob { label: "latency", step: 10.0, max: 1000.0, get: |c| ms(c.latency), set: |c, v| c.latency = from_ms(v), fmt: |v| parse::fmt_duration(from_ms(v)) },
    Knob { label: "jitter", step: 5.0, max: 500.0, get: |c| ms(c.jitter), set: |c, v| c.jitter = from_ms(v), fmt: |v| parse::fmt_duration(from_ms(v)) },
    Knob { label: "loss", step: 0.5, max: 100.0, get: |c| c.loss * 100.0, set: |c, v| c.loss = v / 100.0, fmt: pct },
    Knob { label: "loss-burst", step: 5.0, max: 100.0, get: |c| c.loss_burst * 100.0, set: |c, v| c.loss_burst = v / 100.0, fmt: pct },
    Knob { label: "dup", step: 0.5, max: 100.0, get: |c| c.dup * 100.0, set: |c, v| c.dup = v / 100.0, fmt: pct },
    Knob { label: "reorder", step: 0.5, max: 100.0, get: |c| c.reorder * 100.0, set: |c, v| c.reorder = v / 100.0, fmt: pct },
    Knob { label: "corrupt", step: 0.1, max: 100.0, get: |c| c.corrupt * 100.0, set: |c, v| c.corrupt = v / 100.0, fmt: pct },
    Knob { label: "bandwidth", step: 0.5, max: 100.0, get: |c| c.bandwidth as f64 / 1e6, set: |c, v| c.bandwidth = (v * 1e6) as u64, fmt: |v| parse::fmt_bandwidth((v * 1e6) as u64) },
];

struct App {
    dir: Direction,
    selected: usize,
    last_tick: Instant,
    last_bytes: [u64; 2],
    /// Bytes per second per direction, refreshed every tick.
    rate: [f64; 2],
}

impl App {
    fn new() -> Self {
        Self { dir: Direction::Up, selected: 0, last_tick: Instant::now(), last_bytes: [0, 0], rate: [0.0, 0.0] }
    }

    fn tick(&mut self, stats: &Stats) {
        let elapsed = self.last_tick.elapsed().as_secs_f64();
        if elapsed < 0.5 {
            return;
        }
        for dir in [Direction::Up, Direction::Down] {
            let i = dir.index();
            let bytes = stats.dir(dir).bytes.load(Relaxed);
            self.rate[i] = bytes.saturating_sub(self.last_bytes[i]) as f64 / elapsed;
            self.last_bytes[i] = bytes;
        }
        self.last_tick = Instant::now();
    }

    fn adjust(&self, config: &RwLock<Config>, steps: f64) {
        let knob = &KNOBS[self.selected];
        let mut cfg = config.write().unwrap();
        let c = cfg.dir_mut(self.dir);
        let v = ((knob.get)(c) + steps * knob.step).clamp(0.0, knob.max);
        (knob.set)(c, (v / knob.step).round() * knob.step);
    }
}

/// Runs until the user quits. Takes over the terminal while running.
pub fn run(config: Arc<RwLock<Config>>, stats: Arc<Stats>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    ACTIVE.store(true, Relaxed);
    let result = event_loop(&mut terminal, &config, &stats);
    restore();
    result
}

/// Gives the terminal back if the TUI is up. Safe to call from any thread and any exit path.
pub fn restore() {
    if ACTIVE.swap(false, Relaxed) {
        ratatui::restore();
    }
}

static ACTIVE: AtomicBool = AtomicBool::new(false);

fn event_loop(terminal: &mut DefaultTerminal, config: &RwLock<Config>, stats: &Stats) -> io::Result<()> {
    let mut app = App::new();
    loop {
        app.tick(stats);
        terminal.draw(|f| draw(f, &app, config, stats))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let mult = if key.modifiers.contains(KeyModifiers::SHIFT) { 10.0 } else { 1.0 };
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Tab | KeyCode::BackTab => {
                app.dir = if app.dir == Direction::Up { Direction::Down } else { Direction::Up }
            }
            KeyCode::Up => app.selected = app.selected.saturating_sub(1),
            KeyCode::Down => app.selected = (app.selected + 1).min(KNOBS.len() - 1),
            KeyCode::Left | KeyCode::Char('-') => app.adjust(config, -mult),
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => app.adjust(config, mult),
            KeyCode::Char('0') => app.adjust(config, f64::NEG_INFINITY),
            _ => {}
        }
    }
}

fn draw(f: &mut Frame, app: &App, config: &RwLock<Config>, stats: &Stats) {
    let [top, middle, bottom] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(0), Constraint::Length(1)]).areas(f.area());

    let header = Row::new(["", "Packets", "Bytes", "Dropped", "Dup", "Reordered", "Avg delay", "In flight", "Throughput"])
        .style(Style::new().bold());
    let rows = [Direction::Up, Direction::Down].map(|dir| {
        let s = stats.dir(dir);
        Row::new(vec![
            dir.name().to_string(),
            thousands(s.packets.load(Relaxed)),
            parse::fmt_bytes(s.bytes.load(Relaxed)),
            thousands(s.dropped.load(Relaxed)),
            thousands(s.dup.load(Relaxed)),
            thousands(s.reordered.load(Relaxed)),
            parse::fmt_duration(s.avg_delay()),
            thousands(s.in_flight.load(Relaxed)),
            format!("{}/s", parse::fmt_bandwidth((app.rate[dir.index()] * 8.0) as u64)),
        ])
    });
    let mut widths = vec![Constraint::Fill(1); 9];
    widths[0] = Constraint::Length(5);
    f.render_widget(
        Table::new(rows, widths).header(header).block(Block::bordered().title(" lagproxy ")),
        top,
    );

    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(middle);
    let cfg = config.read().unwrap();
    for (dir, title, area) in [(Direction::Up, " up: client -> server ", left), (Direction::Down, " down: server -> client ", right)] {
        let active = dir == app.dir;
        let block = Block::bordered()
            .title(title)
            .border_style(if active { Style::new().yellow() } else { Style::new() });
        let inner = block.inner(area);
        f.render_widget(block, area);
        let lines = Layout::vertical(vec![Constraint::Length(1); KNOBS.len()]).split(inner);
        let c = cfg.dir(dir);
        for (i, (knob, line)) in KNOBS.iter().zip(lines.iter()).enumerate() {
            let v = (knob.get)(c);
            let selected = active && i == app.selected;
            let gauge = Gauge::default()
                .ratio((v / knob.max).clamp(0.0, 1.0))
                .label(format!("{:<11}{}", knob.label, (knob.fmt)(v)))
                .gauge_style(if selected { Style::new().fg(Color::Yellow).bg(Color::DarkGray) } else { Style::new().fg(Color::Blue).bg(Color::DarkGray) });
            f.render_widget(gauge, *line);
        }
    }

    f.render_widget(
        Paragraph::new(" tab: direction   up/down: select   left/right: adjust (shift = x10)   0: clear   q: quit ")
            .style(Style::new().dim()),
        bottom,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjust_changes_only_the_selected_direction_and_knob() {
        let config = RwLock::new(Config::default());
        let mut app = App::new();
        app.dir = Direction::Down;
        app.adjust(&config, 3.0);
        let c = config.read().unwrap();
        assert_eq!(c.down.latency, Duration::from_millis(30));
        assert_eq!(c.up.latency, Duration::ZERO);
        assert_eq!(c.down.jitter, Duration::ZERO);
    }

    #[test]
    fn adjust_clamps_and_snaps_to_steps() {
        let config = RwLock::new(Config::default());
        let mut app = App::new();
        app.selected = KNOBS.iter().position(|k| k.label == "loss").unwrap();
        app.adjust(&config, -5.0);
        assert_eq!(config.read().unwrap().up.loss, 0.0);
        app.adjust(&config, 1e9);
        assert_eq!(config.read().unwrap().up.loss, 1.0);
        app.adjust(&config, f64::NEG_INFINITY);
        assert_eq!(config.read().unwrap().up.loss, 0.0);
        app.adjust(&config, 1.0);
        assert_eq!(config.read().unwrap().up.loss, 0.005);
    }

    #[test]
    fn every_knob_round_trips_and_formats() {
        let mut c = Conditions::default();
        for knob in KNOBS {
            (knob.set)(&mut c, knob.step * 3.0);
            let v = (knob.get)(&c);
            assert!((v - knob.step * 3.0).abs() < 1e-9, "{}: {v}", knob.label);
            assert!(!(knob.fmt)(v).is_empty());
        }
        assert_eq!(c.latency, Duration::from_millis(30));
        assert_eq!(c.bandwidth, 1_500_000);
        assert_eq!((KNOBS[7].fmt)(0.0), "unlimited");
    }

    #[test]
    fn tick_measures_throughput() {
        let stats = Stats::new();
        let mut app = App::new();
        app.last_tick = Instant::now() - Duration::from_secs(1);
        stats.up.bytes.store(1000, Relaxed);
        app.tick(&stats);
        assert!((app.rate[0] - 1000.0).abs() < 50.0, "{}", app.rate[0]);
        assert_eq!(app.rate[1], 0.0);
    }
}
