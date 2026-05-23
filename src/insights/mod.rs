//! Insights — terminal-native data viz for the StriVo transcript corpus.
//!
//! Reads Crunchr's existing `word_frequency` / `segments` / `video_analysis`
//! tables (zero new schema) and surfaces them as ASCII histograms, speaker
//! airtime bars, and dataset exports (CSV / JSON). VisiData-in-spirit.
//!
//! M4 MVP scope:
//!   - I1: Word frequency histogram with stopword toggle.
//!   - I4: CSV / JSON export of the current view's data.
//!   - I2 / I3 / I5 stubbed as TODO branches the user can grow into.
//!
//! Activation: `,i` per the plugin namespace (P4).

use std::path::PathBuf;

use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
    Frame,
};
use rusqlite::Connection;

use strivo_core::app::{AppState, DaemonEvent};
use strivo_core::plugin::{
    DaemonEventKind, PaneId, Plugin, PluginAction, PluginCommand, PluginContext, StatusSlot,
};

pub mod export;
pub mod frequency;

pub const INSIGHTS_PANE: PaneId = "insights";

pub struct InsightsPlugin {
    db_path: PathBuf,
    /// In-memory view state — keeps the active view, cursor, filters.
    view: InsightsView,
    /// Word-frequency data for the currently-loaded recording (or aggregate
    /// across all recordings if `selected_recording_id` is None).
    cached_freq: Vec<frequency::FrequencyRow>,
    cursor: usize,
    show_stopwords: bool,
    /// Last status string surfaced via [`Plugin::status_line`].
    last_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsightsView {
    /// Bar chart of top words. Default landing view.
    Frequency,
    /// Stub: speaker airtime + sentiment over time (I2 — M5).
    Speakers,
    /// Stub: cross-recording topic graph (I3 — M5).
    Topics,
}

impl Default for InsightsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl InsightsPlugin {
    pub fn new() -> Self {
        Self {
            db_path: PathBuf::new(),
            view: InsightsView::Frequency,
            cached_freq: Vec::new(),
            cursor: 0,
            show_stopwords: false,
            last_status: None,
        }
    }

    fn open_conn(&self) -> anyhow::Result<Connection> {
        let conn = Connection::open(&self.db_path)?;
        conn.execute_batch("PRAGMA query_only = ON;")?;
        Ok(conn)
    }

    fn refresh_freq(&mut self, app: &AppState) {
        let limit = 50;
        let conn = match self.open_conn() {
            Ok(c) => c,
            Err(e) => {
                self.last_status = Some(format!("crunchr.db unavailable: {e}"));
                self.cached_freq.clear();
                return;
            }
        };
        let recording_id = app.selected_recording_id.map(|u| u.to_string());
        let rows = if let Some(rid) = recording_id {
            frequency::top_words_for_recording(&conn, &rid, limit, self.show_stopwords)
        } else {
            frequency::top_words_global(&conn, limit, self.show_stopwords)
        };
        match rows {
            Ok(rs) => {
                if self.cursor >= rs.len() {
                    self.cursor = rs.len().saturating_sub(1);
                }
                self.cached_freq = rs;
                self.last_status = Some(format!("{} words", self.cached_freq.len()));
            }
            Err(e) => {
                self.cached_freq.clear();
                self.last_status = Some(format!("query failed: {e}"));
            }
        }
    }
}

impl Plugin for InsightsPlugin {
    fn name(&self) -> &'static str {
        "insights"
    }

    fn display_name(&self) -> &str {
        "Insights"
    }

    fn init(&mut self, ctx: &PluginContext) -> anyhow::Result<()> {
        // Look up Crunchr's DB next to our own data dir. Crunchr writes
        // to `<data_dir>/plugins/crunchr/crunchr.db`; we read it.
        if let Some(parent) = ctx.data_dir.parent() {
            self.db_path = parent.join("crunchr").join("crunchr.db");
        }
        tracing::info!(
            plugin = "insights",
            db = %self.db_path.display(),
            "Insights plugin initialized"
        );
        Ok(())
    }

    fn event_filter(&self) -> Option<Vec<DaemonEventKind>> {
        // Insights cares about recording selection changes (to refresh the
        // per-recording frequency) and RecordingFinished (a new transcript
        // arrived). The host doesn't expose a SelectionChanged event yet,
        // so we re-refresh lazily on pane activation in on_event.
        Some(vec![DaemonEventKind::RecordingFinished])
    }

    fn on_event(&mut self, event: &DaemonEvent, app: &AppState) -> Vec<PluginAction> {
        if matches!(event, DaemonEvent::RecordingFinished { .. }) {
            self.refresh_freq(app);
        }
        Vec::new()
    }

    fn on_key(&mut self, key: KeyEvent, app: &AppState) -> Vec<PluginAction> {
        use crossterm::event::KeyCode::*;
        match key.code {
            Char('j') | Down => {
                if self.cursor + 1 < self.cached_freq.len() {
                    self.cursor += 1;
                }
            }
            Char('k') | Up => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            Char('g') | Home => self.cursor = 0,
            Char('G') | End => {
                self.cursor = self.cached_freq.len().saturating_sub(1)
            }
            Char('s') => {
                self.show_stopwords = !self.show_stopwords;
                self.refresh_freq(app);
            }
            Char('r') => self.refresh_freq(app),
            Char('e') => {
                // CSV export.
                let path = export::export_csv(&self.cached_freq);
                self.last_status = match path {
                    Ok(p) => Some(format!("exported CSV → {}", p.display())),
                    Err(e) => Some(format!("CSV export failed: {e}")),
                };
            }
            Char('J') => {
                let path = export::export_json(&self.cached_freq);
                self.last_status = match path {
                    Ok(p) => Some(format!("exported JSON → {}", p.display())),
                    Err(e) => Some(format!("JSON export failed: {e}")),
                };
            }
            Char('1') => self.view = InsightsView::Frequency,
            Char('2') => self.view = InsightsView::Speakers,
            Char('3') => self.view = InsightsView::Topics,
            _ => {}
        }
        Vec::new()
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![]
    }

    fn panes(&self) -> Vec<PaneId> {
        vec![INSIGHTS_PANE]
    }

    fn render_pane(
        &self,
        _pane_id: PaneId,
        frame: &mut Frame,
        area: Rect,
        _app: &AppState,
    ) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1))
            .title(" Insights ")
            .title_style(Style::new().add_modifier(Modifier::BOLD));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [tabs_area, body_area, hint_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        // Tab strip
        let tabs = Line::from(vec![
            Span::styled(
                tab_label("1", "Frequency", self.view == InsightsView::Frequency),
                tab_style(self.view == InsightsView::Frequency),
            ),
            Span::raw("  "),
            Span::styled(
                tab_label("2", "Speakers", self.view == InsightsView::Speakers),
                tab_style(self.view == InsightsView::Speakers),
            ),
            Span::raw("  "),
            Span::styled(
                tab_label("3", "Topics", self.view == InsightsView::Topics),
                tab_style(self.view == InsightsView::Topics),
            ),
        ]);
        frame.render_widget(Paragraph::new(tabs), tabs_area);

        // Body
        match self.view {
            InsightsView::Frequency => render_frequency_body(frame, body_area, &self.cached_freq, self.cursor),
            InsightsView::Speakers => render_stub(
                frame,
                body_area,
                "Speaker airtime + sentiment trend",
                "I2 stub — landing in M5. Today the data is in segments + video_analysis.",
            ),
            InsightsView::Topics => render_stub(
                frame,
                body_area,
                "Cross-recording topic graph",
                "I3 stub — landing in M5. Today topics live in video_analysis.topics JSON.",
            ),
        }

        // Hint bar
        let stopword_label = if self.show_stopwords { "show all" } else { "hide stopwords" };
        let hint = Line::from(vec![
            Span::styled("[j/k]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" nav  "),
            Span::styled("[s]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!(" {stopword_label}  ")),
            Span::styled("[e]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" CSV  "),
            Span::styled("[J]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" JSON  "),
            Span::styled("[r]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" refresh  "),
            Span::styled("[1/2/3]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" view"),
        ]);
        frame.render_widget(Paragraph::new(hint).wrap(Wrap { trim: true }), hint_area);
    }

    fn status_line(&self, _app: &AppState) -> Option<String> {
        self.last_status.clone()
    }

    fn status_slot(&self) -> StatusSlot {
        StatusSlot::Tray
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn tab_label(key: &str, name: &str, active: bool) -> String {
    if active {
        format!("[{key}] {name}")
    } else {
        format!(" {key}  {name}")
    }
}

fn tab_style(active: bool) -> Style {
    if active {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::DIM)
    }
}

fn render_frequency_body(
    frame: &mut Frame,
    area: Rect,
    rows: &[frequency::FrequencyRow],
    cursor: usize,
) {
    if rows.is_empty() {
        let p = Paragraph::new("No transcripts indexed yet. Process a recording with Crunchr first.")
            .style(Style::new().add_modifier(Modifier::DIM));
        frame.render_widget(p, area);
        return;
    }
    let max = rows.iter().map(|r| r.count).max().unwrap_or(1).max(1);
    let bar_width = area.width.saturating_sub(22) as usize;
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let bar_len = (r.count as f64 / max as f64 * bar_width as f64).round() as usize;
            let bar: String = "█".repeat(bar_len);
            let marker = if i == cursor { "▶ " } else { "  " };
            Line::from(vec![
                Span::styled(marker.to_string(), Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("{:<14}", truncate(&r.word, 14)),
                    if i == cursor {
                        Style::new().add_modifier(Modifier::BOLD)
                    } else {
                        Style::new()
                    },
                ),
                Span::raw(" "),
                Span::raw(bar),
                Span::raw(" "),
                Span::styled(
                    format!("{}", r.count),
                    Style::new().add_modifier(Modifier::DIM),
                ),
            ])
        })
        .collect();
    let scroll_off = if cursor + 4 > area.height as usize {
        (cursor + 4) - area.height as usize
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(lines).scroll((scroll_off as u16, 0)),
        area,
    );
}

fn render_stub(frame: &mut Frame, area: Rect, title: &str, body: &str) {
    let lines = vec![
        Line::from(Span::styled(
            title.to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(body.to_string(), Style::new().add_modifier(Modifier::DIM))),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}
