//! Interactive review TUI (VR-2/VR-3).
//!
//! This module renders signed ledger proofs (session list, verdict bundle,
//! ledger chain, transcript) so a human can review a Truss task without reading
//! the full transcript. VR-3 adds action keybindings, but every provenance write
//! routes through core [`Store`] primitives (`human_review` and durable
//! `signals`). UI state itself (selection / scroll / pending note input) is
//! presentation only and is never persisted to the ledger.
//!
//! The whole module is gated behind the `tui` cargo feature so headless/CI and
//! `--no-default-features` builds never pull in ratatui/crossterm.
//!
//! Layout is split so the rendering is hermetically testable:
//!   * [`ReviewApp`] owns the loaded snapshots + transient UI state.
//!   * `render_*` functions draw a [`Frame`] from that state — no terminal,
//!     network, or credentials required, so they run against a `TestBackend`.
//!   * [`run`] is the only part that touches a real terminal (event loop +
//!     crossterm raw mode); it is excluded from the hermetic tests.

use std::io;

use anyhow::{anyhow, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};

use truss_core::approval::APPROVAL_GATE_SIGNAL;
use truss_core::ledger::{
    LedgerReader, LedgerSessionSummary, LedgerVerdictBundle, LedgerVerifyReport, VerificationStatus,
};
use truss_core::store::{HumanReviewDecision, Store};

/// Which screen is in front. Transcript is reachable but never the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Recent/active sessions with machine verdict + human-review status.
    List,
    /// Verdict bundle detail (DoD, evidence, panel, sign-off, budget).
    Verdict,
    /// Ledger chain view: head hash + verify status + per-entry hashes.
    Chain,
    /// Transcript drill-down (secondary view).
    Transcript,
}

/// A snapshot of one session's reviewable proofs. Everything here is produced
/// by [`LedgerReader`]; actions refresh the snapshot after core writes.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub summary: LedgerSessionSummary,
    pub verdict: Option<LedgerVerdictBundle>,
    pub verify: Option<LedgerVerifyReport>,
    /// Human-readable transcript lines (run prompt, model results, tool calls).
    pub transcript: Vec<String>,
    pub pending_signal: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    Review(HumanReviewDecision),
    Signal,
}

/// In-memory, non-durable application state. None of this is ever written back
/// to the ledger.
#[derive(Debug, Clone)]
pub struct ReviewApp {
    sessions: Vec<SessionView>,
    selected: usize,
    screen: Screen,
    detail_scroll: u16,
    should_quit: bool,
    pending_action: Option<PendingAction>,
    input: String,
    status: Option<String>,
    db_hint: String,
}

impl ReviewApp {
    /// Build the app state by reading every listed session through the reader.
    /// This calls `list`, `verdict`, and `verify` and snapshots the results; it
    /// performs no writes itself.
    pub fn load(reader: &LedgerReader, db_hint: impl Into<String>) -> Result<Self> {
        let summaries = reader.list_sessions().map_err(|e| anyhow!(e.to_string()))?;
        let mut sessions = Vec::with_capacity(summaries.len());
        for summary in summaries {
            let id = summary.id.clone();
            // A session may not have a verdict bundle yet; treat a read error as
            // "no verdict to show" rather than failing the whole TUI.
            let verdict = reader.verdict(&id).ok();
            let verify = reader.verify(&id, None).ok();
            let entries = reader.entries(&id).unwrap_or_default();
            let transcript = transcript_lines(&entries);
            let pending_signal = pending_signal_name(&entries);
            sessions.push(SessionView {
                summary,
                verdict,
                verify,
                transcript,
                pending_signal,
            });
        }
        Ok(Self {
            sessions,
            selected: 0,
            screen: Screen::List,
            detail_scroll: 0,
            should_quit: false,
            pending_action: None,
            input: String::new(),
            status: None,
            db_hint: db_hint.into(),
        })
    }

    /// Active screen. Test-only accessor (the event loop owns transitions).
    #[cfg(test)]
    pub fn screen(&self) -> Screen {
        self.screen
    }

    /// Whether any sessions are loaded. Test-only accessor.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    #[cfg(test)]
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    fn selected_session(&self) -> Option<&SessionView> {
        self.sessions.get(self.selected)
    }

    fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
        }
    }

    fn select_prev(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + self.sessions.len() - 1) % self.sessions.len();
        }
    }

    /// Apply one keypress. The event loop checks `should_quit`.
    /// Keybindings are documented in the footer rendered by [`render_footer`].
    pub fn on_key(&mut self, code: KeyCode, store: &Store) -> Result<()> {
        if self.pending_action.is_some() {
            return self.on_input_key(code, store);
        }
        match self.screen {
            Screen::List => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Down | KeyCode::Char('j') => self.select_next(),
                KeyCode::Up | KeyCode::Char('k') => self.select_prev(),
                KeyCode::Enter | KeyCode::Char('v') => self.open(Screen::Verdict),
                KeyCode::Char('c') => self.open(Screen::Chain),
                KeyCode::Char('t') => self.open(Screen::Transcript),
                KeyCode::Char('a') => self.start_review(HumanReviewDecision::Approved),
                KeyCode::Char('r') => self.start_review(HumanReviewDecision::Rejected),
                KeyCode::Char('n') => self.start_review(HumanReviewDecision::NeedsChanges),
                KeyCode::Char('s') => self.start_signal(),
                KeyCode::Char('x') => self.show_resume_command(),
                _ => {}
            },
            Screen::Verdict | Screen::Chain | Screen::Transcript => match code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Backspace => {
                    self.screen = Screen::List;
                    self.detail_scroll = 0;
                }
                KeyCode::Char('v') => self.open(Screen::Verdict),
                KeyCode::Char('c') => self.open(Screen::Chain),
                KeyCode::Char('t') => self.open(Screen::Transcript),
                KeyCode::Char('a') => self.start_review(HumanReviewDecision::Approved),
                KeyCode::Char('r') => self.start_review(HumanReviewDecision::Rejected),
                KeyCode::Char('n') => self.start_review(HumanReviewDecision::NeedsChanges),
                KeyCode::Char('s') => self.start_signal(),
                KeyCode::Char('x') => self.show_resume_command(),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1)
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn open(&mut self, screen: Screen) {
        if self.selected_session().is_some() {
            self.screen = screen;
            self.detail_scroll = 0;
        }
    }

    fn start_review(&mut self, decision: HumanReviewDecision) {
        if self.selected_session().is_none() {
            return;
        }
        self.pending_action = Some(PendingAction::Review(decision));
        self.input.clear();
        self.status = Some(format!(
            "{} note (optional): type note, enter to sign, esc to cancel",
            decision_label(decision)
        ));
    }

    fn start_signal(&mut self) {
        let Some(view) = self.selected_session() else {
            return;
        };
        if view.summary.run_status != "suspended" {
            self.status = Some("selected run is not suspended".into());
            return;
        }
        if view.pending_signal.is_none() {
            self.status =
                Some("selected run is suspended, but no pending signal name was found".into());
            return;
        }
        if view.pending_signal.as_deref() == Some(APPROVAL_GATE_SIGNAL) {
            self.status = Some("approval gate requires a signed a/r/n review decision".into());
            return;
        }
        self.pending_action = Some(PendingAction::Signal);
        self.input.clear();
        self.status = Some("signal payload (optional): enter to deliver, esc to cancel".into());
    }

    fn show_resume_command(&mut self) {
        let Some(view) = self.selected_session() else {
            return;
        };
        self.status = Some(format!(
            "resume control: truss --resume {} --db {}",
            view.summary.id, self.db_hint
        ));
    }

    fn on_input_key(&mut self, code: KeyCode, store: &Store) -> Result<()> {
        match code {
            KeyCode::Esc => {
                self.pending_action = None;
                self.input.clear();
                self.status = Some("action cancelled".into());
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.commit_pending_action(store)?,
            KeyCode::Char(ch) => self.input.push(ch),
            _ => {}
        }
        Ok(())
    }

    fn commit_pending_action(&mut self, store: &Store) -> Result<()> {
        let Some(action) = self.pending_action.take() else {
            return Ok(());
        };
        let Some(view) = self.selected_session() else {
            return Ok(());
        };
        let session_id = view.summary.id.clone();
        let note_or_payload = if self.input.trim().is_empty() {
            None
        } else {
            Some(self.input.trim().to_string())
        };
        match action {
            PendingAction::Review(decision) => {
                store
                    .record_human_review(&session_id, decision, note_or_payload)
                    .map_err(|e| anyhow!(e.to_string()))?;
                self.status = Some(format!(
                    "signed {} review for {session_id}",
                    decision_label(decision)
                ));
            }
            PendingAction::Signal => {
                let signal = view
                    .pending_signal
                    .clone()
                    .ok_or_else(|| anyhow!("selected run has no pending signal"))?;
                store
                    .add_signal(&session_id, &signal, note_or_payload.as_deref())
                    .map_err(|e| anyhow!(e.to_string()))?;
                self.status = Some(format!(
                    "delivered signal '{signal}' to {session_id}; resume with: truss --resume {session_id} --db {}",
                    self.db_hint
                ));
            }
        }
        self.input.clear();
        self.refresh(store)
    }

    fn refresh(&mut self, store: &Store) -> Result<()> {
        let selected_id = self.selected_session().map(|view| view.summary.id.clone());
        let reader = LedgerReader::new(store.clone());
        let mut fresh = ReviewApp::load(&reader, self.db_hint.clone())?;
        if let Some(selected_id) = selected_id {
            if let Some(index) = fresh
                .sessions
                .iter()
                .position(|view| view.summary.id == selected_id)
            {
                fresh.selected = index;
            }
        }
        fresh.screen = self.screen;
        fresh.detail_scroll = self.detail_scroll;
        fresh.status = self.status.clone();
        *self = fresh;
        Ok(())
    }
}

/// Format a verification status as a glanceable trust label (principle #2:
/// trust tiers visible).
fn verify_label(status: VerificationStatus) -> &'static str {
    match status {
        VerificationStatus::Verified => "VERIFIED",
        VerificationStatus::Unverified => "UNVERIFIED",
    }
}

fn verify_style(status: VerificationStatus) -> Style {
    match status {
        VerificationStatus::Verified => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        VerificationStatus::Unverified => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
    }
}

fn decision_label(decision: HumanReviewDecision) -> &'static str {
    match decision {
        HumanReviewDecision::Approved => "approved",
        HumanReviewDecision::Rejected => "rejected",
        HumanReviewDecision::NeedsChanges => "needs_changes",
    }
}

fn human_status(view: &SessionView) -> String {
    match view.verdict.as_ref().and_then(|v| v.human_signoff.as_ref()) {
        Some(signoff) => {
            let mut label = decision_label(signoff.decision).to_string();
            if !signoff.attested_head_matches_current {
                label.push_str(" (stale)");
            }
            label
        }
        None => "—".to_string(),
    }
}

fn short_hash(hash: &str) -> &str {
    &hash[..12.min(hash.len())]
}

/// Pull a human-readable transcript out of the raw journal entries.
fn transcript_lines(entries: &[truss_core::store::JournalEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            let detail = match entry.kind.as_str() {
                "run_started" => entry
                    .payload
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                "model_result" => entry
                    .payload
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                "tool_call" => entry
                    .payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                "tool_result" => entry
                    .payload
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            };
            let detail = first_line(&detail);
            if detail.is_empty() {
                format!("#{:<3} {}", entry.seq, entry.kind)
            } else {
                format!("#{:<3} {}: {}", entry.seq, entry.kind, detail)
            }
        })
        .collect()
}

fn pending_signal_name(entries: &[truss_core::store::JournalEntry]) -> Option<String> {
    let mut pending = None;
    for entry in entries {
        match entry.kind.as_str() {
            "suspended"
                if entry.payload.get("await_kind").and_then(|v| v.as_str()) == Some("signal") =>
            {
                pending = entry
                    .payload
                    .get("signal_name")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
            }
            "signal_received" | "run_completed" | "run_failed" => pending = None,
            _ => {}
        }
    }
    pending
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

// ---------------------------------------------------------------------------
// Rendering (hermetic — no terminal required).
// ---------------------------------------------------------------------------

/// Draw the whole frame for the current screen. Pure function of `app` state.
pub fn render(frame: &mut Frame, app: &ReviewApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .split(frame.area());

    render_header(frame, chunks[0], app);
    match app.screen {
        Screen::List => render_list(frame, chunks[1], app),
        Screen::Verdict => render_verdict(frame, chunks[1], app),
        Screen::Chain => render_chain(frame, chunks[1], app),
        Screen::Transcript => render_transcript(frame, chunks[1], app),
    }
    render_footer(frame, chunks[2], app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &ReviewApp) {
    let title = match app.screen {
        Screen::List => "Truss review — sessions",
        Screen::Verdict => "Truss review — verdict bundle",
        Screen::Chain => "Truss review — ledger chain",
        Screen::Transcript => "Truss review — transcript",
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        area,
    );
}

fn render_list(frame: &mut Frame, area: Rect, app: &ReviewApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Recent sessions (machine verdict vs human sign-off)");

    if app.sessions.is_empty() {
        frame.render_widget(
            Paragraph::new("No durable sessions found in this ledger.").block(block),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|view| {
            let verify = view.summary.verification.status;
            let machine = view.summary.verdict_status.as_deref().unwrap_or("unknown");
            let human = human_status(view);
            let await_label = view
                .pending_signal
                .as_ref()
                .map(|signal| {
                    if signal == APPROVAL_GATE_SIGNAL {
                        " gated:approval".to_string()
                    } else {
                        format!(" await:{signal}")
                    }
                })
                .unwrap_or_default();
            let line = Line::from(vec![
                Span::raw(format!("{:<22} ", view.summary.id)),
                Span::styled(
                    format!("{:<11}", verify_label(verify)),
                    verify_style(verify),
                ),
                Span::raw(format!(
                    " status:{:<10} machine:{machine:<8} human:{human}{await_label}",
                    view.summary.run_status
                )),
            ]);
            ListItem::new(line)
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.selected));
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_verdict(frame: &mut Frame, area: Rect, app: &ReviewApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Verdict bundle");
    let Some(view) = app.selected_session() else {
        frame.render_widget(Paragraph::new("No session selected.").block(block), area);
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(format!("session: {}", view.summary.id)));

    let verify = view.summary.verification.status;
    lines.push(Line::from(vec![
        Span::raw("ledger: "),
        Span::styled(verify_label(verify), verify_style(verify)),
    ]));

    let Some(verdict) = &view.verdict else {
        lines.push(Line::from(""));
        lines.push(Line::from(
            "No verdict bundle recorded for this session yet.",
        ));
        frame.render_widget(paragraph(lines, app.detail_scroll).block(block), area);
        return;
    };

    // Side-by-side machine verdict vs human sign-off (design decision #4).
    let human = verdict
        .human_signoff
        .as_ref()
        .map(|s| {
            let stale = if s.attested_head_matches_current {
                ""
            } else {
                " (stale: head moved since sign-off)"
            };
            format!("{}{}", decision_label(s.decision), stale)
        })
        .unwrap_or_else(|| "— none —".to_string());
    lines.push(Line::from(vec![
        Span::styled("machine: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(verdict.overall.clone()),
        Span::raw("    "),
        Span::styled("human: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(human),
    ]));
    if let Some(signoff) = &verdict.human_signoff {
        if let Some(note) = &signoff.note {
            lines.push(Line::from(format!("  human note: {note}")));
        }
        lines.push(Line::from(format!(
            "  reviewer_key_id: {}",
            short_hash(&signoff.reviewer_key_id)
        )));
    }

    if let Some(contract) = &verdict.contract {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "definition of done",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if let Some(intent) = &contract.intent {
            lines.push(Line::from(format!("  intent: {intent}")));
        }
        for clause in &contract.definition_of_done {
            lines.push(Line::from(format!("  - [{}] {}", clause.kind, clause.desc)));
        }
    }

    if !verdict.evidence.checks.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "clause evidence",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for check in &verdict.evidence.checks {
            lines.push(Line::from(format!(
                "  {} [{}] {}",
                check.verdict, check.kind, check.desc
            )));
            if let Some(first) = check.evidence.lines().next() {
                lines.push(Line::from(format!("      {first}")));
            }
        }
    }

    if !verdict.evidence.self_validation.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "self-validation evidence",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for item in &verdict.evidence.self_validation {
            lines.push(Line::from(format!(
                "  exit={} cmd={}",
                item.exit
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "?".into()),
                item.cmd.as_deref().unwrap_or("unknown")
            )));
        }
    }

    if !verdict.evidence.panel.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "reviewer panel",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for panel in &verdict.evidence.panel {
            lines.push(Line::from(format!(
                "  {} {}",
                panel.clause_verdict, panel.desc
            )));
            if let Some(majority) = &panel.majority {
                lines.push(Line::from(format!(
                    "      majority: {} ({}/{})",
                    majority.decision, majority.met_count, majority.reviewer_count
                )));
            }
            for reviewer in &panel.reviewers {
                lines.push(Line::from(format!(
                    "      [{}] {} {}: {}",
                    reviewer.seat, reviewer.lens, reviewer.verdict, reviewer.reasoning
                )));
            }
        }
    }

    // Budget (principle #3: cost & budget always in view).
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "budget",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(format!(
        "  diff summary: {}",
        diff_summary(verdict)
    )));
    lines.push(Line::from(format!("  spend: {}", budget_summary(verdict))));

    frame.render_widget(paragraph(lines, app.detail_scroll).block(block), area);
}

/// Pull a diff summary out of the recorded verdict payload, if present.
fn diff_summary(verdict: &LedgerVerdictBundle) -> String {
    verdict
        .verdict_entry
        .as_ref()
        .and_then(|entry| entry.payload.get("reviewed_diff"))
        .and_then(|diff| diff.get("summary").or(Some(diff)))
        .and_then(|v| v.as_str())
        .map(first_line)
        .unwrap_or_else(|| "no diff recorded".to_string())
}

/// Pull a usage/budget summary out of the recorded verdict payload, if present.
fn budget_summary(verdict: &LedgerVerdictBundle) -> String {
    let Some(usage) = verdict
        .verdict_entry
        .as_ref()
        .and_then(|entry| entry.payload.get("usage"))
    else {
        return "no usage recorded".to_string();
    };
    let cost = usage
        .get("cost_usd")
        .and_then(|v| v.as_f64())
        .map(|c| format!("${c:.4}"))
        .unwrap_or_else(|| "$?".into());
    let tokens = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .map(|t| t.to_string())
        .unwrap_or_else(|| "?".into());
    format!("{cost} / {tokens} tokens")
}

fn render_chain(frame: &mut Frame, area: Rect, app: &ReviewApp) {
    let block = Block::default().borders(Borders::ALL).title("Ledger chain");
    let Some(view) = app.selected_session() else {
        frame.render_widget(Paragraph::new("No session selected.").block(block), area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(format!("session: {}", view.summary.id)));
    lines.push(Line::from(format!(
        "head: {}",
        view.summary.head_hash.as_deref().unwrap_or("-")
    )));

    let status = view.summary.verification.status;
    let detail = match (&view.verify, status) {
        (Some(report), VerificationStatus::Verified) => format!(
            "{} entries, hash chain + signatures intact",
            report.verification.verified_entries
        ),
        (Some(report), VerificationStatus::Unverified) => format!(
            "TAMPER at seq {}",
            report
                .verification
                .failed_at_seq
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".into())
        ),
        (None, _) => "verify report unavailable".to_string(),
    };
    lines.push(Line::from(vec![
        Span::raw("verify: "),
        Span::styled(verify_label(status), verify_style(status)),
        Span::raw(format!("  {detail}")),
    ]));

    if let Some(report) = &view.verify {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("{:<20} {:>5} {:<12} VERIFY", "KIND", "SEQ", "HASH"),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for entry in &report.entries {
            lines.push(Line::from(format!(
                "{:<20} {:>5} {:<12} {}",
                entry.kind,
                entry.seq,
                entry.short_hash,
                verify_label(entry.verification_status).to_ascii_lowercase()
            )));
        }
    }

    frame.render_widget(paragraph(lines, app.detail_scroll).block(block), area);
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &ReviewApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Transcript (secondary view)");
    let Some(view) = app.selected_session() else {
        frame.render_widget(Paragraph::new("No session selected.").block(block), area);
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(format!("session: {}", view.summary.id)));
    lines.push(Line::from(""));
    if view.transcript.is_empty() {
        lines.push(Line::from("No transcript entries."));
    }
    for entry in &view.transcript {
        lines.push(Line::from(entry.clone()));
    }
    frame.render_widget(paragraph(lines, app.detail_scroll).block(block), area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &ReviewApp) {
    let keys = if let Some(action) = app.pending_action {
        match action {
            PendingAction::Review(decision) => format!(
                "{} note: {}_  enter sign  esc cancel",
                decision_label(decision),
                app.input
            ),
            PendingAction::Signal => {
                format!("signal payload: {}_  enter deliver  esc cancel", app.input)
            }
        }
    } else {
        match app.screen {
            Screen::List => {
                "↑/↓ move  enter/v verdict  c chain  t transcript  a/r/n sign  s signal  x resume  q quit".to_string()
            }
            Screen::Verdict | Screen::Chain | Screen::Transcript => {
                "↑/↓ scroll  v verdict  c chain  t transcript  a/r/n sign  s signal  x resume  esc/q back".to_string()
            }
        }
    };
    let status = app.status.as_deref().unwrap_or("");
    frame.render_widget(
        Paragraph::new(vec![Line::from(keys), Line::from(status.to_string())])
            .block(Block::default().borders(Borders::ALL).title("keys"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn paragraph(lines: Vec<Line<'static>>, scroll: u16) -> Paragraph<'static> {
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
}

// ---------------------------------------------------------------------------
// Terminal driver (not exercised by the hermetic tests).
// ---------------------------------------------------------------------------

/// Open the review TUI over `store`, run the event loop until the user quits,
/// and restore the terminal. Provenance writes route through core store APIs.
pub fn run(store: &Store, db_hint: impl Into<String>) -> Result<()> {
    let reader = LedgerReader::new(store.clone());
    let app = ReviewApp::load(&reader, db_hint)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(&mut terminal, app, store);
    // Always restore the terminal, even if the loop errored.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    mut app: ReviewApp,
    store: &Store,
) -> Result<()> {
    loop {
        terminal.draw(|frame| render(frame, &app))?;
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                app.on_key(key.code, store)?;
            }
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use std::sync::Arc;
    use truss_core::identity::Signer;
    use truss_core::store::{HumanReviewDecision, Store};

    fn fixture_db(path: &std::path::Path) {
        let store = Store::open(path.to_str().unwrap(), Arc::new(Signer::generate())).unwrap();
        store
            .create_session("run-tui", Some("mock-model"), "local", "local")
            .unwrap();
        store
            .append(
                "run-tui",
                "run_started",
                &serde_json::json!({ "prompt": "prove it" }),
            )
            .unwrap();
        store
            .append(
                "run-tui",
                "model_result",
                &serde_json::json!({ "content": "work complete", "stop_reason": "EndTurn" }),
            )
            .unwrap();
        store
            .append(
                "run-tui",
                "self_validation",
                &serde_json::json!({
                    "cmd": "cargo test --workspace",
                    "exit": 0,
                    "summary": "tests passed",
                    "workdir_hash": "sha256:fixture",
                    "evidence_only": true,
                    "verdict_unchanged": true,
                }),
            )
            .unwrap();
        store
            .append(
                "run-tui",
                "contract",
                &serde_json::json!({
                    "id": "contract-tui",
                    "title": "Fixture contract",
                    "intent": "Prove the review TUI renders verdict evidence.",
                }),
            )
            .unwrap();
        store
            .append(
                "run-tui",
                "verdict",
                &serde_json::json!({
                    "overall": "Pass",
                    "reviewed_diff": { "summary": "3 files changed, +42 -7" },
                    "usage": { "cost_usd": 0.0123, "total_tokens": 4096 },
                    "clauses": [
                        {
                            "kind": "check",
                            "desc": "workspace tests pass",
                            "verdict": "Pass",
                            "evidence": "$ cargo test --workspace\n[exit 0]\nok"
                        },
                        {
                            "kind": "review",
                            "desc": "the implementation satisfies VR-2",
                            "verdict": "Pass",
                            "evidence": "panel: 2/3 reviewers say met"
                        }
                    ]
                }),
            )
            .unwrap();
        store.set_status("run-tui", "completed").unwrap();
        // A signed human sign-off so side-by-side machine/human renders.
        store
            .record_human_review(
                "run-tui",
                HumanReviewDecision::Approved,
                Some("ready".into()),
            )
            .unwrap();
    }

    fn fixture_suspended_db(path: &std::path::Path) {
        let store = Store::open(path.to_str().unwrap(), Arc::new(Signer::generate())).unwrap();
        store
            .create_session("run-wait", Some("mock-model"), "local", "local")
            .unwrap();
        store
            .append(
                "run-wait",
                "run_started",
                &serde_json::json!({ "prompt": "wait for go" }),
            )
            .unwrap();
        store
            .append(
                "run-wait",
                "suspended",
                &serde_json::json!({
                    "await_kind": "signal",
                    "signal_name": "go",
                    "pending_tool_id": "tool-1",
                    "reason": "waiting for approval"
                }),
            )
            .unwrap();
        store.set_status("run-wait", "suspended").unwrap();
    }

    fn load_app(path: &std::path::Path) -> ReviewApp {
        let reader = LedgerReader::new(Store::open_readonly(path.to_str().unwrap()).unwrap());
        ReviewApp::load(&reader, path.display().to_string()).unwrap()
    }

    fn temp_db(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "truss-tui-{label}-{}-{unique}.db",
            std::process::id()
        ))
    }

    /// Render the app to a `TestBackend` buffer and return it as plain text.
    fn render_to_string(app: &ReviewApp, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn list_screen_renders_sessions_with_trust_and_human_status() {
        let db = temp_db("list");
        fixture_db(&db);
        let app = load_app(&db);
        assert!(!app.is_empty());
        let text = render_to_string(&app, 100, 24);
        assert!(text.contains("Truss review — sessions"));
        assert!(text.contains("run-tui"));
        assert!(text.contains("VERIFIED"));
        assert!(text.contains("status:completed"));
        assert!(text.contains("machine:pass"));
        assert!(text.contains("human:approved"));
        // Footer keybindings are visible.
        assert!(text.contains("verdict"));
        assert!(text.contains("quit"));
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn verdict_screen_renders_dod_evidence_panel_signoff_and_budget() {
        let db = temp_db("verdict");
        fixture_db(&db);
        let mut app = load_app(&db);
        let store = Store::open(&db.to_string_lossy(), Arc::new(Signer::generate())).unwrap();
        app.on_key(KeyCode::Enter, &store).unwrap();
        assert_eq!(app.screen(), Screen::Verdict);
        let text = render_to_string(&app, 110, 40);
        assert!(text.contains("Verdict bundle"));
        // Side-by-side machine vs human.
        assert!(text.contains("machine:"));
        assert!(text.contains("human:"));
        assert!(text.contains("approved"));
        // DoD + intent.
        assert!(text.contains("definition of done"));
        assert!(text.contains("satisfies VR-2"));
        // Clause + self-validation evidence.
        assert!(text.contains("clause evidence"));
        assert!(text.contains("workspace tests pass"));
        assert!(text.contains("self-validation evidence"));
        // Budget + diff summary.
        assert!(text.contains("3 files changed"));
        assert!(text.contains("4096 tokens"));
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn chain_screen_renders_head_and_verify_status() {
        let db = temp_db("chain");
        fixture_db(&db);
        let mut app = load_app(&db);
        let store = Store::open(&db.to_string_lossy(), Arc::new(Signer::generate())).unwrap();
        app.on_key(KeyCode::Char('c'), &store).unwrap();
        assert_eq!(app.screen(), Screen::Chain);
        let text = render_to_string(&app, 100, 30);
        assert!(text.contains("Ledger chain"));
        assert!(text.contains("head:"));
        assert!(text.contains("VERIFIED"));
        assert!(text.contains("intact"));
        assert!(text.contains("verdict"));
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn transcript_screen_is_reachable_but_not_default() {
        let db = temp_db("transcript");
        fixture_db(&db);
        let mut app = load_app(&db);
        let store = Store::open(&db.to_string_lossy(), Arc::new(Signer::generate())).unwrap();
        // Default screen is the list, never the transcript.
        assert_eq!(app.screen(), Screen::List);
        app.on_key(KeyCode::Char('t'), &store).unwrap();
        assert_eq!(app.screen(), Screen::Transcript);
        let text = render_to_string(&app, 100, 24);
        assert!(text.contains("Transcript"));
        assert!(text.contains("prove it"));
        assert!(text.contains("work complete"));
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn navigation_returns_to_list_and_quits() {
        let db = temp_db("nav");
        fixture_db(&db);
        let mut app = load_app(&db);
        let store = Store::open(&db.to_string_lossy(), Arc::new(Signer::generate())).unwrap();
        app.on_key(KeyCode::Enter, &store).unwrap();
        assert_eq!(app.screen(), Screen::Verdict);
        app.on_key(KeyCode::Esc, &store).unwrap();
        assert_eq!(app.screen(), Screen::List);
        assert!(!app.should_quit);
        app.on_key(KeyCode::Char('q'), &store).unwrap();
        assert!(app.should_quit);
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn review_action_records_signed_entry_and_refreshes_signoff() {
        let db = temp_db("action-review");
        fixture_db(&db);
        let store = Store::open(&db.to_string_lossy(), Arc::new(Signer::generate())).unwrap();
        let mut app = load_app(&db);

        app.on_key(KeyCode::Char('r'), &store).unwrap();
        for ch in "ship blocker".chars() {
            app.on_key(KeyCode::Char(ch), &store).unwrap();
        }
        app.on_key(KeyCode::Enter, &store).unwrap();

        assert!(app.status().unwrap().contains("signed rejected review"));
        let reader = LedgerReader::new(Store::open_readonly(db.to_str().unwrap()).unwrap());
        let verdict = reader.verdict("run-tui").unwrap();
        let signoff = verdict.human_signoff.unwrap();
        assert_eq!(signoff.decision, HumanReviewDecision::Rejected);
        assert_eq!(signoff.note.as_deref(), Some("ship blocker"));
        let verify = reader.verify("run-tui", None).unwrap();
        assert_eq!(verify.verification.status, VerificationStatus::Verified);
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn signal_action_delivers_pending_signal_for_suspended_run() {
        let db = temp_db("action-signal");
        fixture_suspended_db(&db);
        let store = Store::open(&db.to_string_lossy(), Arc::new(Signer::generate())).unwrap();
        let mut app = load_app(&db);
        let text = render_to_string(&app, 110, 24);
        assert!(text.contains("status:suspended"));
        assert!(text.contains("await:go"));

        app.on_key(KeyCode::Char('s'), &store).unwrap();
        for ch in r#"{"approved":true}"#.chars() {
            app.on_key(KeyCode::Char(ch), &store).unwrap();
        }
        app.on_key(KeyCode::Enter, &store).unwrap();

        assert!(app.status().unwrap().contains("delivered signal 'go'"));
        assert_eq!(
            store.take_pending_signal("run-wait", "go").unwrap(),
            Some(r#"{"approved":true}"#.into())
        );
        let _ = std::fs::remove_file(db);
    }
}
