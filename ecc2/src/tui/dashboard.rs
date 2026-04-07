use std::collections::HashMap;
use std::path::Path;

use ratatui::{
    prelude::*,
    widgets::{
        Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Tabs, Wrap,
    },
};
use tokio::sync::broadcast;

use super::widgets::{budget_state, format_currency, format_token_count, BudgetState, TokenMeter};
use crate::comms;
use crate::config::{Config, PaneLayout};
use crate::observability::ToolLogEntry;
use crate::session::output::{OutputEvent, OutputLine, SessionOutputStore, OutputStream, OUTPUT_BUFFER_LIMIT};
use crate::session::manager;
use crate::session::store::StateStore;
use crate::session::{Session, SessionMessage, SessionMetrics, SessionState, WorktreeInfo};
use crate::worktree;

const DEFAULT_PANE_SIZE_PERCENT: u16 = 35;
const DEFAULT_GRID_SIZE_PERCENT: u16 = 50;
const OUTPUT_PANE_PERCENT: u16 = 70;
const MIN_PANE_SIZE_PERCENT: u16 = 20;
const MAX_PANE_SIZE_PERCENT: u16 = 80;
const PANE_RESIZE_STEP_PERCENT: u16 = 5;
const MAX_LOG_ENTRIES: u64 = 12;

pub struct Dashboard {
    db: StateStore,
    cfg: Config,
    output_store: SessionOutputStore,
    output_rx: broadcast::Receiver<OutputEvent>,
    sessions: Vec<Session>,
    session_output_cache: HashMap<String, Vec<OutputLine>>,
    unread_message_counts: HashMap<String, usize>,
    selected_messages: Vec<SessionMessage>,
    selected_parent_session: Option<String>,
    selected_child_sessions: Vec<DelegatedChildSummary>,
    selected_team_summary: Option<TeamSummary>,
    logs: Vec<ToolLogEntry>,
    selected_diff_summary: Option<String>,
    selected_pane: Pane,
    selected_session: usize,
    show_help: bool,
    operator_note: Option<String>,
    output_follow: bool,
    output_scroll_offset: usize,
    last_output_height: usize,
    pane_size_percent: u16,
    session_table_state: TableState,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SessionSummary {
    total: usize,
    pending: usize,
    running: usize,
    idle: usize,
    completed: usize,
    failed: usize,
    stopped: usize,
    unread_messages: usize,
    inbox_sessions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Pane {
    Sessions,
    Output,
    Metrics,
    Log,
}

#[derive(Debug, Clone, Copy)]
struct PaneAreas {
    sessions: Rect,
    output: Rect,
    metrics: Rect,
    log: Option<Rect>,
}

#[derive(Debug, Clone, Copy)]
struct AggregateUsage {
    total_tokens: u64,
    total_cost_usd: f64,
    token_state: BudgetState,
    cost_state: BudgetState,
    overall_state: BudgetState,
}

#[derive(Debug, Clone)]
struct DelegatedChildSummary {
    session_id: String,
    state: SessionState,
    unread_messages: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TeamSummary {
    total: usize,
    idle: usize,
    running: usize,
    pending: usize,
    failed: usize,
    stopped: usize,
}

impl Dashboard {
    pub fn new(db: StateStore, cfg: Config) -> Self {
        Self::with_output_store(db, cfg, SessionOutputStore::default())
    }

    pub fn with_output_store(db: StateStore, cfg: Config, output_store: SessionOutputStore) -> Self {
        let pane_size_percent = match cfg.pane_layout {
            PaneLayout::Grid => DEFAULT_GRID_SIZE_PERCENT,
            PaneLayout::Horizontal | PaneLayout::Vertical => DEFAULT_PANE_SIZE_PERCENT,
        };
        let sessions = db.list_sessions().unwrap_or_default();
        let output_rx = output_store.subscribe();
        let mut session_table_state = TableState::default();
        if !sessions.is_empty() {
            session_table_state.select(Some(0));
        }

        let mut dashboard = Self {
            db,
            cfg,
            output_store,
            output_rx,
            sessions,
            session_output_cache: HashMap::new(),
            unread_message_counts: HashMap::new(),
            selected_messages: Vec::new(),
            selected_parent_session: None,
            selected_child_sessions: Vec::new(),
            selected_team_summary: None,
            logs: Vec::new(),
            selected_diff_summary: None,
            selected_pane: Pane::Sessions,
            selected_session: 0,
            show_help: false,
            operator_note: None,
            output_follow: true,
            output_scroll_offset: 0,
            last_output_height: 0,
            pane_size_percent,
            session_table_state,
        };
        dashboard.unread_message_counts = dashboard.db.unread_message_counts().unwrap_or_default();
        dashboard.sync_selected_output();
        dashboard.sync_selected_diff();
        dashboard.sync_selected_messages();
        dashboard.sync_selected_lineage();
        dashboard.refresh_logs();
        dashboard
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(frame.area());

        self.render_header(frame, chunks[0]);

        if self.show_help {
            self.render_help(frame, chunks[1]);
        } else {
            let pane_areas = self.pane_areas(chunks[1]);
            self.render_sessions(frame, pane_areas.sessions);
            self.render_output(frame, pane_areas.output);
            self.render_metrics(frame, pane_areas.metrics);

            if let Some(log_area) = pane_areas.log {
                self.render_log(frame, log_area);
            }
        }

        self.render_status_bar(frame, chunks[2]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let running = self
            .sessions
            .iter()
            .filter(|session| session.state == SessionState::Running)
            .count();
        let total = self.sessions.len();

        let title = format!(
            " ECC 2.0 | {running} running / {total} total | {} {}% ",
            self.layout_label(),
            self.pane_size_percent
        );
        let tabs = Tabs::new(
            self.visible_panes()
                .iter()
                .map(|pane| pane.title())
                .collect::<Vec<_>>(),
        )
            .block(Block::default().borders(Borders::ALL).title(title))
            .select(self.selected_pane_index())
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(tabs, area);
    }

    fn render_sessions(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Sessions ")
            .border_style(self.pane_border_style(Pane::Sessions));
        let inner_area = block.inner(area);
        frame.render_widget(block, area);

        if inner_area.is_empty() {
            return;
        }

        let summary = SessionSummary::from_sessions(&self.sessions, &self.unread_message_counts);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(3)])
            .split(inner_area);

        frame.render_widget(
            Paragraph::new(vec![summary_line(&summary), attention_queue_line(&summary)]),
            chunks[0],
        );

        let rows = self.sessions.iter().map(|session| {
            session_row(
                session,
                self.unread_message_counts
                    .get(&session.id)
                    .copied()
                    .unwrap_or(0),
            )
        });
        let header = Row::new(["ID", "Agent", "State", "Branch", "Inbox", "Tokens", "Duration"])
            .style(Style::default().add_modifier(Modifier::BOLD));
        let widths = [
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(12),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(8),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(1)
            .highlight_symbol(">> ")
            .highlight_spacing(HighlightSpacing::Always)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );

        let selected = if self.sessions.is_empty() {
            None
        } else {
            Some(self.selected_session.min(self.sessions.len() - 1))
        };
        if self.session_table_state.selected() != selected {
            self.session_table_state.select(selected);
        }

        frame.render_stateful_widget(table, chunks[1], &mut self.session_table_state);
    }

    fn render_output(&mut self, frame: &mut Frame, area: Rect) {
        self.sync_output_scroll(area.height.saturating_sub(2) as usize);

        let content = if self.sessions.get(self.selected_session).is_some() {
            let lines = self.selected_output_lines();

            if lines.is_empty() {
                "Waiting for session output...".to_string()
            } else {
                lines
                    .iter()
                    .map(|line| line.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        } else {
            "No sessions. Press 'n' to start one.".to_string()
        };

        let paragraph = Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Output ")
                    .border_style(self.pane_border_style(Pane::Output)),
            )
            .scroll((self.output_scroll_offset as u16, 0));
        frame.render_widget(paragraph, area);
    }

    fn render_metrics(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Metrics ")
            .border_style(self.pane_border_style(Pane::Metrics));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.is_empty() {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(1),
            ])
            .split(inner);

        let aggregate = self.aggregate_usage();
        frame.render_widget(
            TokenMeter::tokens(
                "Token Budget",
                aggregate.total_tokens,
                self.cfg.token_budget,
            ),
            chunks[0],
        );
        frame.render_widget(
            TokenMeter::currency(
                "Cost Budget",
                aggregate.total_cost_usd,
                self.cfg.cost_budget_usd,
            ),
            chunks[1],
        );
        frame.render_widget(
            Paragraph::new(self.selected_session_metrics_text()).wrap(Wrap { trim: true }),
            chunks[2],
        );
    }

    fn render_log(&self, frame: &mut Frame, area: Rect) {
        let content = if self.sessions.get(self.selected_session).is_none() {
            "No session selected.".to_string()
        } else if self.logs.is_empty() {
            "No tool logs available for this session yet.".to_string()
        } else {
            self.logs
                .iter()
                .map(|entry| {
                    format!(
                        "[{}] {} | {}ms | risk {:.0}%\ninput: {}\noutput: {}",
                        self.short_timestamp(&entry.timestamp),
                        entry.tool_name,
                        entry.duration_ms,
                        entry.risk_score * 100.0,
                        self.log_field(&entry.input_summary),
                        self.log_field(&entry.output_summary)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        };

        let paragraph = Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Log ")
                    .border_style(self.pane_border_style(Pane::Log)),
            )
            .scroll((self.output_scroll_offset as u16, 0))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let text = format!(
            " [n]ew session  [a]ssign  dra[i]n inbox  [g]lobal dispatch  [s]top  [u]resume  [x]cleanup  [d]elete  [r]efresh  [Tab] switch pane  [j/k] scroll  [+/-] resize  [{}] layout  [?] help  [q]uit ",
            self.layout_label()
        );
        let text = if let Some(note) = self.operator_note.as_ref() {
            format!(" {} |{}", truncate_for_dashboard(note, 96), text)
        } else {
            text
        };
        let aggregate = self.aggregate_usage();
        let (summary_text, summary_style) = self.aggregate_cost_summary();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(aggregate.overall_state.style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.is_empty() {
            return;
        }

        let summary_width = summary_text
            .len()
            .min(inner.width.saturating_sub(1) as usize) as u16;
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(summary_width)])
            .split(inner);

        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(summary_text)
                .style(summary_style)
                .alignment(Alignment::Right),
            chunks[1],
        );
    }

    fn render_help(&self, frame: &mut Frame, area: Rect) {
        let help = vec![
            "Keyboard Shortcuts:",
            "",
            "  n       New session",
            "  a       Assign follow-up work from selected session",
            "  i       Drain unread task handoffs from selected session inbox",
            "  g       Auto-dispatch unread handoffs across lead sessions",
            "  s       Stop selected session",
            "  u       Resume selected session",
            "  x       Cleanup selected worktree",
            "  d       Delete selected inactive session",
            "  Tab     Next pane",
            "  S-Tab   Previous pane",
            "  j/↓     Scroll down",
            "  k/↑     Scroll up",
            "  +/=     Increase pane size",
            "  -       Decrease pane size",
            "  r       Refresh",
            "  ?       Toggle help",
            "  q/C-c   Quit",
        ];

        let paragraph = Paragraph::new(help.join("\n")).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Help ")
                .border_style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(paragraph, area);
    }

    pub fn next_pane(&mut self) {
        let visible_panes = self.visible_panes();
        let next_index = self
            .selected_pane_index()
            .checked_add(1)
            .map(|index| index % visible_panes.len())
            .unwrap_or(0);

        self.selected_pane = visible_panes[next_index];
    }

    pub fn prev_pane(&mut self) {
        let visible_panes = self.visible_panes();
        let previous_index = if self.selected_pane_index() == 0 {
            visible_panes.len() - 1
        } else {
            self.selected_pane_index() - 1
        };

        self.selected_pane = visible_panes[previous_index];
    }

    pub fn increase_pane_size(&mut self) {
        self.pane_size_percent =
            (self.pane_size_percent + PANE_RESIZE_STEP_PERCENT).min(MAX_PANE_SIZE_PERCENT);
    }

    pub fn decrease_pane_size(&mut self) {
        self.pane_size_percent = self
            .pane_size_percent
            .saturating_sub(PANE_RESIZE_STEP_PERCENT)
            .max(MIN_PANE_SIZE_PERCENT);
    }

    pub fn scroll_down(&mut self) {
        match self.selected_pane {
            Pane::Sessions if !self.sessions.is_empty() => {
                self.selected_session = (self.selected_session + 1).min(self.sessions.len() - 1);
                self.sync_selection();
                self.reset_output_view();
                self.sync_selected_output();
                self.sync_selected_diff();
                self.sync_selected_messages();
                self.sync_selected_lineage();
                self.refresh_logs();
            }
            Pane::Output => {
                let max_scroll = self.max_output_scroll();
                if self.output_follow {
                    return;
                }

                if self.output_scroll_offset >= max_scroll.saturating_sub(1) {
                    self.output_follow = true;
                    self.output_scroll_offset = max_scroll;
                } else {
                    self.output_scroll_offset = self.output_scroll_offset.saturating_add(1);
                }
            }
            Pane::Metrics => {}
            Pane::Log => {
                self.output_follow = false;
                self.output_scroll_offset = self.output_scroll_offset.saturating_add(1);
            }
            Pane::Sessions => {}
        }
    }

    pub fn scroll_up(&mut self) {
        match self.selected_pane {
            Pane::Sessions => {
                self.selected_session = self.selected_session.saturating_sub(1);
                self.sync_selection();
                self.reset_output_view();
                self.sync_selected_output();
                self.sync_selected_diff();
                self.sync_selected_messages();
                self.sync_selected_lineage();
                self.refresh_logs();
            }
            Pane::Output => {
                if self.output_follow {
                    self.output_follow = false;
                    self.output_scroll_offset = self.max_output_scroll();
                }

                self.output_scroll_offset = self.output_scroll_offset.saturating_sub(1);
            }
            Pane::Metrics => {}
            Pane::Log => {
                self.output_follow = false;
                self.output_scroll_offset = self.output_scroll_offset.saturating_sub(1);
            }
        }
    }

    pub async fn new_session(&mut self) {
        if self.active_session_count() >= self.cfg.max_parallel_sessions {
            tracing::warn!(
                "Cannot queue new session: active session limit reached ({})",
                self.cfg.max_parallel_sessions
            );
            return;
        }

        let task = self.new_session_task();
        let agent = self.cfg.default_agent.clone();

        let session_id = match manager::create_session(&self.db, &self.cfg, &task, &agent, true).await {
            Ok(session_id) => session_id,
            Err(error) => {
                tracing::warn!("Failed to create new session from dashboard: {error}");
                self.set_operator_note(format!("new session failed: {error}"));
                return;
            }
        };

        if let Some(source_session) = self.sessions.get(self.selected_session) {
            let context = format!(
                "Dashboard handoff from {} [{}] | cwd {}{}",
                format_session_id(&source_session.id),
                source_session.agent_type,
                source_session.working_dir.display(),
                source_session
                    .worktree
                    .as_ref()
                    .map(|worktree| format!(
                        " | worktree {} ({})",
                        worktree.branch,
                        worktree.path.display()
                    ))
                    .unwrap_or_default()
            );
            if let Err(error) = comms::send(
                &self.db,
                &source_session.id,
                &session_id,
                &comms::MessageType::TaskHandoff {
                    task: source_session.task.clone(),
                    context,
                },
            ) {
                tracing::warn!(
                    "Failed to send handoff from session {} to {}: {error}",
                    source_session.id,
                    session_id
                );
            }
        }

        self.refresh();
        self.sync_selection_by_id(Some(&session_id));
        self.set_operator_note(format!("spawned session {}", format_session_id(&session_id)));
        self.reset_output_view();
        self.sync_selected_output();
        self.sync_selected_diff();
        self.sync_selected_messages();
        self.sync_selected_lineage();
        self.refresh_logs();
    }

    pub async fn assign_selected(&mut self) {
        let Some(source_session) = self.sessions.get(self.selected_session) else {
            return;
        };

        let task = self.new_session_task();
        let agent = self.cfg.default_agent.clone();

        let outcome = match manager::assign_session(
            &self.db,
            &self.cfg,
            &source_session.id,
            &task,
            &agent,
            true,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(
                    "Failed to assign follow-up work from session {}: {error}",
                    source_session.id
                );
                self.set_operator_note(format!("assignment failed: {error}"));
                return;
            }
        };

        self.refresh();
        self.sync_selection_by_id(Some(&outcome.session_id));
        self.set_operator_note(format!(
            "assigned via {} -> {}",
            assignment_action_label(outcome.action),
            format_session_id(&outcome.session_id)
        ));
        self.reset_output_view();
        self.sync_selected_output();
        self.sync_selected_diff();
        self.sync_selected_messages();
        self.sync_selected_lineage();
        self.refresh_logs();
    }

    pub async fn drain_inbox_selected(&mut self) {
        let Some(source_session) = self.sessions.get(self.selected_session) else {
            return;
        };

        let agent = self.cfg.default_agent.clone();
        let source_session_id = source_session.id.clone();

        let outcomes = match manager::drain_inbox(
            &self.db,
            &self.cfg,
            &source_session_id,
            &agent,
            true,
            self.cfg.max_parallel_sessions,
        )
        .await
        {
            Ok(outcomes) => outcomes,
            Err(error) => {
                tracing::warn!(
                    "Failed to drain inbox for session {}: {error}",
                    source_session_id
                );
                self.set_operator_note(format!(
                    "drain inbox failed for {}: {error}",
                    format_session_id(&source_session_id)
                ));
                return;
            }
        };

        self.refresh();
        self.sync_selection_by_id(Some(&source_session_id));
        self.sync_selected_output();
        self.sync_selected_diff();
        self.sync_selected_messages();
        self.sync_selected_lineage();
        self.refresh_logs();

        if outcomes.is_empty() {
            self.set_operator_note(format!(
                "no unread handoffs for {}",
                format_session_id(&source_session_id)
            ));
        } else {
            self.set_operator_note(format!(
                "drained {} handoff(s) from {}",
                outcomes.len(),
                format_session_id(&source_session_id)
            ));
        }
    }

    pub async fn auto_dispatch_backlog(&mut self) {
        let agent = self.cfg.default_agent.clone();
        let lead_limit = self.sessions.len().max(1);

        let outcomes = match manager::auto_dispatch_backlog(
            &self.db,
            &self.cfg,
            &agent,
            true,
            lead_limit,
        )
        .await
        {
            Ok(outcomes) => outcomes,
            Err(error) => {
                tracing::warn!("Failed to auto-dispatch backlog from dashboard: {error}");
                self.set_operator_note(format!("global auto-dispatch failed: {error}"));
                return;
            }
        };

        let total_routed: usize = outcomes.iter().map(|outcome| outcome.routed.len()).sum();
        let selected_session_id = self
            .sessions
            .get(self.selected_session)
            .map(|session| session.id.clone());

        self.refresh();
        self.sync_selection_by_id(selected_session_id.as_deref());
        self.sync_selected_output();
        self.sync_selected_diff();
        self.sync_selected_messages();
        self.sync_selected_lineage();
        self.refresh_logs();

        if total_routed == 0 {
            self.set_operator_note("no unread handoff backlog found".to_string());
        } else {
            self.set_operator_note(format!(
                "auto-dispatched {} handoff(s) across {} lead session(s)",
                total_routed,
                outcomes.len()
            ));
        }
    }

    pub async fn stop_selected(&mut self) {
        let Some(session) = self.sessions.get(self.selected_session) else {
            return;
        };

        let session_id = session.id.clone();
        if let Err(error) = manager::stop_session(&self.db, &session_id).await {
            tracing::warn!("Failed to stop session {}: {error}", session.id);
            self.set_operator_note(format!("stop failed for {}: {error}", format_session_id(&session_id)));
            return;
        }

        self.refresh();
        self.set_operator_note(format!("stopped session {}", format_session_id(&session_id)));
    }

    pub async fn resume_selected(&mut self) {
        let Some(session) = self.sessions.get(self.selected_session) else {
            return;
        };

        let session_id = session.id.clone();
        if let Err(error) = manager::resume_session(&self.db, &self.cfg, &session_id).await {
            tracing::warn!("Failed to resume session {}: {error}", session.id);
            self.set_operator_note(format!("resume failed for {}: {error}", format_session_id(&session_id)));
            return;
        }

        self.refresh();
        self.set_operator_note(format!("resumed session {}", format_session_id(&session_id)));
    }

    pub async fn cleanup_selected_worktree(&mut self) {
        let Some(session) = self.sessions.get(self.selected_session) else {
            return;
        };

        if session.worktree.is_none() {
            return;
        }

        let session_id = session.id.clone();
        if let Err(error) = manager::cleanup_session_worktree(&self.db, &session_id).await {
            tracing::warn!("Failed to cleanup session {} worktree: {error}", session.id);
            self.set_operator_note(format!(
                "cleanup failed for {}: {error}",
                format_session_id(&session_id)
            ));
            return;
        }

        self.refresh();
        self.set_operator_note(format!("cleaned worktree for {}", format_session_id(&session_id)));
    }

    pub async fn delete_selected_session(&mut self) {
        let Some(session) = self.sessions.get(self.selected_session) else {
            return;
        };

        let session_id = session.id.clone();
        if let Err(error) = manager::delete_session(&self.db, &session_id).await {
            tracing::warn!("Failed to delete session {}: {error}", session.id);
            self.set_operator_note(format!("delete failed for {}: {error}", format_session_id(&session_id)));
            return;
        }

        self.refresh();
        self.set_operator_note(format!("deleted session {}", format_session_id(&session_id)));
    }

    pub fn refresh(&mut self) {
        self.sync_from_store();
    }

    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    pub async fn tick(&mut self) {
        loop {
            match self.output_rx.try_recv() {
                Ok(_event) => {}
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }

        self.sync_from_store();
    }

    fn sync_from_store(&mut self) {
        let selected_id = self.selected_session_id().map(ToOwned::to_owned);
        self.sessions = match self.db.list_sessions() {
            Ok(sessions) => sessions,
            Err(error) => {
                tracing::warn!("Failed to refresh sessions: {error}");
                Vec::new()
            }
        };
        self.unread_message_counts = match self.db.unread_message_counts() {
            Ok(counts) => counts,
            Err(error) => {
                tracing::warn!("Failed to refresh unread message counts: {error}");
                HashMap::new()
            }
        };
        self.sync_selection_by_id(selected_id.as_deref());
        self.ensure_selected_pane_visible();
        self.sync_selected_output();
        self.sync_selected_diff();
        self.sync_selected_messages();
        self.sync_selected_lineage();
        self.refresh_logs();
    }

    fn sync_selection(&mut self) {
        if self.sessions.is_empty() {
            self.selected_session = 0;
            self.session_table_state.select(None);
        } else {
            self.selected_session = self.selected_session.min(self.sessions.len() - 1);
            self.session_table_state.select(Some(self.selected_session));
        }
    }

    fn sync_selection_by_id(&mut self, selected_id: Option<&str>) {
        if let Some(selected_id) = selected_id {
            if let Some(index) = self.sessions.iter().position(|session| session.id == selected_id) {
                self.selected_session = index;
            }
        }
        self.sync_selection();
    }

    fn ensure_selected_pane_visible(&mut self) {
        if !self.visible_panes().contains(&self.selected_pane) {
            self.selected_pane = Pane::Sessions;
        }
    }

    fn sync_selected_output(&mut self) {
        let Some(session_id) = self.selected_session_id().map(ToOwned::to_owned) else {
            self.output_scroll_offset = 0;
            self.output_follow = true;
            return;
        };

        match self.db.get_output_lines(&session_id, OUTPUT_BUFFER_LIMIT) {
            Ok(lines) => {
                self.output_store.replace_lines(&session_id, lines.clone());
                self.session_output_cache.insert(session_id, lines);
            }
            Err(error) => {
                tracing::warn!("Failed to load session output: {error}");
            }
        }
    }

    fn sync_selected_diff(&mut self) {
        self.selected_diff_summary = self
            .sessions
            .get(self.selected_session)
            .and_then(|session| session.worktree.as_ref())
            .and_then(|worktree| worktree::diff_summary(worktree).ok().flatten());
    }

    fn sync_selected_messages(&mut self) {
        let Some(session_id) = self.selected_session_id().map(ToOwned::to_owned) else {
            self.selected_messages.clear();
            return;
        };

        let unread_count = self.unread_message_counts.get(&session_id).copied().unwrap_or(0);
        if unread_count > 0 {
            match self.db.mark_messages_read(&session_id) {
                Ok(_) => {
                    self.unread_message_counts.insert(session_id.clone(), 0);
                }
                Err(error) => {
                    tracing::warn!(
                        "Failed to mark session {} messages as read: {error}",
                        session_id
                    );
                }
            }
        }

        self.selected_messages = match self.db.list_messages_for_session(&session_id, 5) {
            Ok(messages) => messages,
            Err(error) => {
                tracing::warn!("Failed to load session messages: {error}");
                Vec::new()
            }
        };
    }

    fn sync_selected_lineage(&mut self) {
        let Some(session_id) = self.selected_session_id().map(ToOwned::to_owned) else {
            self.selected_parent_session = None;
            self.selected_child_sessions.clear();
            self.selected_team_summary = None;
            return;
        };

        self.selected_parent_session = match self.db.latest_task_handoff_source(&session_id) {
            Ok(parent) => parent,
            Err(error) => {
                tracing::warn!("Failed to load session parent linkage: {error}");
                None
            }
        };

        self.selected_child_sessions = match self.db.delegated_children(&session_id, 50) {
            Ok(children) => {
                let mut delegated = Vec::new();
                let mut team = TeamSummary::default();

                for child_id in children {
                    match self.db.get_session(&child_id) {
                        Ok(Some(session)) => {
                            team.total += 1;
                            match session.state {
                                SessionState::Idle => team.idle += 1,
                                SessionState::Running => team.running += 1,
                                SessionState::Pending => team.pending += 1,
                                SessionState::Failed => team.failed += 1,
                                SessionState::Stopped => team.stopped += 1,
                                SessionState::Completed => {}
                            }

                            delegated.push(DelegatedChildSummary {
                                unread_messages: self
                                    .unread_message_counts
                                    .get(&child_id)
                                    .copied()
                                    .unwrap_or(0),
                                state: session.state,
                                session_id: child_id,
                            });
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(
                                "Failed to load delegated child session {}: {error}",
                                child_id
                            );
                        }
                    }
                }

                self.selected_team_summary = if team.total > 0 { Some(team) } else { None };
                delegated.truncate(3);
                delegated
            }
            Err(error) => {
                tracing::warn!("Failed to load delegated child sessions: {error}");
                self.selected_team_summary = None;
                Vec::new()
            }
        };
    }

    fn selected_session_id(&self) -> Option<&str> {
        self.sessions
            .get(self.selected_session)
            .map(|session| session.id.as_str())
    }

    fn selected_output_lines(&self) -> &[OutputLine] {
        self.selected_session_id()
            .and_then(|session_id| self.session_output_cache.get(session_id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn sync_output_scroll(&mut self, viewport_height: usize) {
        self.last_output_height = viewport_height.max(1);
        let max_scroll = self.max_output_scroll();

        if self.output_follow {
            self.output_scroll_offset = max_scroll;
        } else {
            self.output_scroll_offset = self.output_scroll_offset.min(max_scroll);
        }
    }

    fn max_output_scroll(&self) -> usize {
        self.selected_output_lines()
            .len()
            .saturating_sub(self.last_output_height.max(1))
    }

    fn reset_output_view(&mut self) {
        self.output_follow = true;
        self.output_scroll_offset = 0;
    }

    fn refresh_logs(&mut self) {
        let Some(session_id) = self.selected_session_id().map(ToOwned::to_owned) else {
            self.logs.clear();
            return;
        };

        match self.db.query_tool_logs(&session_id, 1, MAX_LOG_ENTRIES) {
            Ok(page) => self.logs = page.entries,
            Err(error) => {
                tracing::warn!("Failed to load tool logs: {error}");
                self.logs.clear();
            }
        }
    }

    fn aggregate_usage(&self) -> AggregateUsage {
        let total_tokens = self
            .sessions
            .iter()
            .map(|session| session.metrics.tokens_used)
            .sum();
        let total_cost_usd = self
            .sessions
            .iter()
            .map(|session| session.metrics.cost_usd)
            .sum::<f64>();
        let token_state = budget_state(total_tokens as f64, self.cfg.token_budget as f64);
        let cost_state = budget_state(total_cost_usd, self.cfg.cost_budget_usd);

        AggregateUsage {
            total_tokens,
            total_cost_usd,
            token_state,
            cost_state,
            overall_state: token_state.max(cost_state),
        }
    }

    fn selected_session_metrics_text(&self) -> String {
        if let Some(session) = self.sessions.get(self.selected_session) {
            let metrics = &session.metrics;
            let mut lines = vec![
                format!(
                    "Selected {} [{}]",
                    &session.id[..8.min(session.id.len())],
                    session.state
                ),
                format!("Task {}", session.task),
            ];

            if let Some(parent) = self.selected_parent_session.as_ref() {
                lines.push(format!("Delegated from {}", format_session_id(parent)));
            }

            if let Some(team) = self.selected_team_summary {
                lines.push(format!(
                    "Team {}/{} | idle {} | running {} | pending {} | failed {} | stopped {}",
                    team.total,
                    self.cfg.max_parallel_sessions,
                    team.idle,
                    team.running,
                    team.pending,
                    team.failed,
                    team.stopped
                ));
            }

            if !self.selected_child_sessions.is_empty() {
                lines.push("Delegates".to_string());
                for child in &self.selected_child_sessions {
                    lines.push(format!(
                        "- {} [{}] | inbox {}",
                        format_session_id(&child.session_id),
                        session_state_label(&child.state),
                        child.unread_messages
                    ));
                }
            }

            if let Some(worktree) = session.worktree.as_ref() {
                lines.push(format!(
                    "Branch {} | Base {}",
                    worktree.branch, worktree.base_branch
                ));
                lines.push(format!("Worktree {}", worktree.path.display()));
                if let Some(diff_summary) = self.selected_diff_summary.as_ref() {
                    lines.push(format!("Diff {diff_summary}"));
                }
            }

            lines.push(format!(
                "Tokens {} | Tools {} | Files {}",
                format_token_count(metrics.tokens_used),
                metrics.tool_calls,
                metrics.files_changed,
            ));
            lines.push(format!(
                "Cost ${:.4} | Duration {}s",
                metrics.cost_usd, metrics.duration_secs
            ));

            if let Some(last_output) = self.selected_output_lines().last() {
                lines.push(format!(
                    "Last output {}",
                    truncate_for_dashboard(&last_output.text, 96)
                ));
            }

            lines.push(String::new());
            if self.selected_messages.is_empty() {
                lines.push("Inbox clear".to_string());
            } else {
                lines.push("Recent messages:".to_string());
                let recent = self
                    .selected_messages
                    .iter()
                    .rev()
                    .take(3)
                    .collect::<Vec<_>>();
                for message in recent.into_iter().rev() {
                    lines.push(format!(
                        "- {} {} -> {} | {}",
                        self.short_timestamp(&message.timestamp.to_rfc3339()),
                        format_session_id(&message.from_session),
                        format_session_id(&message.to_session),
                        comms::preview(&message.msg_type, &message.content)
                    ));
                }
            }

            let attention_items = self.attention_queue_items(3);
            if attention_items.is_empty() {
                lines.push(String::new());
                lines.push("Attention queue clear".to_string());
            } else {
                lines.push(String::new());
                lines.push("Needs attention:".to_string());
                lines.extend(attention_items);
            }

            lines.join("\n")
        } else {
            "No metrics available".to_string()
        }
    }

    fn aggregate_cost_summary(&self) -> (String, Style) {
        let aggregate = self.aggregate_usage();
        let mut text = if self.cfg.cost_budget_usd > 0.0 {
            format!(
                "Aggregate cost {} / {}",
                format_currency(aggregate.total_cost_usd),
                format_currency(self.cfg.cost_budget_usd),
            )
        } else {
            format!(
                "Aggregate cost {} (no budget)",
                format_currency(aggregate.total_cost_usd)
            )
        };

        match aggregate.overall_state {
            BudgetState::Warning => text.push_str(" | Budget warning"),
            BudgetState::OverBudget => text.push_str(" | Budget exceeded"),
            _ => {}
        }

        (text, aggregate.overall_state.style())
    }

    fn attention_queue_items(&self, limit: usize) -> Vec<String> {
        let mut items = Vec::new();

        for session in &self.sessions {
            let unread = self
                .unread_message_counts
                .get(&session.id)
                .copied()
                .unwrap_or(0);
            if unread > 0 {
                items.push(format!(
                    "- Inbox {} | {} unread | {}",
                    format_session_id(&session.id),
                    unread,
                    truncate_for_dashboard(&session.task, 40)
                ));
            }

            if matches!(
                session.state,
                SessionState::Failed | SessionState::Stopped | SessionState::Pending
            ) {
                items.push(format!(
                    "- {} {} | {}",
                    session_state_label(&session.state),
                    format_session_id(&session.id),
                    truncate_for_dashboard(&session.task, 48)
                ));
            }

            if items.len() >= limit {
                break;
            }
        }

        items.truncate(limit);
        items
    }

    fn set_operator_note(&mut self, note: String) {
        self.operator_note = Some(note);
    }

    fn active_session_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| {
                matches!(
                    session.state,
                    SessionState::Pending | SessionState::Running | SessionState::Idle
                )
            })
            .count()
    }

    fn new_session_task(&self) -> String {
        self.sessions
            .get(self.selected_session)
            .map(|session| {
                format!(
                    "Follow up on {}: {}",
                    format_session_id(&session.id),
                    truncate_for_dashboard(&session.task, 96)
                )
            })
            .unwrap_or_else(|| "New ECC 2.0 session".to_string())
    }

    fn pane_areas(&self, area: Rect) -> PaneAreas {
        match self.cfg.pane_layout {
            PaneLayout::Horizontal => {
                let columns = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(self.primary_constraints())
                    .split(area);
                let right_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Percentage(OUTPUT_PANE_PERCENT),
                        Constraint::Percentage(100 - OUTPUT_PANE_PERCENT),
                    ])
                    .split(columns[1]);

                PaneAreas {
                    sessions: columns[0],
                    output: right_rows[0],
                    metrics: right_rows[1],
                    log: None,
                }
            }
            PaneLayout::Vertical => {
                let rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(self.primary_constraints())
                    .split(area);
                let bottom_columns = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(OUTPUT_PANE_PERCENT),
                        Constraint::Percentage(100 - OUTPUT_PANE_PERCENT),
                    ])
                    .split(rows[1]);

                PaneAreas {
                    sessions: rows[0],
                    output: bottom_columns[0],
                    metrics: bottom_columns[1],
                    log: None,
                }
            }
            PaneLayout::Grid => {
                let rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(self.primary_constraints())
                    .split(area);
                let top_columns = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(self.primary_constraints())
                    .split(rows[0]);
                let bottom_columns = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(self.primary_constraints())
                    .split(rows[1]);

                PaneAreas {
                    sessions: top_columns[0],
                    output: top_columns[1],
                    metrics: bottom_columns[0],
                    log: Some(bottom_columns[1]),
                }
            }
        }
    }

    fn primary_constraints(&self) -> [Constraint; 2] {
        [
            Constraint::Percentage(self.pane_size_percent),
            Constraint::Percentage(100 - self.pane_size_percent),
        ]
    }

    fn visible_panes(&self) -> &'static [Pane] {
        match self.cfg.pane_layout {
            PaneLayout::Grid => &[Pane::Sessions, Pane::Output, Pane::Metrics, Pane::Log],
            PaneLayout::Horizontal | PaneLayout::Vertical => {
                &[Pane::Sessions, Pane::Output, Pane::Metrics]
            }
        }
    }

    fn selected_pane_index(&self) -> usize {
        self.visible_panes()
            .iter()
            .position(|pane| *pane == self.selected_pane)
            .unwrap_or(0)
    }

    fn pane_border_style(&self, pane: Pane) -> Style {
        if self.selected_pane == pane {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        }
    }

    fn layout_label(&self) -> &'static str {
        match self.cfg.pane_layout {
            PaneLayout::Horizontal => "horizontal",
            PaneLayout::Vertical => "vertical",
            PaneLayout::Grid => "grid",
        }
    }

    fn log_field<'a>(&self, value: &'a str) -> &'a str {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            "n/a"
        } else {
            trimmed
        }
    }

    fn short_timestamp(&self, timestamp: &str) -> String {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .map(|value| value.format("%H:%M:%S").to_string())
            .unwrap_or_else(|_| timestamp.to_string())
    }

    #[cfg(test)]
    fn aggregate_cost_summary_text(&self) -> String {
        self.aggregate_cost_summary().0
    }

    #[cfg(test)]
    fn selected_output_text(&self) -> String {
        self.selected_output_lines()
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Pane {
    fn title(self) -> &'static str {
        match self {
            Pane::Sessions => "Sessions",
            Pane::Output => "Output",
            Pane::Metrics => "Metrics",
            Pane::Log => "Log",
        }
    }
}

impl SessionSummary {
    fn from_sessions(sessions: &[Session], unread_message_counts: &HashMap<String, usize>) -> Self {
        sessions.iter().fold(
            Self {
                total: sessions.len(),
                unread_messages: unread_message_counts.values().sum(),
                inbox_sessions: unread_message_counts.values().filter(|count| **count > 0).count(),
                ..Self::default()
            },
            |mut summary, session| {
                match session.state {
                    SessionState::Pending => summary.pending += 1,
                    SessionState::Running => summary.running += 1,
                    SessionState::Idle => summary.idle += 1,
                    SessionState::Completed => summary.completed += 1,
                    SessionState::Failed => summary.failed += 1,
                    SessionState::Stopped => summary.stopped += 1,
                }
                summary
            },
        )
    }
}

fn session_row(session: &Session, unread_messages: usize) -> Row<'static> {
    Row::new(vec![
        Cell::from(format_session_id(&session.id)),
        Cell::from(session.agent_type.clone()),
        Cell::from(session_state_label(&session.state)).style(
            Style::default()
                .fg(session_state_color(&session.state))
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from(session_branch(session)),
        Cell::from(if unread_messages == 0 {
            "-".to_string()
        } else {
            unread_messages.to_string()
        })
        .style(if unread_messages == 0 {
            Style::default()
        } else {
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        }),
        Cell::from(session.metrics.tokens_used.to_string()),
        Cell::from(format_duration(session.metrics.duration_secs)),
    ])
}

fn summary_line(summary: &SessionSummary) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("Total {}  ", summary.total),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        summary_span("Running", summary.running, Color::Green),
        summary_span("Idle", summary.idle, Color::Yellow),
        summary_span("Completed", summary.completed, Color::Blue),
        summary_span("Failed", summary.failed, Color::Red),
        summary_span("Stopped", summary.stopped, Color::DarkGray),
        summary_span("Pending", summary.pending, Color::Reset),
    ])
}

fn summary_span(label: &str, value: usize, color: Color) -> Span<'static> {
    Span::styled(
        format!("{label} {value}  "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn attention_queue_line(summary: &SessionSummary) -> Line<'static> {
    if summary.failed == 0
        && summary.stopped == 0
        && summary.pending == 0
        && summary.unread_messages == 0
    {
        return Line::from(vec![
            Span::styled(
                "Attention queue clear",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  no failed, stopped, or pending sessions"),
        ]);
    }

    Line::from(vec![
        Span::styled(
            "Attention queue  ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        summary_span("Inbox", summary.unread_messages, Color::Magenta),
        summary_span("Failed", summary.failed, Color::Red),
        summary_span("Stopped", summary.stopped, Color::DarkGray),
        summary_span("Pending", summary.pending, Color::Yellow),
    ])
}

fn truncate_for_dashboard(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let truncated: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

fn session_state_label(state: &SessionState) -> &'static str {
    match state {
        SessionState::Pending => "Pending",
        SessionState::Running => "Running",
        SessionState::Idle => "Idle",
        SessionState::Completed => "Completed",
        SessionState::Failed => "Failed",
        SessionState::Stopped => "Stopped",
    }
}

fn session_state_color(state: &SessionState) -> Color {
    match state {
        SessionState::Running => Color::Green,
        SessionState::Idle => Color::Yellow,
        SessionState::Failed => Color::Red,
        SessionState::Stopped => Color::DarkGray,
        SessionState::Completed => Color::Blue,
        SessionState::Pending => Color::Reset,
    }
}

fn format_session_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn assignment_action_label(action: manager::AssignmentAction) -> &'static str {
    match action {
        manager::AssignmentAction::Spawned => "spawned",
        manager::AssignmentAction::ReusedIdle => "reused idle",
        manager::AssignmentAction::ReusedActive => "reused active",
    }
}

fn session_branch(session: &Session) -> String {
    session
        .worktree
        .as_ref()
        .map(|worktree| worktree.branch.clone())
        .unwrap_or_else(|| "-".to_string())
}

fn format_duration(duration_secs: u64) -> String {
    let hours = duration_secs / 3600;
    let minutes = (duration_secs % 3600) / 60;
    let seconds = duration_secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use chrono::Utc;
    use ratatui::{backend::TestBackend, Terminal};
    use std::path::PathBuf;
    use uuid::Uuid;

    use super::*;
    use crate::config::PaneLayout;

    #[test]
    fn render_sessions_shows_summary_headers_and_selected_row() {
        let dashboard = test_dashboard(
            vec![
                sample_session(
                    "run-12345678",
                    "planner",
                    SessionState::Running,
                    Some("feat/run"),
                    128,
                    15,
                ),
                sample_session(
                    "done-87654321",
                    "reviewer",
                    SessionState::Completed,
                    Some("release/v1"),
                    2048,
                    125,
                ),
            ],
            1,
        );

        let rendered = render_dashboard_text(dashboard, 180, 24);
        assert!(rendered.contains("ID"));
        assert!(rendered.contains("Branch"));
        assert!(rendered.contains("Total 2"));
        assert!(rendered.contains("Running 1"));
        assert!(rendered.contains("Completed 1"));
        assert!(rendered.contains("Attention queue clear"));
        assert!(rendered.contains("done-876"));
    }

    #[test]
    fn selected_session_metrics_text_includes_worktree_output_and_attention_queue() {
        let mut dashboard = test_dashboard(
            vec![
                sample_session(
                    "focus-12345678",
                    "planner",
                    SessionState::Running,
                    Some("ecc/focus"),
                    512,
                    42,
                ),
                sample_session(
                    "failed-87654321",
                    "reviewer",
                    SessionState::Failed,
                    Some("ecc/failed"),
                    64,
                    5,
                ),
            ],
            0,
        );
        dashboard
            .session_output_cache
            .insert(
                "focus-12345678".to_string(),
                vec![OutputLine {
                    stream: OutputStream::Stdout,
                    text: "last useful output".to_string(),
                }],
            );
        dashboard.selected_diff_summary = Some("1 file changed, 2 insertions(+)".to_string());

        let text = dashboard.selected_session_metrics_text();
        assert!(text.contains("Branch ecc/focus | Base main"));
        assert!(text.contains("Worktree /tmp/ecc/focus"));
        assert!(text.contains("Diff 1 file changed, 2 insertions(+)"));
        assert!(text.contains("Last output last useful output"));
        assert!(text.contains("Needs attention:"));
        assert!(text.contains("Failed failed-8 | Render dashboard rows"));
    }

    #[test]
    fn selected_session_metrics_text_includes_team_capacity_summary() {
        let mut dashboard = test_dashboard(
            vec![sample_session(
                "focus-12345678",
                "planner",
                SessionState::Running,
                Some("ecc/focus"),
                512,
                42,
            )],
            0,
        );
        dashboard.selected_team_summary = Some(TeamSummary {
            total: 3,
            idle: 1,
            running: 1,
            pending: 1,
            failed: 0,
            stopped: 0,
        });

        let text = dashboard.selected_session_metrics_text();
        assert!(text.contains("Team 3/8 | idle 1 | running 1 | pending 1 | failed 0 | stopped 0"));
    }

    #[test]
    fn aggregate_cost_summary_mentions_total_cost() {
        let db = StateStore::open(Path::new(":memory:")).unwrap();
        let mut cfg = Config::default();
        cfg.cost_budget_usd = 10.0;

        let mut dashboard = Dashboard::new(db, cfg);
        dashboard.sessions = vec![budget_session("sess-1", 3_500, 8.25)];

        assert_eq!(
            dashboard.aggregate_cost_summary_text(),
            "Aggregate cost $8.25 / $10.00 | Budget warning"
        );
    }

    #[test]
    fn new_session_task_uses_selected_session_context() {
        let dashboard = test_dashboard(
            vec![sample_session(
                "focus-12345678",
                "planner",
                SessionState::Running,
                Some("ecc/focus"),
                512,
                42,
            )],
            0,
        );

        assert_eq!(
            dashboard.new_session_task(),
            "Follow up on focus-12: Render dashboard rows"
        );
    }

    #[test]
    fn active_session_count_only_counts_live_queue_states() {
        let dashboard = test_dashboard(
            vec![
                sample_session("pending-1", "planner", SessionState::Pending, None, 1, 1),
                sample_session("running-1", "planner", SessionState::Running, None, 1, 1),
                sample_session("idle-1", "planner", SessionState::Idle, None, 1, 1),
                sample_session("failed-1", "planner", SessionState::Failed, None, 1, 1),
                sample_session("stopped-1", "planner", SessionState::Stopped, None, 1, 1),
                sample_session("done-1", "planner", SessionState::Completed, None, 1, 1),
            ],
            0,
        );

        assert_eq!(dashboard.active_session_count(), 3);
    }

    #[test]
    fn refresh_preserves_selected_session_by_id() -> Result<()> {
        let db_path = std::env::temp_dir().join(format!("ecc2-dashboard-{}.db", Uuid::new_v4()));
        let db = StateStore::open(&db_path)?;
        let now = Utc::now();

        db.insert_session(&Session {
            id: "older".to_string(),
            task: "older".to_string(),
            agent_type: "claude".to_string(),
            working_dir: PathBuf::from("/tmp"),
            state: SessionState::Idle,
            pid: None,
            worktree: None,
            created_at: now,
            updated_at: now,
            metrics: SessionMetrics::default(),
        })?;

        db.insert_session(&Session {
            id: "newer".to_string(),
            task: "newer".to_string(),
            agent_type: "claude".to_string(),
            working_dir: PathBuf::from("/tmp"),
            state: SessionState::Running,
            pid: None,
            worktree: None,
            created_at: now,
            updated_at: now + chrono::Duration::seconds(1),
            metrics: SessionMetrics::default(),
        })?;

        let mut dashboard = Dashboard::new(db, Config::default());
        dashboard.selected_session = 1;
        dashboard.sync_selection();
        dashboard.refresh();

        assert_eq!(dashboard.selected_session_id(), Some("older"));
        let _ = std::fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn metrics_scroll_does_not_mutate_output_scroll() -> Result<()> {
        let db_path = std::env::temp_dir().join(format!("ecc2-dashboard-{}.db", Uuid::new_v4()));
        let db = StateStore::open(&db_path)?;
        let now = Utc::now();

        db.insert_session(&Session {
            id: "session-1".to_string(),
            task: "inspect output".to_string(),
            agent_type: "claude".to_string(),
            working_dir: PathBuf::from("/tmp"),
            state: SessionState::Running,
            pid: None,
            worktree: None,
            created_at: now,
            updated_at: now,
            metrics: SessionMetrics::default(),
        })?;

        for index in 0..6 {
            db.append_output_line("session-1", OutputStream::Stdout, &format!("line {index}"))?;
        }

        let mut dashboard = Dashboard::new(db, Config::default());
        dashboard.selected_pane = Pane::Output;
        dashboard.refresh();
        dashboard.sync_output_scroll(3);
        dashboard.scroll_up();
        let previous_scroll = dashboard.output_scroll_offset;

        dashboard.selected_pane = Pane::Metrics;
        dashboard.scroll_up();
        dashboard.scroll_down();

        assert_eq!(dashboard.output_scroll_offset, previous_scroll);
        let _ = std::fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn refresh_loads_selected_session_output_and_follows_tail() -> Result<()> {
        let db_path = std::env::temp_dir().join(format!("ecc2-dashboard-{}.db", Uuid::new_v4()));
        let db = StateStore::open(&db_path)?;
        let now = Utc::now();

        db.insert_session(&Session {
            id: "session-1".to_string(),
            task: "tail output".to_string(),
            agent_type: "claude".to_string(),
            working_dir: PathBuf::from("/tmp"),
            state: SessionState::Running,
            pid: None,
            worktree: None,
            created_at: now,
            updated_at: now,
            metrics: SessionMetrics::default(),
        })?;

        for index in 0..12 {
            db.append_output_line("session-1", OutputStream::Stdout, &format!("line {index}"))?;
        }

        let mut dashboard = Dashboard::new(db, Config::default());
        dashboard.selected_pane = Pane::Output;
        dashboard.refresh();
        dashboard.sync_output_scroll(4);

        assert_eq!(dashboard.output_scroll_offset, 8);
        assert!(dashboard.selected_output_text().contains("line 11"));

        let _ = std::fs::remove_file(db_path);
        Ok(())
    }

    #[tokio::test]
    async fn stop_selected_uses_session_manager_transition() -> Result<()> {
        let db_path = std::env::temp_dir().join(format!("ecc2-dashboard-{}.db", Uuid::new_v4()));
        let db = StateStore::open(&db_path)?;
        let now = Utc::now();

        db.insert_session(&Session {
            id: "running-1".to_string(),
            task: "stop me".to_string(),
            agent_type: "claude".to_string(),
            state: SessionState::Running,
            working_dir: PathBuf::from("/tmp"),
            pid: Some(999_999),
            worktree: None,
            created_at: now,
            updated_at: now,
            metrics: SessionMetrics::default(),
        })?;

        let dashboard_store = StateStore::open(&db_path)?;
        let mut dashboard = Dashboard::new(dashboard_store, Config::default());
        dashboard.stop_selected().await;

        let session = db
            .get_session("running-1")?
            .expect("session should exist after stop");
        assert_eq!(session.state, SessionState::Stopped);
        assert_eq!(session.pid, None);

        let _ = std::fs::remove_file(db_path);
        Ok(())
    }

    #[tokio::test]
    async fn resume_selected_requeues_failed_session() -> Result<()> {
        let db_path = std::env::temp_dir().join(format!("ecc2-dashboard-{}.db", Uuid::new_v4()));
        let db = StateStore::open(&db_path)?;
        let now = Utc::now();

        db.insert_session(&Session {
            id: "failed-1".to_string(),
            task: "resume me".to_string(),
            agent_type: "claude".to_string(),
            state: SessionState::Failed,
            working_dir: PathBuf::from("/tmp/ecc2-resume"),
            pid: None,
            worktree: Some(WorktreeInfo {
                path: PathBuf::from("/tmp/ecc2-resume"),
                branch: "ecc/failed-1".to_string(),
                base_branch: "main".to_string(),
            }),
            created_at: now,
            updated_at: now,
            metrics: SessionMetrics::default(),
        })?;

        let dashboard_store = StateStore::open(&db_path)?;
        let mut dashboard = Dashboard::new(dashboard_store, Config::default());
        dashboard.resume_selected().await;

        let session = db
            .get_session("failed-1")?
            .expect("session should exist after resume");
        assert_eq!(session.state, SessionState::Pending);
        assert_eq!(session.pid, None);

        let _ = std::fs::remove_file(db_path);
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_selected_worktree_clears_session_metadata() -> Result<()> {
        let db_path = std::env::temp_dir().join(format!("ecc2-dashboard-{}.db", Uuid::new_v4()));
        let db = StateStore::open(&db_path)?;
        let now = Utc::now();
        let worktree_path = std::env::temp_dir().join(format!("ecc2-cleanup-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&worktree_path)?;

        db.insert_session(&Session {
            id: "stopped-1".to_string(),
            task: "cleanup me".to_string(),
            agent_type: "claude".to_string(),
            state: SessionState::Stopped,
            working_dir: worktree_path.clone(),
            pid: None,
            worktree: Some(WorktreeInfo {
                path: worktree_path.clone(),
                branch: "ecc/stopped-1".to_string(),
                base_branch: "main".to_string(),
            }),
            created_at: now,
            updated_at: now,
            metrics: SessionMetrics::default(),
        })?;

        let dashboard_store = StateStore::open(&db_path)?;
        let mut dashboard = Dashboard::new(dashboard_store, Config::default());
        dashboard.cleanup_selected_worktree().await;

        let session = db
            .get_session("stopped-1")?
            .expect("session should exist after cleanup");
        assert!(session.worktree.is_none(), "worktree metadata should be cleared");

        let _ = std::fs::remove_dir_all(worktree_path);
        let _ = std::fs::remove_file(db_path);
        Ok(())
    }

    #[tokio::test]
    async fn delete_selected_session_removes_inactive_session() -> Result<()> {
        let db_path = std::env::temp_dir().join(format!("ecc2-dashboard-{}.db", Uuid::new_v4()));
        let db = StateStore::open(&db_path)?;
        let now = Utc::now();

        db.insert_session(&Session {
            id: "done-1".to_string(),
            task: "delete me".to_string(),
            agent_type: "claude".to_string(),
            working_dir: PathBuf::from("/tmp"),
            state: SessionState::Completed,
            pid: None,
            worktree: None,
            created_at: now,
            updated_at: now,
            metrics: SessionMetrics::default(),
        })?;

        let dashboard_store = StateStore::open(&db_path)?;
        let mut dashboard = Dashboard::new(dashboard_store, Config::default());
        dashboard.delete_selected_session().await;

        assert!(db.get_session("done-1")?.is_none(), "session should be deleted");

        let _ = std::fs::remove_file(db_path);
        Ok(())
    }

    #[tokio::test]
    async fn auto_dispatch_backlog_sets_operator_note_when_clear() -> Result<()> {
        let db_path = std::env::temp_dir().join(format!("ecc2-dashboard-{}.db", Uuid::new_v4()));
        let db = StateStore::open(&db_path)?;
        let now = Utc::now();

        db.insert_session(&Session {
            id: "lead-1".to_string(),
            task: "coordinate".to_string(),
            agent_type: "claude".to_string(),
            working_dir: PathBuf::from("/tmp"),
            state: SessionState::Running,
            pid: None,
            worktree: None,
            created_at: now,
            updated_at: now,
            metrics: SessionMetrics::default(),
        })?;

        let dashboard_store = StateStore::open(&db_path)?;
        let mut dashboard = Dashboard::new(dashboard_store, Config::default());
        dashboard.auto_dispatch_backlog().await;

        assert_eq!(
            dashboard.operator_note.as_deref(),
            Some("no unread handoff backlog found")
        );

        let _ = std::fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn grid_layout_renders_four_panes() {
        let mut dashboard = test_dashboard(vec![sample_session("grid-1", "claude", SessionState::Running, None, 1, 1)], 0);
        dashboard.cfg.pane_layout = PaneLayout::Grid;
        dashboard.pane_size_percent = DEFAULT_GRID_SIZE_PERCENT;

        let areas = dashboard.pane_areas(Rect::new(0, 0, 100, 40));
        let log_area = areas.log.expect("grid layout should include a log pane");

        assert!(areas.output.x > areas.sessions.x);
        assert!(areas.metrics.y > areas.sessions.y);
        assert!(log_area.x > areas.metrics.x);
    }

    #[test]
    fn pane_resize_clamps_to_bounds() {
        let mut dashboard = test_dashboard(Vec::new(), 0);
        dashboard.cfg.pane_layout = PaneLayout::Grid;
        dashboard.pane_size_percent = DEFAULT_GRID_SIZE_PERCENT;

        for _ in 0..20 {
            dashboard.increase_pane_size();
        }
        assert_eq!(dashboard.pane_size_percent, MAX_PANE_SIZE_PERCENT);

        for _ in 0..40 {
            dashboard.decrease_pane_size();
        }
        assert_eq!(dashboard.pane_size_percent, MIN_PANE_SIZE_PERCENT);
    }

    #[test]
    fn pane_navigation_skips_log_outside_grid_layouts() {
        let mut dashboard = test_dashboard(Vec::new(), 0);
        dashboard.next_pane();
        dashboard.next_pane();
        dashboard.next_pane();
        assert_eq!(dashboard.selected_pane, Pane::Sessions);

        dashboard.cfg.pane_layout = PaneLayout::Grid;
        dashboard.pane_size_percent = DEFAULT_GRID_SIZE_PERCENT;
        dashboard.next_pane();
        dashboard.next_pane();
        dashboard.next_pane();
        assert_eq!(dashboard.selected_pane, Pane::Log);
    }

    fn test_dashboard(sessions: Vec<Session>, selected_session: usize) -> Dashboard {
        let selected_session = selected_session.min(sessions.len().saturating_sub(1));
        let cfg = Config::default();
        let output_store = SessionOutputStore::default();
        let output_rx = output_store.subscribe();
        let mut session_table_state = TableState::default();
        if !sessions.is_empty() {
            session_table_state.select(Some(selected_session));
        }

        Dashboard {
            db: StateStore::open(Path::new(":memory:")).expect("open test db"),
            pane_size_percent: match cfg.pane_layout {
                PaneLayout::Grid => DEFAULT_GRID_SIZE_PERCENT,
                PaneLayout::Horizontal | PaneLayout::Vertical => DEFAULT_PANE_SIZE_PERCENT,
            },
            cfg,
            output_store,
            output_rx,
            sessions,
            session_output_cache: HashMap::new(),
            unread_message_counts: HashMap::new(),
            selected_messages: Vec::new(),
            selected_parent_session: None,
            selected_child_sessions: Vec::new(),
            selected_team_summary: None,
            logs: Vec::new(),
            selected_diff_summary: None,
            selected_pane: Pane::Sessions,
            selected_session,
            show_help: false,
            operator_note: None,
            output_follow: true,
            output_scroll_offset: 0,
            last_output_height: 0,
            session_table_state,
        }
    }

    fn sample_session(
        id: &str,
        agent_type: &str,
        state: SessionState,
        branch: Option<&str>,
        tokens_used: u64,
        duration_secs: u64,
    ) -> Session {
        Session {
            id: id.to_string(),
            task: "Render dashboard rows".to_string(),
            agent_type: agent_type.to_string(),
            state,
            working_dir: branch
                .map(|branch| PathBuf::from(format!("/tmp/{branch}")))
                .unwrap_or_else(|| PathBuf::from("/tmp")),
            pid: None,
            worktree: branch.map(|branch| WorktreeInfo {
                path: PathBuf::from(format!("/tmp/{branch}")),
                branch: branch.to_string(),
                base_branch: "main".to_string(),
            }),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metrics: SessionMetrics {
                tokens_used,
                tool_calls: 4,
                files_changed: 2,
                duration_secs,
                cost_usd: 0.42,
            },
        }
    }

    fn budget_session(id: &str, tokens_used: u64, cost_usd: f64) -> Session {
        let now = Utc::now();
        Session {
            id: id.to_string(),
            task: "Budget tracking".to_string(),
            agent_type: "claude".to_string(),
            state: SessionState::Running,
            working_dir: PathBuf::from("/tmp"),
            pid: None,
            worktree: None,
            created_at: now,
            updated_at: now,
            metrics: SessionMetrics {
                tokens_used,
                tool_calls: 0,
                files_changed: 0,
                duration_secs: 0,
                cost_usd,
            },
        }
    }

    fn render_dashboard_text(mut dashboard: Dashboard, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create terminal");

        terminal
            .draw(|frame| dashboard.render(frame))
            .expect("render dashboard");

        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
