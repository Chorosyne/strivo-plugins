//! Editor — transcript-as-timeline clip editor.
//!
//! Word-level in/out marks on the selected recording's transcript build
//! a clip list (Edit-kind EDL). The Concat verb produces an ffmpeg
//! lossless concat command that respects clip ordering; keyframe
//! alignment is snapped to nearest-keyframe (M5 — for now, clips with
//! sub-second drift fall back to the existing `recording::ffmpeg`
//! re-encode path used by Crunchr's clip export).
//!
//! M4 MVP scope (this commit):
//!   - E1: Timeline view + v/o/Enter marks, clip list, persistence as an
//!     `<recording>.edl.json` sidecar via the host's `edl::` module.
//!   - E2: Lossless concat export command synthesis (returns the ffmpeg
//!     argv for the host to spawn; actual spawning lives with the
//!     existing recording::ffmpeg path).
//! Deferred to M5 (kept as stubs in [`EditorView`]):
//!   - E3: Multi-VOD compilation
//!   - E4: Speaker filter cuts

use std::path::PathBuf;

use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
    Frame,
};

use strivo_core::app::{AppState, DaemonEvent};
use strivo_core::edl::{EdlDoc, EdlInput, EdlKind, EdlOp};
use strivo_core::plugin::{
    DaemonEventKind, PaneId, Plugin, PluginAction, PluginCommand, PluginContext, StatusSlot,
};

pub mod concat;

pub const EDITOR_PANE: PaneId = "editor";

/// In-memory clip on the user's working timeline. Persisted to the EDL
/// on `:save`; the sidecar JSON survives restarts.
#[derive(Debug, Clone)]
pub struct EditorClip {
    pub in_word: u32,
    pub out_word: u32,
    pub label: String,
}

pub struct EditorPlugin {
    /// Word-index cursor in the loaded transcript.
    cursor: u32,
    /// Currently-marked in-point (None until [v] is pressed).
    pending_in: Option<u32>,
    /// Clip list — appended to via [Enter] when both marks are set.
    clips: Vec<EditorClip>,
    /// Loaded transcript word stream. `(word_text, start_sec)`.
    words: Vec<(String, f64)>,
    /// Currently-loaded recording's path (for the eventual ffmpeg invocation).
    recording_path: Option<PathBuf>,
    /// Currently-loaded recording's uuid as text (for the EDL `vod_id`).
    recording_id: Option<String>,
    /// Crunchr DB path resolved at init.
    crunchr_db: PathBuf,
    /// View — Timeline (E1), Compilation (E3 stub), Filter (E4 stub).
    view: EditorView,
    /// Status surfaced to the host.
    last_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorView {
    Timeline,
    Compilation,
    Filter,
}

impl Default for EditorPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorPlugin {
    pub fn new() -> Self {
        Self {
            cursor: 0,
            pending_in: None,
            clips: Vec::new(),
            words: Vec::new(),
            recording_path: None,
            recording_id: None,
            crunchr_db: PathBuf::new(),
            view: EditorView::Timeline,
            last_status: None,
        }
    }

    fn open_crunchr_conn(&self) -> anyhow::Result<rusqlite::Connection> {
        let conn = rusqlite::Connection::open(&self.crunchr_db)?;
        conn.execute_batch("PRAGMA query_only = ON;")?;
        Ok(conn)
    }

    fn load_transcript(&mut self, app: &AppState) {
        let Some(rec_id) = app.selected_recording_id else {
            self.words.clear();
            self.recording_id = None;
            self.recording_path = None;
            self.last_status = Some("no recording selected".into());
            return;
        };
        let Some(rec) = app.recordings.get(&rec_id) else {
            return;
        };
        self.recording_path = Some(rec.output_path.clone());
        self.recording_id = Some(rec_id.to_string());

        let conn = match self.open_crunchr_conn() {
            Ok(c) => c,
            Err(e) => {
                self.last_status = Some(format!("crunchr.db unavailable: {e}"));
                return;
            }
        };

        match load_words(&conn, &rec_id.to_string()) {
            Ok(words) => {
                self.last_status =
                    Some(format!("{} words from crunchr transcript", words.len()));
                self.words = words;
                self.cursor = 0;
                self.pending_in = None;
            }
            Err(e) => {
                self.words.clear();
                self.last_status = Some(format!("no transcript: {e}"));
            }
        }

        // Re-hydrate the clip list if a sidecar exists.
        if let Some(path) = self.edl_path() {
            if path.exists() {
                match strivo_core::edl::load(&path) {
                    Ok(doc) => {
                        self.clips = doc
                            .ops
                            .iter()
                            .filter_map(|op| match op {
                                EdlOp::Clip {
                                    in_word,
                                    out_word,
                                    label,
                                } => Some(EditorClip {
                                    in_word: *in_word,
                                    out_word: *out_word,
                                    label: label.clone(),
                                }),
                                _ => None,
                            })
                            .collect();
                        self.last_status = Some(format!(
                            "{} words · {} clips loaded",
                            self.words.len(),
                            self.clips.len()
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "editor EDL load failed");
                    }
                }
            }
        }
    }

    /// Where the per-recording EDL sidecar lives — next to the recording.
    fn edl_path(&self) -> Option<PathBuf> {
        let path = self.recording_path.as_ref()?;
        let mut p = path.clone();
        p.set_extension("edl.json");
        Some(p)
    }

    /// Append a new clip from the pending in-mark and the cursor.
    fn finalize_clip(&mut self) {
        let Some(start) = self.pending_in else {
            self.last_status = Some("mark in with [v] first".into());
            return;
        };
        let end = self.cursor;
        if start >= end {
            self.last_status = Some("in must precede out — try selecting a later word".into());
            return;
        }
        let label = self
            .words
            .iter()
            .skip(start as usize)
            .take((end - start) as usize)
            .map(|(w, _)| w.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(48)
            .collect::<String>();
        self.clips.push(EditorClip {
            in_word: start,
            out_word: end,
            label,
        });
        self.pending_in = None;
        self.save_edl();
        self.last_status = Some(format!("clip added ({} total)", self.clips.len()));
    }

    /// Persist the EDL sidecar. Logs and surfaces failures via
    /// `last_status` but never panics — a write-failure shouldn't crash
    /// the user's session.
    fn save_edl(&mut self) {
        let Some(path) = self.edl_path() else { return };
        let Some(rid) = self.recording_id.clone() else {
            return;
        };
        let mut doc = EdlDoc::new(EdlKind::Edit, "editor-clips");
        doc.inputs.push(EdlInput {
            vod_id: rid,
            path: self.recording_path.clone(),
        });
        for c in &self.clips {
            doc.ops.push(EdlOp::Clip {
                in_word: c.in_word,
                out_word: c.out_word,
                label: c.label.clone(),
            });
        }
        if !self.clips.is_empty() {
            // Auto-add a Concat that joins every clip into the default
            // output. Users editing the EDL by hand can re-order this.
            doc.ops.push(EdlOp::Concat {
                clips: (0..self.clips.len()).collect(),
                output: "edit.mkv".into(),
            });
        }
        if let Err(e) = strivo_core::edl::save(&path, &doc) {
            self.last_status = Some(format!("EDL save failed: {e}"));
        }
    }
}

impl Plugin for EditorPlugin {
    fn name(&self) -> &'static str {
        "editor"
    }

    fn display_name(&self) -> &str {
        "Editor"
    }

    fn init(&mut self, ctx: &PluginContext) -> anyhow::Result<()> {
        if let Some(parent) = ctx.data_dir.parent() {
            self.crunchr_db = parent.join("crunchr").join("crunchr.db");
        }
        tracing::info!(
            plugin = "editor",
            db = %self.crunchr_db.display(),
            "Editor plugin initialized"
        );
        Ok(())
    }

    fn event_filter(&self) -> Option<Vec<DaemonEventKind>> {
        Some(vec![DaemonEventKind::RecordingFinished])
    }

    fn on_event(&mut self, event: &DaemonEvent, app: &AppState) -> Vec<PluginAction> {
        if matches!(event, DaemonEvent::RecordingFinished { .. }) {
            // A new transcript may exist; reload if the user is on the
            // affected recording.
            self.load_transcript(app);
        }
        Vec::new()
    }

    fn on_key(&mut self, key: KeyEvent, app: &AppState) -> Vec<PluginAction> {
        use crossterm::event::KeyCode::*;
        let was_empty = self.words.is_empty();
        match key.code {
            Char('r') => self.load_transcript(app),
            Char('1') => self.view = EditorView::Timeline,
            Char('2') => self.view = EditorView::Compilation,
            Char('3') => self.view = EditorView::Filter,
            _ => {}
        }
        if was_empty && self.words.is_empty() {
            // Lazy-load on first key.
            self.load_transcript(app);
        }
        if self.words.is_empty() {
            return Vec::new();
        }
        match key.code {
            Char('h') | Left => self.cursor = self.cursor.saturating_sub(1),
            Char('l') | Right => {
                if (self.cursor as usize) + 1 < self.words.len() {
                    self.cursor += 1;
                }
            }
            Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+W → jump 10 words ahead.
                self.cursor =
                    ((self.cursor as usize + 10).min(self.words.len() - 1)) as u32;
            }
            Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.cursor.saturating_sub(10);
            }
            Char('v') => {
                self.pending_in = Some(self.cursor);
                self.last_status =
                    Some(format!("in @ word {} — move cursor + Enter", self.cursor));
            }
            Char('o') => {
                if self.pending_in.is_some() {
                    self.finalize_clip();
                } else {
                    self.last_status = Some("press [v] to mark in first".into());
                }
            }
            Enter => self.finalize_clip(),
            Char('d') => {
                if !self.clips.is_empty() {
                    self.clips.pop();
                    self.save_edl();
                    self.last_status =
                        Some(format!("clip removed ({} total)", self.clips.len()));
                }
            }
            Char('x') => {
                // E2: synthesize concat command and surface the path.
                let argv = concat::build_concat_argv(&self.clips, &self.words, &self.recording_path);
                match argv {
                    Ok(_argv) => {
                        self.last_status = Some(format!(
                            "concat command ready ({} clips) — host spawn coming with C1 phase 2 pipeline refactor",
                            self.clips.len()
                        ));
                    }
                    Err(e) => {
                        self.last_status = Some(format!("concat synthesis failed: {e}"));
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![]
    }

    fn panes(&self) -> Vec<PaneId> {
        vec![EDITOR_PANE]
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
            .title(" Editor ")
            .title_style(Style::new().add_modifier(Modifier::BOLD));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [tabs_area, body_area, clip_area, hint_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(2),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        // Tab strip
        let tabs = Line::from(vec![
            Span::styled(
                tab_label("1", "Timeline", self.view == EditorView::Timeline),
                tab_style(self.view == EditorView::Timeline),
            ),
            Span::raw("  "),
            Span::styled(
                tab_label("2", "Compilation", self.view == EditorView::Compilation),
                tab_style(self.view == EditorView::Compilation),
            ),
            Span::raw("  "),
            Span::styled(
                tab_label("3", "Filter", self.view == EditorView::Filter),
                tab_style(self.view == EditorView::Filter),
            ),
        ]);
        frame.render_widget(Paragraph::new(tabs), tabs_area);

        match self.view {
            EditorView::Timeline => self.render_timeline(frame, body_area),
            EditorView::Compilation => render_stub(
                frame,
                body_area,
                "Compilation",
                "E3 stub — multi-VOD compilation. M5. Today: single-VOD only.",
            ),
            EditorView::Filter => render_stub(
                frame,
                body_area,
                "Speaker filter",
                "E4 stub — :filter-speaker include/exclude. M5.",
            ),
        }

        self.render_clip_list(frame, clip_area);

        let hint = Line::from(vec![
            Span::styled("[h/l]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" word  "),
            Span::styled("[v]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" mark in  "),
            Span::styled("[o/Enter]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" mark out + add  "),
            Span::styled("[d]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" del last  "),
            Span::styled("[x]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" concat  "),
            Span::styled("[r]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" reload"),
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

impl EditorPlugin {
    fn render_timeline(&self, frame: &mut Frame, area: Rect) {
        if self.words.is_empty() {
            let msg = if self.recording_path.is_some() {
                "Transcript has no word-level timings.\n\
                 Re-run Crunchr with a backend that emits them (whisperx-local)."
            } else {
                "Select a recording from the list, then press any key here to load it."
            };
            frame.render_widget(
                Paragraph::new(msg).wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        // Show a window of words around the cursor, highlighting the
        // pending in-mark and the cursor itself.
        let total = self.words.len();
        let half = (area.width as usize / 5).max(8); // ~5 chars per word average
        let start = self.cursor.saturating_sub(half as u32) as usize;
        let end = (self.cursor as usize + half).min(total);
        let mut spans: Vec<Span<'_>> = Vec::new();
        for i in start..end {
            let style = if i as u32 == self.cursor {
                Style::new()
                    .add_modifier(Modifier::REVERSED)
                    .add_modifier(Modifier::BOLD)
            } else if Some(i as u32) == self.pending_in {
                Style::new().add_modifier(Modifier::UNDERLINED)
            } else if self
                .clips
                .iter()
                .any(|c| (c.in_word..c.out_word).contains(&(i as u32)))
            {
                Style::new().add_modifier(Modifier::DIM).add_modifier(Modifier::UNDERLINED)
            } else {
                Style::new()
            };
            spans.push(Span::styled(self.words[i].0.clone(), style));
            spans.push(Span::raw(" "));
        }
        let timestamp = self
            .words
            .get(self.cursor as usize)
            .map(|(_, s)| format!(" @ {:.2}s", s))
            .unwrap_or_default();
        let header = Line::from(Span::styled(
            format!(
                "word {}/{}{timestamp}",
                self.cursor as usize + 1,
                total
            ),
            Style::new().add_modifier(Modifier::DIM),
        ));
        let body = Line::from(spans);
        frame.render_widget(
            Paragraph::new(vec![header, Line::raw(""), body]).wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_clip_list(&self, frame: &mut Frame, area: Rect) {
        let header = Line::from(Span::styled(
            format!("Clips ({})", self.clips.len()),
            Style::new()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::DIM),
        ));
        let mut lines = vec![header];
        if self.clips.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (none yet — [v] then [Enter] to add)",
                Style::new().add_modifier(Modifier::DIM),
            )));
        } else {
            for (i, c) in self.clips.iter().enumerate() {
                let start_secs = self
                    .words
                    .get(c.in_word as usize)
                    .map(|(_, s)| *s)
                    .unwrap_or(0.0);
                let end_secs = self
                    .words
                    .get(c.out_word as usize)
                    .map(|(_, s)| *s)
                    .unwrap_or(0.0);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {:>2}. ", i + 1),
                        Style::new().add_modifier(Modifier::DIM),
                    ),
                    Span::styled(
                        format!("{:.1}s→{:.1}s ", start_secs, end_secs),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(c.label.clone(), Style::new()),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
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

/// Pull word-level timings from Crunchr's segments table. Returns the
/// flattened word stream `(word_text, start_sec)`. Segments without
/// word_timings fall back to a synthetic uniform spread across the
/// segment's start/end so the user can still mark on them.
fn load_words(
    conn: &rusqlite::Connection,
    recording_id: &str,
) -> anyhow::Result<Vec<(String, f64)>> {
    let mut stmt = conn.prepare(
        "SELECT s.start_sec, s.end_sec, s.text, s.word_timings \
         FROM segments s \
         JOIN videos v ON v.id = s.video_id \
         WHERE v.recording_id = ?1 \
         ORDER BY s.segment_index",
    )?;
    let rows = stmt.query_map([recording_id], |row| {
        let start: f64 = row.get(0)?;
        let end: f64 = row.get(1)?;
        let text: String = row.get(2)?;
        let timings: Option<String> = row.get(3)?;
        Ok((start, end, text, timings))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (start, end, text, timings_json) = r?;
        if let Some(json) = timings_json {
            if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&json) {
                for v in parsed {
                    let w = v.get("w").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let s = v.get("s").and_then(|x| x.as_f64()).unwrap_or(start);
                    if !w.is_empty() {
                        out.push((w, s));
                    }
                }
                continue;
            }
        }
        // Fallback: synthesize uniform word timings over the segment.
        let words: Vec<&str> = text.split_whitespace().collect();
        let n = words.len() as f64;
        if n > 0.0 {
            let step = (end - start) / n;
            for (i, w) in words.iter().enumerate() {
                out.push((w.to_string(), start + step * i as f64));
            }
        }
    }
    Ok(out)
}
