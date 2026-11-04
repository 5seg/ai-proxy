//! Terminal UI: a live, read-only view over the usage database.
//!
//! The screen is rebuilt from a snapshot of aggregated rows. Snapshots are
//! re-read from SQLite at most every [`REFRESH`] and only between input polls,
//! so key handling is never blocked by a database read.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};
use rusqlite::Connection;

use crate::config::Config;
use crate::report::{self, Agg, Group, Prices, RawRow};

/// How often the database is re-read while the UI is idle.
const REFRESH: Duration = Duration::from_secs(2);

const HINTS: &str = " q quit · tab/shift-tab switch · 1-5 jump · j/k scroll · pgup/pgdn page · g/G top/bottom · r reload";

const HEADERS: [&str; 6] = ["reqs", "input", "cached", "hit", "output", "cost(USD)"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Overview,
    Devices,
    Ips,
    Models,
    Keys,
}

impl Screen {
    const ALL: [Screen; 5] = [
        Screen::Overview,
        Screen::Devices,
        Screen::Ips,
        Screen::Models,
        Screen::Keys,
    ];

    fn index(self) -> usize {
        match self {
            Screen::Overview => 0,
            Screen::Devices => 1,
            Screen::Ips => 2,
            Screen::Models => 3,
            Screen::Keys => 4,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Screen::Overview => "Overview",
            Screen::Devices => "Devices",
            Screen::Ips => "IPs",
            Screen::Models => "Models",
            Screen::Keys => "Keys",
        }
    }

    /// The group a screen aggregates by; the overview shows the day table.
    fn group(self) -> Group {
        match self {
            Screen::Overview => Group::Day,
            Screen::Devices => Group::Device,
            Screen::Ips => Group::Ip,
            Screen::Models => Group::Model,
            Screen::Keys => Group::Key,
        }
    }
}

/// Aggregated rows for every screen, read from the database in one pass.
struct Snapshot {
    totals: Agg,
    days: Vec<Agg>,
    devices: Vec<Agg>,
    ips: Vec<Agg>,
    models: Vec<Agg>,
    keys: Vec<Agg>,
}

impl Snapshot {
    fn load(conn: &Connection, retention_days: i64) -> Result<Self> {
        let since = crate::now_ms() - retention_days * 86_400_000;
        let prices = Prices::load(conn)?;
        let rows: Vec<RawRow> = report::load_rows(conn, since, i64::MAX)?;
        Ok(Self {
            totals: report::totals(&rows, &prices),
            days: report::aggregate(&rows, Group::Day, &prices, true),
            devices: report::aggregate(&rows, Group::Device, &prices, false),
            ips: report::aggregate(&rows, Group::Ip, &prices, false),
            models: report::aggregate(&rows, Group::Model, &prices, false),
            keys: report::aggregate(&rows, Group::Key, &prices, false),
        })
    }

    fn rows_for(&self, screen: Screen) -> &[Agg] {
        match screen {
            Screen::Overview => &self.days,
            Screen::Devices => &self.devices,
            Screen::Ips => &self.ips,
            Screen::Models => &self.models,
            Screen::Keys => &self.keys,
        }
    }
}

struct App<'a> {
    config: &'a Config,
    screen: Screen,
    states: [TableState; 5],
    /// Rows that fit in the table body, refreshed on every draw.
    page: usize,
    snapshot: Option<Snapshot>,
    error: Option<String>,
    last_read: Option<i64>,
    last_refresh: Instant,
    dirty: bool,
    quit: bool,
}

impl<'a> App<'a> {
    fn new(config: &'a Config) -> Self {
        Self {
            config,
            screen: Screen::Overview,
            states: Default::default(),
            page: 10,
            snapshot: None,
            error: None,
            last_read: None,
            last_refresh: Instant::now(),
            dirty: false,
            quit: false,
        }
    }

    fn current_rows(&self) -> usize {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.rows_for(self.screen).len())
            .unwrap_or(0)
    }

    fn state(&mut self) -> &mut TableState {
        &mut self.states[self.screen.index()]
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.current_rows();
        if len == 0 {
            self.state().select(None);
            return;
        }
        let last = len as isize - 1;
        let state = self.state();
        let current = state.selected().unwrap_or(0) as isize;
        state.select(Some((current + delta).clamp(0, last) as usize));
    }

    fn select(&mut self, index: usize) {
        let len = self.current_rows();
        if len == 0 {
            self.state().select(None);
        } else {
            self.state().select(Some(index.min(len - 1)));
        }
    }

    fn cycle(&mut self, delta: isize) {
        let count = Screen::ALL.len() as isize;
        let index = (self.screen.index() as isize + delta).rem_euclid(count);
        self.screen = Screen::ALL[index as usize];
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }
        let page = self.page.max(1) as isize;
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('r') => self.dirty = true,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.cycle(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.cycle(-1),
            KeyCode::Char(digit @ '1'..='5') => {
                self.screen = Screen::ALL[digit as usize - '1' as usize];
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::Char('g') | KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => {
                let len = self.current_rows();
                if len > 0 {
                    self.select(len - 1);
                }
            }
            _ => {}
        }
    }

    fn needs_refresh(&self) -> bool {
        self.dirty || self.last_refresh.elapsed() >= REFRESH
    }
}

/// Reloads the snapshot, keeping the previous one (and reporting the error in
/// the footer) when the database cannot be read.
fn reload(conn: &Connection, app: &mut App<'_>) {
    match Snapshot::load(conn, app.config.retention_days) {
        Ok(snapshot) => {
            app.snapshot = Some(snapshot);
            app.error = None;
            app.last_read = Some(crate::now_ms());
            clamp_selection(app);
        }
        Err(err) => app.error = Some(format!("{err:#}")),
    }
    app.last_refresh = Instant::now();
    app.dirty = false;
}

fn clamp_selection(app: &mut App<'_>) {
    let Some(snapshot) = &app.snapshot else {
        return;
    };
    for screen in Screen::ALL {
        let len = snapshot.rows_for(screen).len();
        let state = &mut app.states[screen.index()];
        if len == 0 {
            state.select(None);
        } else if let Some(selected) = state.selected() {
            state.select(Some(selected.min(len - 1)));
        }
    }
}

/// Restores the terminal on every exit path, including errors and panics.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(err) = execute!(io::stdout(), EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(err);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

pub fn run(conn: rusqlite::Connection, config: &crate::config::Config) -> anyhow::Result<()> {
    let mut app = App::new(config);
    reload(&conn, &mut app);
    let _guard = TerminalGuard::enter().context("failed to set up the terminal")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .context("failed to create the terminal")?;
    terminal.clear().context("failed to clear the terminal")?;
    event_loop(&mut terminal, &conn, &mut app)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    conn: &Connection,
    app: &mut App<'_>,
) -> Result<()> {
    while !app.quit {
        terminal
            .draw(|frame| draw(frame, app))
            .context("failed to draw the screen")?;

        let wait = REFRESH.saturating_sub(app.last_refresh.elapsed());
        if event::poll(wait).context("failed to poll for input")? {
            if let Event::Key(key) = event::read().context("failed to read input")? {
                app.handle_key(key);
            }
        }
        if app.needs_refresh() {
            reload(conn, app);
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame<'_>, app: &mut App<'_>) {
    let [header, tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(Paragraph::new(header_line(app)), header);
    frame.render_widget(Paragraph::new(tab_line(app)), tabs);
    match app.screen {
        Screen::Overview => draw_overview(frame, app, body),
        screen => draw_group_table(frame, app, body, screen),
    }
    frame.render_widget(Paragraph::new(footer_line(app)), footer);
}

fn header_line(app: &App<'_>) -> Line<'static> {
    let read = match app.last_read {
        Some(ms) => format!("read {}", clock(ms)),
        None => "no data".to_string(),
    };
    Line::from(vec![
        Span::styled(
            " ai-proxy tui ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("last {} day(s)", app.config.retention_days),
            Style::new().fg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(
            app.config.db_path.display().to_string(),
            Style::new().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled(read, Style::new().fg(Color::DarkGray)),
    ])
}

fn tab_line(app: &App<'_>) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, screen) in Screen::ALL.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let label = format!(" {} {} ", index + 1, screen.title());
        let style = if *screen == app.screen {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

fn footer_line(app: &App<'_>) -> Line<'static> {
    if let Some(error) = &app.error {
        return Line::from(vec![
            Span::styled(
                format!(" error: {error}"),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  · q quit · r retry",
                Style::new().fg(Color::DarkGray),
            ),
        ]);
    }
    Line::from(Span::styled(HINTS, Style::new().fg(Color::DarkGray)))
}

fn draw_overview(frame: &mut Frame<'_>, app: &mut App<'_>, area: Rect) {
    let lines = summary_lines(app);
    let wanted = (lines.len() as u16 + 2).clamp(3, 12);
    let summary_height = wanted.min(area.height.saturating_sub(3).max(3));
    let [summary, table] = Layout::vertical([
        Constraint::Length(summary_height),
        Constraint::Min(3),
    ])
    .areas(area);
    let block = Block::bordered().title(Span::styled(
        " summary ",
        Style::new().add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(lines).block(block), summary);

    let (rows, total) = match &app.snapshot {
        Some(snapshot) => (snapshot.days.clone(), snapshot.totals.clone()),
        None => (Vec::new(), empty_total()),
    };
    draw_table(
        frame,
        app,
        table,
        Group::Day.label(),
        " per day ",
        &rows,
        &total,
    );
}

fn draw_group_table(frame: &mut Frame<'_>, app: &mut App<'_>, area: Rect, screen: Screen) {
    let (rows, total) = match &app.snapshot {
        Some(snapshot) => (
            snapshot.rows_for(screen).to_vec(),
            report::fold(snapshot.rows_for(screen)),
        ),
        None => (Vec::new(), empty_total()),
    };
    draw_table(
        frame,
        app,
        area,
        screen.group().label(),
        &format!(" {} · {} row(s) ", screen.title(), rows.len()),
        &rows,
        &total,
    );
}

fn draw_table(
    frame: &mut Frame<'_>,
    app: &mut App<'_>,
    area: Rect,
    label: &str,
    title: &str,
    rows: &[Agg],
    total: &Agg,
) {
    let mut header = vec![label.to_string()];
    header.extend(HEADERS.iter().map(|header| header.to_string()));
    let header = Row::new(header)
        .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));

    let mut body: Vec<Row<'static>> = rows
        .iter()
        .map(|agg| Row::new(report::agg_row(agg)))
        .collect();
    body.push(
        Row::new(report::agg_row(total)).style(Style::new().add_modifier(Modifier::BOLD)),
    );

    let widths = [
        Constraint::Fill(1),
        Constraint::Length(7),
        Constraint::Length(11),
        Constraint::Length(11),
        Constraint::Length(6),
        Constraint::Length(11),
        Constraint::Length(12),
    ];
    let table = Table::new(body, widths)
        .header(header)
        .column_spacing(1)
        .block(Block::bordered().title(Span::styled(
            title.to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        )))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    app.page = area.height.saturating_sub(3).max(1) as usize;
    let state = &mut app.states[app.screen.index()];
    frame.render_stateful_widget(table, area, state);
}

fn summary_lines(app: &App<'_>) -> Vec<Line<'static>> {
    let Some(snapshot) = &app.snapshot else {
        return vec![Line::from(Span::styled(
            "  no data yet",
            Style::new().fg(Color::DarkGray),
        ))];
    };
    let total = &snapshot.totals;
    vec![
        summary_line("requests", report::fmt_int(total.requests)),
        summary_line("input", report::fmt_int(total.input)),
        summary_line(
            "cached",
            format!(
                "{}  (hit {})",
                report::fmt_int(total.cached),
                report::fmt_rate(total.hit_rate())
            ),
        ),
        summary_line("output", report::fmt_int(total.output)),
        summary_line("cost", report::fmt_cost(total)),
        summary_line("cache savings", report::fmt_usd(total.saved)),
    ]
    .into_iter()
    .chain(total.unpriced.iter().next().map(|_| {
        summary_line(
            "unpriced",
            total.unpriced.iter().cloned().collect::<Vec<_>>().join(", "),
        )
    }))
    .collect()
}

fn summary_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<14}"), Style::new().fg(Color::Gray)),
        Span::styled(value, Style::new().fg(Color::White)),
    ])
}

fn empty_total() -> Agg {
    Agg {
        label: "total".to_string(),
        ..Agg::default()
    }
}

fn clock(ms: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(ms / 1000) {
        Ok(value) => format!(
            "{:02}:{:02}:{:02} UTC",
            value.hour(),
            value.minute(),
            value.second()
        ),
        Err(_) => "--:--:-- UTC".to_string(),
    }
}
