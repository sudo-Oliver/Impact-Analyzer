use std::io;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};

use crate::graph::ImpactGraph;
use crate::parser::{Action, Plan};

// ── App state ─────────────────────────────────────────────────────────────────

struct App {
    plan: Plan,
    graph: ImpactGraph,
    list_state: ListState,
}

impl App {
    fn new(plan: Plan, graph: ImpactGraph) -> Self {
        let mut list_state = ListState::default();
        if !plan.resource_changes.is_empty() {
            list_state.select(Some(0));
        }
        Self { plan, graph, list_state }
    }

    fn select_next(&mut self) {
        let len = self.plan.resource_changes.len();
        if len == 0 { return; }
        let next = self.list_state.selected().map_or(0, |i| (i + 1) % len);
        self.list_state.select(Some(next));
    }

    fn select_prev(&mut self) {
        let len = self.plan.resource_changes.len();
        if len == 0 { return; }
        let prev = self.list_state.selected().map_or(0, |i| {
            if i == 0 { len - 1 } else { i - 1 }
        });
        self.list_state.select(Some(prev));
    }

    fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        loop {
            terminal.draw(|f| self.render(f))?;

            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Up   | KeyCode::Char('k') => self.select_prev(),
                        KeyCode::Down | KeyCode::Char('j') => self.select_next(),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn render(&mut self, f: &mut Frame) {
        let area = f.size();

        // Vertical: header (3) / body (fill) / footer (1)
        let vchunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        // Horizontal body: 38% resource list / 62% detail panel
        let hchunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(38),
                Constraint::Percentage(62),
            ])
            .split(vchunks[1]);

        self.render_header(f, vchunks[0]);
        self.render_list(f, hchunks[0]);
        self.render_details(f, hchunks[1]);
        self.render_footer(f, vchunks[2]);
    }

    // ── Header ────────────────────────────────────────────────────────────────

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let total  = self.plan.resource_changes.len();
        let active = self.plan.resource_changes.iter()
            .filter(|rc| !rc.change.is_noop())
            .count();
        let drift  = self.plan.resource_drift.len();

        let lines = vec![
            Line::from(vec![
                Span::styled(
                    " eia  Enterprise Impact Analyzer",
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{} resource(s)", total),
                    Style::default().fg(Color::White),
                ),
                Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{} change(s)", active),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{} drift(s)", drift),
                    Style::default().fg(if drift > 0 { Color::Red } else { Color::Green }),
                ),
            ]),
        ];

        let para = Paragraph::new(lines)
            .style(Style::default().bg(Color::DarkGray));
        f.render_widget(para, area);
    }

    // ── Resource list (left panel) ────────────────────────────────────────────

    fn render_list(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self.plan.resource_changes
            .iter()
            .map(|rc| {
                let actions = &rc.change.actions;
                let color   = action_color(actions);
                let sigil   = action_sigil(actions);
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {} ", sigil), Style::default().fg(color)),
                    Span::styled(rc.address.clone(), Style::default().fg(color)),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Resources "),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ")
            .highlight_spacing(HighlightSpacing::Always);

        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    // ── Detail panel (right panel) ────────────────────────────────────────────

    fn render_details(&self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" Details ");

        if self.plan.resource_changes.is_empty() {
            let para = Paragraph::new("No resources in this plan.")
                .block(block)
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(para, area);
            return;
        }

        let idx = self.list_state.selected().unwrap_or(0);
        let rc  = &self.plan.resource_changes[idx];

        let actions     = &rc.change.actions;
        let action_strs = actions.iter().map(action_label).collect::<Vec<_>>().join(" + ");
        let color       = action_color(actions);

        let mut blast = self.graph.blast_radius(&rc.address);
        blast.sort();

        let bold = Style::default().add_modifier(Modifier::BOLD);
        let dim  = Style::default().fg(Color::DarkGray);

        let mut lines: Vec<Line> = vec![
            Line::from(vec![
                Span::styled("Address   ", bold),
                Span::raw(rc.address.clone()),
            ]),
            Line::from(vec![
                Span::styled("Type      ", bold),
                Span::raw(rc.resource_type.clone().unwrap_or_else(|| "—".into())),
            ]),
            Line::from(vec![
                Span::styled("Provider  ", bold),
                Span::raw(rc.provider_name.clone().unwrap_or_else(|| "—".into())),
            ]),
            Line::from(vec![
                Span::styled("Action    ", bold),
                Span::styled(
                    action_strs,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::raw(""),
        ];

        // ── Blast radius ──────────────────────────────────────────────────────
        let blast_color = if blast.is_empty() { Color::Green } else { Color::Yellow };

        lines.push(Line::from(vec![
            Span::styled(
                format!("Blast radius  ({} affected)", blast.len()),
                Style::default().fg(blast_color).add_modifier(Modifier::BOLD),
            ),
        ]));

        if blast.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  No downstream dependencies.", dim),
            ]));
        } else {
            for addr in &blast {
                lines.push(Line::from(vec![
                    Span::styled("  ↳ ", Style::default().fg(Color::Yellow)),
                    Span::raw(addr.as_str()),
                ]));
            }
        }

        let para = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });

        f.render_widget(para, area);
    }

    // ── Footer ────────────────────────────────────────────────────────────────

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let para = Paragraph::new(
            "  ↑ ↓  or  j k   Navigate    q / Esc   Quit",
        )
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));
        f.render_widget(para, area);
    }
}

// ── Action helpers ─────────────────────────────────────────────────────────────

fn action_color(actions: &[Action]) -> Color {
    let del = actions.iter().any(|a| matches!(a, Action::Delete | Action::Forget));
    let cre = actions.iter().any(|a| matches!(a, Action::Create));
    match (del, cre) {
        (true, true)  => Color::Magenta,
        (true, false) => Color::Red,
        (false, true) => Color::Green,
        _ if actions.iter().any(|a| matches!(a, Action::Update)) => Color::Yellow,
        _ if actions.iter().any(|a| matches!(a, Action::Read))   => Color::Cyan,
        _                                                          => Color::DarkGray,
    }
}

fn action_sigil(actions: &[Action]) -> char {
    let del = actions.iter().any(|a| matches!(a, Action::Delete | Action::Forget));
    let cre = actions.iter().any(|a| matches!(a, Action::Create));
    match (del, cre) {
        (true, true)  => '±',
        (true, false) => '-',
        (false, true) => '+',
        _ if actions.iter().any(|a| matches!(a, Action::Update)) => '~',
        _                                                          => ' ',
    }
}

fn action_label(action: &Action) -> &'static str {
    match action {
        Action::NoOp   => "no-op",
        Action::Create => "create",
        Action::Read   => "read",
        Action::Update => "update",
        Action::Delete => "delete",
        Action::Forget => "forget",
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Launch the interactive TUI. Restores terminal state even if an error occurs.
pub fn run_tui(plan: Plan) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend      = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let graph    = ImpactGraph::build(&plan);
    let mut app  = App::new(plan, graph);
    let result   = app.run(&mut terminal);

    // Always restore terminal state so the shell prompt appears correctly.
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    result
}
