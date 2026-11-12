//! `top`: a full-screen view of the queue (alternate screen, raw mode),
//! refreshed every `--interval` seconds and on resize.
//!
//! [`View`] is a plain description of what is on screen, built from the
//! ledger by [`View::build`] (pure) and drawn by [`draw`].

use std::io::IsTerminal;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};
use ratatui::Frame;
use task_traffic_controller::admit::{self, Decision, Limits, LiveSnapshot, Reason, State};
use task_traffic_controller::config::Config;
use task_traffic_controller::display::{fmt_bytes, fmt_duration, is_idle, running_for, truncate};
use task_traffic_controller::error::{Error, Result};
use task_traffic_controller::ledger::{Entry, Ledger};
use task_traffic_controller::{eta, exec, key, lock, managed, Context};

/// One running entry's share of the budget bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub letter: char,
    pub slots: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// No learned wall time.
    Unknown,
    /// `pct` is capped at 99 while the command is still running. `left` is
    /// the time to the expected end, or the overrun when `over`.
    Known { pct: u8, left: Duration, over: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub letter: char,
    pub project: String,
    pub job: String,
    pub weight: u32,
    pub using: Option<f64>,
    pub elapsed: Duration,
    pub progress: Progress,
    pub idle: bool,
    pub backfilled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WaitRow {
    pub project: String,
    pub job: String,
    pub weight: u32,
    pub waited: Duration,
    pub front: bool,
    pub why: String,
    /// Seconds until the estimated start; None when there is no estimate.
    pub start_in: Option<f64>,
    pub rough: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeldRow {
    pub id: String,
    pub reason: String,
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Machine {
    pub cpu_busy: f64,
    pub available_memory: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub now: DateTime<Utc>,
    pub budget: u32,
    pub used: u32,
    /// One per running entry, in `running` order. Slots sum to at most
    /// `budget`; `slots + free == budget`.
    pub slots: Vec<Slot>,
    pub free: u32,
    /// Slots charged beyond the budget (measured use above the weight).
    pub over: u32,
    pub machine: Option<Machine>,
    pub running: Vec<RunRow>,
    pub waiting: Vec<WaitRow>,
    pub held: Vec<HeldRow>,
}

impl View {
    /// Read the ledger and the machine, then [`View::from_parts`].
    pub fn build(ctx: &Context, cfg: &Config) -> Result<View> {
        let ledger = lock::load_reaped(ctx)?;
        let held = managed::views(ctx, &ledger.entries)
            .into_iter()
            .filter(|v| v.quarantined)
            .map(|v| HeldRow {
                id: v.command_id,
                reason: v.reason,
                hint: v.hint,
            })
            .collect();
        Ok(View::from_parts(
            &ledger,
            held,
            cfg.limits(),
            ctx.live.snapshot(),
            ctx.clock.now(),
        ))
    }

    pub fn from_parts(
        ledger: &Ledger,
        held: Vec<HeldRow>,
        limits: Limits,
        live: Option<LiveSnapshot>,
        now: DateTime<Utc>,
    ) -> View {
        let queue = ledger.queue_at(now);
        let used = admit::used_budget(&queue);
        let timed: Vec<eta::Timed> = queue
            .iter()
            .zip(&ledger.entries)
            .map(|(entry, e)| eta::Timed {
                entry: entry.clone(),
                elapsed_secs: e
                    .started_at
                    .map(|s| (now - s).num_milliseconds().max(0) as f64 / 1000.0)
                    .unwrap_or(0.0),
            })
            .collect();

        let mut running: Vec<&Entry> = ledger
            .entries
            .iter()
            .filter(|e| e.state == State::Running)
            .collect();
        running.sort_by_key(|e| e.started_at);

        let mut slots = Vec::new();
        let mut taken = 0u32;
        let mut over = 0u32;
        let mut run_rows = Vec::new();
        for (i, e) in running.iter().enumerate() {
            let letter = letter(i);
            let charged = queue
                .iter()
                .find(|q| q.id == e.id)
                .map(admit::charged)
                .unwrap_or(e.weight);
            let fits = charged.min(limits.budget.saturating_sub(taken));
            over += charged - fits;
            taken += fits;
            slots.push(Slot {
                letter,
                slots: fits,
            });
            let elapsed = running_for(e, now);
            run_rows.push(RunRow {
                letter,
                project: project(e),
                job: job_name(e),
                weight: e.weight,
                using: e.using,
                elapsed,
                progress: progress(elapsed, e.expected_secs),
                idle: is_idle(e.using, elapsed),
                backfilled: e.backfilled,
            });
        }

        let order = admit::queue_order(&queue);
        let mut wait_rows = Vec::new();
        for q in &order {
            let Some(e) = ledger.find(&q.id) else {
                continue;
            };
            let est = eta::estimate(&timed, limits, &e.id);
            wait_rows.push(WaitRow {
                project: project(e),
                job: job_name(e),
                weight: e.weight,
                waited: (now - e.enqueued_at).to_std().unwrap_or_default(),
                front: e.front,
                why: why(&queue, q, limits, live),
                start_in: est.as_ref().map(|x| x.start_in),
                rough: est.as_ref().is_some_and(|x| x.rough),
            });
        }

        View {
            now,
            budget: limits.budget,
            used,
            slots,
            free: limits.budget.saturating_sub(taken),
            over,
            machine: live.map(|s| Machine {
                cpu_busy: s.cpu_busy,
                available_memory: s.available_memory,
            }),
            running: run_rows,
            waiting: wait_rows,
            held,
        }
    }
}

/// A, B, C, ... Z, then back to A.
fn letter(i: usize) -> char {
    (b'A' + (i % 26) as u8) as char
}

/// Repo basename, plus `/subdir` when there is one.
fn project(e: &Entry) -> String {
    if e.repo.is_empty() {
        "-".into()
    } else {
        key::project_label(&e.repo, &e.subdir)
    }
}

/// The `--label`; otherwise the script's file name for `bash ... step.sh`
/// style commands; otherwise the whole command line.
fn job_name(e: &Entry) -> String {
    if let Some(l) = e.label.as_deref().filter(|l| !l.is_empty()) {
        return l.to_string();
    }
    script_name(&e.argv).unwrap_or_else(|| e.argv.clone())
}

fn script_name(argv: &str) -> Option<String> {
    let mut words = argv.split_whitespace();
    let shell = Path::new(words.next()?).file_name()?.to_str()?;
    if !matches!(shell, "sh" | "bash" | "zsh" | "dash") {
        return None;
    }
    words.filter(|w| !w.starts_with('-')).find_map(|w| {
        let name = Path::new(w).file_name()?.to_str()?;
        name.ends_with(".sh").then(|| name.to_string())
    })
}

fn progress(elapsed: Duration, expected_secs: Option<f64>) -> Progress {
    let Some(exp) = expected_secs.filter(|x| *x > 0.0) else {
        return Progress::Unknown;
    };
    let el = elapsed.as_secs_f64();
    let pct = ((el / exp) * 100.0).floor().clamp(0.0, 99.0) as u8;
    Progress::Known {
        pct,
        left: Duration::from_secs_f64((exp - el).abs()),
        over: el > exp,
    }
}

/// Why the waiting entry `q` is not running yet, from the admit decision.
fn why(
    queue: &[admit::QueueEntry],
    q: &admit::QueueEntry,
    limits: Limits,
    live: Option<LiveSnapshot>,
) -> String {
    match admit::admit(queue, &q.id, 0, q.waited_secs, limits, live) {
        Decision::Admitted => "starting".into(),
        Decision::Wait(Reason::Budget { used, budget }) => format!(
            "needs w{}, {} free",
            q.weight.min(budget).max(1),
            budget.saturating_sub(used)
        ),
        Decision::Wait(Reason::NotHead { ahead }) => format!("behind {ahead}"),
        Decision::Wait(Reason::Reserved { ahead }) => format!("behind {ahead}, reserved"),
        Decision::Wait(r) => exec::describe(&r),
    }
}

// ---------------------------------------------------------------- drawing

const PALETTE: [Color; 8] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::Blue,
    Color::Red,
    Color::LightCyan,
    Color::LightGreen,
];

fn color(letter: char) -> Color {
    PALETTE[(letter as usize - 'A' as usize) % PALETTE.len()]
}

pub fn draw(f: &mut Frame, v: &View, interval: Duration, error: Option<&str>) {
    let area = f.area();
    let held_h = if v.held.is_empty() {
        0
    } else {
        v.held.len() as u16 + 3
    };
    let [head, budget, running, waiting, held] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(held_h),
    ])
    .areas(area);
    draw_header(f, head, v, interval, error);
    draw_budget(f, budget, v);
    draw_running(f, running, v);
    draw_waiting(f, waiting, v);
    if held_h > 0 {
        draw_held(f, held, v);
    }
}

fn draw_header(f: &mut Frame, area: Rect, v: &View, interval: Duration, error: Option<&str>) {
    let clock = v.now.with_timezone(&Local).format("%H:%M:%S").to_string();
    let mut spans = vec![
        Span::styled(
            "task-traffic-controller",
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {clock}  every {}s", interval.as_secs_f64())),
    ];
    if let Some(e) = error {
        spans.push(Span::styled(
            format!("  {}", truncate(e, 60)),
            Style::new().fg(Color::Red),
        ));
    }
    let hint = "q: quit";
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    f.render_widget(
        Paragraph::new(hint)
            .right_aligned()
            .style(Style::new().add_modifier(Modifier::DIM)),
        area,
    );
}

fn draw_budget(f: &mut Frame, area: Rect, v: &View) {
    let block = Block::bordered().title(format!(" CPU budget {}/{} ", v.used, v.budget));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [bar_row, stat_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    f.render_widget(Paragraph::new(bar_line(v, inner.width)), bar_row);
    let mut stats = vec![Span::raw(format!("{} free", v.free))];
    if v.over > 0 {
        stats.push(Span::styled(
            format!("  {} over budget", v.over),
            Style::new().fg(Color::Red),
        ));
    }
    if let Some(m) = &v.machine {
        stats.push(Span::raw(format!(
            "   machine cpu {:.0}% busy, {} available",
            m.cpu_busy * 100.0,
            fmt_bytes(m.available_memory)
        )));
    }
    f.render_widget(Paragraph::new(Line::from(stats)), stat_row);
}

/// One cell group per slot; each running entry's slots in its own color with
/// its letter first, free slots dim.
fn bar_line(v: &View, width: u16) -> Line<'static> {
    let w = (width as u32 / v.budget.max(1)).clamp(1, 4) as usize;
    let mut spans = Vec::new();
    for s in &v.slots {
        if s.slots == 0 {
            continue;
        }
        let c = color(s.letter);
        let cells = s.slots as usize * w;
        spans.push(Span::styled(
            s.letter.to_string(),
            Style::new()
                .fg(Color::Black)
                .bg(c)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled("█".repeat(cells - 1), Style::new().fg(c)));
    }
    spans.push(Span::styled(
        "░".repeat(v.free as usize * w),
        Style::new().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

fn gauge(pct: u8, width: usize) -> String {
    let filled = (pct as usize * width / 100).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn progress_cell(r: &RunRow) -> String {
    match &r.progress {
        Progress::Unknown => format!("{}  ? ran {}", gauge(0, 10), fmt_duration(r.elapsed)),
        Progress::Known { pct, left, over } => format!(
            "{} {pct:>2}% {}{}",
            gauge(*pct, 10),
            if *over { "over " } else { "left " },
            fmt_duration(*left)
        ),
    }
}

fn cpu_cell(r: &RunRow) -> String {
    match r.using {
        Some(u) => format!("{}/{u:.1}", r.weight),
        None => format!("{}/-", r.weight),
    }
}

fn draw_running(f: &mut Frame, area: Rect, v: &View) {
    let rows: Vec<Row> = v
        .running
        .iter()
        .map(|r| {
            let mut flags = Vec::new();
            if r.idle {
                flags.push(Span::styled("idle ", Style::new().fg(Color::Yellow)));
            }
            if r.backfilled {
                flags.push(Span::styled("[backfill]", Style::new().fg(Color::DarkGray)));
            }
            Row::new(vec![
                Cell::from(r.letter.to_string()).style(
                    Style::new()
                        .fg(Color::Black)
                        .bg(color(r.letter))
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(r.project.clone()),
                Cell::from(r.job.clone()),
                Cell::from(Line::from(flags)),
                Cell::from(cpu_cell(r)),
                Cell::from(progress_cell(r)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(18),
            Constraint::Fill(1),
            Constraint::Length(15),
            Constraint::Length(9),
            Constraint::Length(24),
        ],
    )
    .header(
        Row::new(vec!["", "project", "job", "", "cpu w/use", "progress"])
            .style(Style::new().add_modifier(Modifier::BOLD)),
    )
    .block(Block::bordered().title(format!(" running ({}) ", v.running.len())));
    f.render_widget(table, area);
}

fn eta_cell(w: &WaitRow) -> String {
    match w.start_in {
        None => "?".into(),
        Some(s) if s <= 0.0 => "now".into(),
        Some(s) => format!(
            "~{}{}",
            fmt_duration(Duration::from_secs_f64(s)),
            if w.rough { "?" } else { "" }
        ),
    }
}

fn draw_waiting(f: &mut Frame, area: Rect, v: &View) {
    let rows: Vec<Row> = v
        .waiting
        .iter()
        .map(|w| {
            let job = if w.front {
                format!("[front] {}", w.job)
            } else {
                w.job.clone()
            };
            Row::new(vec![
                Cell::from(w.project.clone()),
                Cell::from(job),
                Cell::from(format!("w{}", w.weight)),
                Cell::from(fmt_duration(w.waited)),
                Cell::from(w.why.clone()),
                Cell::from(eta_cell(w)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Fill(1),
            Constraint::Length(4),
            Constraint::Length(8),
            Constraint::Length(22),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["project", "job", "w", "waited", "why", "start in"])
            .style(Style::new().add_modifier(Modifier::BOLD)),
    )
    .block(Block::bordered().title(format!(" waiting ({}) ", v.waiting.len())));
    f.render_widget(table, area);
}

fn draw_held(f: &mut Frame, area: Rect, v: &View) {
    let lines: Vec<Line> = v
        .held
        .iter()
        .map(|h| Line::from(format!("{}  {}  -> {}", h.id, h.reason, h.hint)))
        .collect();
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(format!(" held, owner gone ({}) ", v.held.len()))),
        area,
    );
}

// ------------------------------------------------------------- event loop

/// Puts the terminal back on every exit path, including a panic unwinding
/// through [`run`] (`ratatui::init` also installs a panic hook that does it
/// before the message prints).
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

fn is_quit(key: &event::KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && match key.code {
            KeyCode::Char('q') | KeyCode::Esc => true,
            KeyCode::Char('c') => key.modifiers.contains(KeyModifiers::CONTROL),
            _ => false,
        }
}

pub fn run(ctx: &Context, cfg: &Config, interval: f64) -> Result<i32> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(Error::Spawn(
            "top needs a terminal; use `status` for a one-shot, pipeable view".into(),
        ));
    }
    let interval = Duration::from_secs_f64(interval.clamp(0.2, 3600.0));
    // Build the first view before touching the terminal, so a ledger that
    // cannot be read is reported on the normal screen.
    let mut view = View::build(ctx, cfg)?;
    let mut error: Option<String> = None;
    let mut terminal = ratatui::init();
    let _restore = Restore;
    let mut next = Instant::now() + interval;
    loop {
        terminal.draw(|f| draw(f, &view, interval, error.as_deref()))?;
        let wait = next.saturating_duration_since(Instant::now());
        if event::poll(wait)? {
            match event::read()? {
                Event::Key(k) if is_quit(&k) => return Ok(0),
                // A resize (or any other event) redraws with the same data.
                _ => continue,
            }
        }
        match View::build(ctx, cfg) {
            Ok(v) => {
                view = v;
                error = None;
            }
            Err(e) => error = Some(e.to_string()),
        }
        next = Instant::now() + interval;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as Cd;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn limits(budget: u32) -> Limits {
        let mut l = Config::parse("").unwrap().limits();
        l.budget = budget;
        l
    }

    #[allow(clippy::too_many_arguments)]
    fn entry(
        id: &str,
        state: State,
        weight: u32,
        seq: u64,
        repo: &str,
        subdir: &str,
        argv: &str,
        now: DateTime<Utc>,
        age_secs: i64,
    ) -> Entry {
        Entry {
            id: id.into(),
            pid: 1000 + seq as u32,
            state,
            front: false,
            weight,
            predicted_rss: 0,
            expected_secs: None,
            repo: repo.into(),
            subdir: subdir.into(),
            argv: argv.into(),
            key: format!("k{seq}"),
            seq,
            enqueued_at: now - Cd::seconds(age_secs),
            started_at: (state == State::Running).then(|| now - Cd::seconds(age_secs)),
            using: None,
            backfilled: false,
            label: None,
        }
    }

    fn ledger(entries: Vec<Entry>) -> Ledger {
        Ledger {
            entries,
            ..Ledger::default()
        }
    }

    fn sample(now: DateTime<Utc>) -> Ledger {
        let mut a = entry(
            "a",
            State::Running,
            4,
            1,
            "/src/web",
            "api",
            "cargo test -p api",
            now,
            95,
        );
        a.expected_secs = Some(190.0);
        a.using = Some(3.4);
        a.label = Some("web/api unit tests".into());
        let mut b = entry(
            "b",
            State::Running,
            2,
            2,
            "/src/web",
            "",
            "npm run build -- --mode production",
            now,
            400,
        );
        b.expected_secs = Some(600.0);
        b.using = Some(0.1);
        b.backfilled = true;
        let c = entry(
            "c",
            State::Running,
            1,
            3,
            "/src/docs",
            "",
            "bash /ci/steps/step-8.sh --fast",
            now,
            12,
        );
        let d = entry(
            "d",
            State::Waiting,
            6,
            4,
            "/src/worker",
            "",
            "cargo test --workspace --all-features",
            now,
            45,
        );
        let mut e = entry(
            "e",
            State::Waiting,
            2,
            5,
            "/src/web",
            "app",
            "npm test",
            now,
            20,
        );
        e.label = Some("web/app e2e".into());
        ledger(vec![a, b, c, d, e])
    }

    #[test]
    fn progress_is_capped_while_running_and_counts_the_overrun() {
        let s = Duration::from_secs;
        assert_eq!(
            progress(s(50), Some(200.0)),
            Progress::Known {
                pct: 25,
                left: s(150),
                over: false
            }
        );
        assert_eq!(
            progress(s(199), Some(200.0)),
            Progress::Known {
                pct: 99,
                left: s(1),
                over: false
            }
        );
        assert_eq!(
            progress(s(200), Some(200.0)),
            Progress::Known {
                pct: 99,
                left: s(0),
                over: false
            }
        );
        assert_eq!(
            progress(s(260), Some(200.0)),
            Progress::Known {
                pct: 99,
                left: s(60),
                over: true
            }
        );
        assert_eq!(progress(s(5), None), Progress::Unknown);
        assert_eq!(progress(s(5), Some(0.0)), Progress::Unknown);
    }

    #[test]
    fn jobs_are_named_by_label_then_script_then_argv() {
        let now = Utc::now();
        let mut e = entry("a", State::Running, 1, 1, "/r/web", "api", "x", now, 1);
        e.argv = "bash -e /ci/steps/step-8.sh --fast".into();
        assert_eq!(job_name(&e), "step-8.sh");
        e.argv = "/bin/zsh ./run.sh".into();
        assert_eq!(job_name(&e), "run.sh");
        e.argv = "bash -c make test".into();
        assert_eq!(job_name(&e), "bash -c make test");
        e.argv = "cargo test --workspace".into();
        assert_eq!(job_name(&e), "cargo test --workspace");
        e.label = Some("unit tests".into());
        assert_eq!(job_name(&e), "unit tests");
        assert_eq!(project(&e), "web/api");
        e.subdir.clear();
        assert_eq!(project(&e), "web");
        e.repo.clear();
        assert_eq!(project(&e), "-");
    }

    #[test]
    fn the_model_names_rows_and_assigns_slots() {
        let now = Utc::now();
        let live = LiveSnapshot {
            available_memory: 9 << 30,
            cpu_busy: 0.5,
            load_avg: 1.0,
        };
        let v = View::from_parts(&sample(now), vec![], limits(12), Some(live), now);
        assert_eq!(v.budget, 12);
        // Oldest first: A is the w2 build, B the w4 tests (measured 3.4, charged 4), C is w1.
        assert_eq!(v.used, 7);
        let letters: Vec<_> = v.running.iter().map(|r| r.letter).collect();
        assert_eq!(letters, ['A', 'B', 'C']);
        assert_eq!(
            v.slots.iter().map(|s| s.slots).collect::<Vec<_>>(),
            [2, 4, 1]
        );
        assert_eq!(v.free, 5);
        assert_eq!(
            v.slots.iter().map(|s| s.slots).sum::<u32>() + v.free,
            v.budget
        );
        assert_eq!(v.over, 0);
        // Oldest started first: B (400 s), A (95 s), C (12 s) -> reorder check.
        assert_eq!(v.running[0].job, "npm run build -- --mode production");
        assert_eq!(v.running[2].job, "step-8.sh");
        assert_eq!(v.running[1].job, "web/api unit tests");
        assert_eq!(v.running[1].project, "web/api");
        assert_eq!(
            v.running[0].progress,
            Progress::Known {
                pct: 66,
                left: Duration::from_secs(200),
                over: false
            }
        );
        assert!(v.running[0].idle && v.running[0].backfilled);
        assert_eq!(v.running[2].progress, Progress::Unknown);
        assert_eq!(v.waiting.len(), 2);
        assert_eq!(v.waiting[0].why, "needs w6, 5 free");
        assert_eq!(v.waiting[1].job, "web/app e2e");
        assert_eq!(v.waiting[1].why, "behind 1");
        assert!(v.machine.is_some());
    }

    #[test]
    fn slots_never_exceed_the_budget() {
        let now = Utc::now();
        let mut x = entry("x", State::Running, 3, 1, "/r/a", "", "a", now, 30);
        let mut y = entry("y", State::Running, 3, 2, "/r/b", "", "b", now, 20);
        y.using = Some(7.0); // measured above its weight
        x.using = None;
        let v = View::from_parts(&ledger(vec![x, y]), vec![], limits(6), None, now);
        let total: u32 = v.slots.iter().map(|s| s.slots).sum();
        assert_eq!(total + v.free, 6);
        assert_eq!(v.free, 0);
        assert_eq!(v.over, 4); // charged 3 + 7 = 10 against 6
        assert!(v.machine.is_none());
    }

    fn frame(v: &View, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, v, Duration::from_secs(1), None))
            .unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_frame_shows_the_budget_bar_and_both_tables() {
        let now = Utc::now();
        let live = LiveSnapshot {
            available_memory: 9 << 30,
            cpu_busy: 0.5,
            load_avg: 1.0,
        };
        let mut v = View::from_parts(&sample(now), vec![], limits(12), Some(live), now);
        v.held.push(HeldRow {
            id: "m1".into(),
            reason: "owner gone".into(),
            hint: "resolve m1".into(),
        });
        let out = frame(&v, 100, 26);
        println!("{out}");
        assert!(out.contains("CPU budget 7/12"));
        assert!(out.contains("running (3)"));
        assert!(out.contains("waiting (2)"));
        assert!(out.contains("held, owner gone (1)"));
        assert!(out.contains("q: quit"));
        assert!(out.contains("web/api unit tests"));
        assert!(out.contains("step-8.sh") && out.contains("4/3.4"));
        assert!(out.contains("needs w6, 5 free"));
        assert!(out.contains("machine cpu 50% busy"));
        assert!(out.contains("idle") && out.contains("[backfill]"));
        // A tiny terminal must not panic.
        let _ = frame(&v, 20, 5);
    }
}
