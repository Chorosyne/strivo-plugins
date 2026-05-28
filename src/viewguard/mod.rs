//! Viewguard — viewbot detection for live streams.
//!
//! Subscribes to the host's channel-monitor stream, persists viewer
//! counts as a time series, runs statistical detectors (SpikeShape,
//! PlateauVariance, BenfordDigits), and surfaces verdicts in a TUI
//! pane plus the recording properties panel.
//!
//! Activation: `,b` per the plugin namespace (P4).

use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;

use chrono::Utc;
use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
    Frame,
};

use strivo_core::app::{AppState, DaemonEvent};
use strivo_core::platform::{ChannelEntry, PlatformKind};
use strivo_core::plugin::{
    DaemonEventKind, PaneId, Plugin, PluginAction, PluginContext, StatusSlot,
};

pub mod detectors;
pub mod score;
pub mod stats;
pub mod store;

use detectors::run_all;
use score::{aggregate, AggregatedVerdict, Band};
use stats::ChannelStats;
use store::{ViewguardStore, VerdictRow};

pub const VIEWGUARD_PANE: PaneId = "viewguard";

pub struct ViewguardPlugin {
    data_dir: PathBuf,
    store: Option<ViewguardStore>,
    channels: HashMap<String, ChannelStats>,
    /// Latest aggregated verdict per channel (in-memory mirror of `verdicts`).
    verdicts: HashMap<String, AggregatedVerdict>,
    cursor: usize,
    last_status: Option<String>,
}

impl Default for ViewguardPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewguardPlugin {
    pub fn new() -> Self {
        Self {
            data_dir: PathBuf::new(),
            store: None,
            channels: HashMap::new(),
            verdicts: HashMap::new(),
            cursor: 0,
            last_status: None,
        }
    }

    fn record_snapshot(&mut self, entries: &[ChannelEntry]) {
        let now = Utc::now();
        for entry in entries.iter().filter(|c| c.is_live) {
            let stats = self
                .channels
                .entry(entry.id.clone())
                .or_insert_with(|| {
                    ChannelStats::new(
                        entry.id.clone(),
                        format!("{}", entry.platform),
                        entry.display_name.clone(),
                    )
                });
            stats.mark_session_start(entry.started_at.unwrap_or(now));
            let viewers = entry.viewer_count.unwrap_or(0) as u32;
            if stats.push(now, viewers) {
                if let Some(s) = &self.store {
                    let _ = s.insert_sample(
                        &entry.id,
                        &platform_str(entry.platform),
                        now,
                        viewers,
                    );
                }
            }
        }
        // Run detectors on every poll for any channel with enough data.
        // Updates in-memory verdict; persistence happens on offline.
        let channel_ids: Vec<String> = self.channels.keys().cloned().collect();
        for channel_id in channel_ids {
            let stats = self.channels.get(&channel_id).unwrap().clone();
            if stats.session_started_at.is_none() {
                continue;
            }
            let signals = run_all(&stats);
            if signals.is_empty() {
                continue;
            }
            if let Some(s) = &self.store {
                for sig in &signals {
                    let _ = s.insert_signal(
                        &channel_id,
                        sig.kind.name(),
                        sig.score,
                        sig.confidence,
                        now,
                        &serde_json::to_string(&sig.evidence).unwrap_or_else(|_| "{}".into()),
                    );
                }
            }
            let verdict = aggregate(&signals);
            if !matches!(verdict.band, Band::Clean) {
                self.last_status = Some(format!(
                    "{} → {}",
                    stats.display_name,
                    verdict.band.as_str()
                ));
            }
            self.verdicts.insert(channel_id, verdict);
        }
    }

    fn close_session(&mut self, channel_id: &str) {
        let now = Utc::now();
        let Some(stats) = self.channels.get_mut(channel_id) else { return };
        let started_at = match stats.session_started_at {
            Some(t) => t,
            None => return,
        };
        let signals = run_all(stats);
        let verdict = aggregate(&signals);
        let contributors = serde_json::to_string(&verdict.contributors)
            .unwrap_or_else(|_| "[]".into());
        if let Some(s) = &self.store {
            let _ = s.upsert_verdict(&VerdictRow {
                channel_id: channel_id.to_string(),
                stream_started_at: started_at,
                stream_ended_at: Some(now),
                final_score: verdict.final_score,
                band: verdict.band.as_str().into(),
                contributors_json: contributors,
            });
        }
        self.verdicts.insert(channel_id.to_string(), verdict);
        stats.end_session();
    }
}

fn platform_str(p: PlatformKind) -> String {
    format!("{}", p)
}

impl Plugin for ViewguardPlugin {
    fn name(&self) -> &'static str { "viewguard" }
    fn display_name(&self) -> &str { "Viewguard" }

    fn init(&mut self, ctx: &PluginContext) -> anyhow::Result<()> {
        self.data_dir = ctx.data_dir.join("plugins").join("viewguard");
        std::fs::create_dir_all(&self.data_dir)?;
        let db_path = self.data_dir.join("viewguard.db");
        self.store = Some(ViewguardStore::open(&db_path)?);
        tracing::info!(
            plugin = "viewguard",
            db = %db_path.display(),
            "viewguard initialized",
        );
        Ok(())
    }

    fn event_filter(&self) -> Option<Vec<DaemonEventKind>> {
        Some(vec![
            DaemonEventKind::ChannelsUpdated,
            DaemonEventKind::ChannelWentLive,
            DaemonEventKind::ChannelWentOffline,
        ])
    }

    fn on_event(&mut self, event: &DaemonEvent, _app: &AppState) -> Vec<PluginAction> {
        match event {
            DaemonEvent::ChannelsUpdated(entries) => self.record_snapshot(entries),
            DaemonEvent::ChannelWentLive(entry) => {
                let now = Utc::now();
                let stats = self.channels.entry(entry.id.clone()).or_insert_with(|| {
                    ChannelStats::new(
                        entry.id.clone(),
                        platform_str(entry.platform),
                        entry.display_name.clone(),
                    )
                });
                stats.mark_session_start(entry.started_at.unwrap_or(now));
            }
            DaemonEvent::ChannelWentOffline(entry) => {
                self.close_session(&entry.id);
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_key(&mut self, key: KeyEvent, _app: &AppState) -> Vec<PluginAction> {
        use crossterm::event::KeyCode::*;
        let live_n = self.live_channel_ids().len();
        match key.code {
            Char('j') | Down => {
                if self.cursor + 1 < live_n { self.cursor += 1; }
            }
            Char('k') | Up => { self.cursor = self.cursor.saturating_sub(1); }
            Char('g') | Home => self.cursor = 0,
            Char('G') | End => self.cursor = live_n.saturating_sub(1),
            _ => {}
        }
        Vec::new()
    }

    fn panes(&self) -> Vec<PaneId> { vec![VIEWGUARD_PANE] }

    fn render_pane(&self, _pane_id: PaneId, frame: &mut Frame, area: Rect, _app: &AppState) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1))
            .title(" Viewguard — viewbot detection ")
            .title_style(Style::new().add_modifier(Modifier::BOLD));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [body, hint] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
        ]).areas(inner);

        let live_ids = self.live_channel_ids();
        if live_ids.is_empty() {
            let p = Paragraph::new(
                "No live channels under surveillance yet.\n\nViewguard kicks in when a tracked channel goes live; it then samples\nviewer counts every 30s and evaluates traffic patterns. Verdicts\nappear here and on the recording detail panel.",
            ).style(Style::new().add_modifier(Modifier::DIM)).wrap(Wrap { trim: false });
            frame.render_widget(p, body);
        } else {
            let cursor = self.cursor.min(live_ids.len() - 1);
            let mut lines: Vec<Line> = Vec::new();
            for (i, id) in live_ids.iter().enumerate() {
                let stats = &self.channels[id];
                let verdict = self.verdicts.get(id);
                let band = verdict.map(|v| v.band).unwrap_or(Band::Clean);
                let score = verdict.map(|v| v.final_score).unwrap_or(0.0);
                let chip_style = band_style(band);
                let marker = if i == cursor { "▶ " } else { "  " };
                let viewers_now = stats.samples.back().map(|s| s.viewers).unwrap_or(0);
                let spark = sparkline(&stats.values(), (body.width.saturating_sub(50)) as usize);
                lines.push(Line::from(vec![
                    Span::styled(marker.to_string(), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("{:<22}", truncate(&stats.display_name, 22)),
                        if i == cursor {
                            Style::new().add_modifier(Modifier::BOLD)
                        } else { Style::new() },
                    ),
                    Span::raw(" "),
                    Span::styled(format!("[{:<10}]", band.as_str()), chip_style),
                    Span::raw(" "),
                    Span::styled(format!("{:>5.2}", score), Style::new().add_modifier(Modifier::DIM)),
                    Span::raw(" "),
                    Span::raw(spark),
                    Span::raw(" "),
                    Span::styled(format!("{:>5}v", viewers_now), Style::new().add_modifier(Modifier::DIM)),
                ]));
                if i == cursor {
                    if let Some(v) = verdict {
                        for c in v.contributors.iter().take(3) {
                            lines.push(Line::from(vec![
                                Span::raw("       "),
                                Span::styled(
                                    format!("· {:<18}", c.detector),
                                    Style::new().add_modifier(Modifier::DIM),
                                ),
                                Span::styled(
                                    format!(" score {:.2}  conf {:.2}  weight {:.1}", c.score, c.confidence, c.weight),
                                    Style::new().add_modifier(Modifier::DIM),
                                ),
                            ]));
                        }
                    }
                }
            }
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
        }

        let hint_line = Line::from(vec![
            Span::styled("[j/k]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" nav  "),
            Span::styled("[,b]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" toggle pane"),
        ]);
        frame.render_widget(Paragraph::new(hint_line), hint);
    }

    fn status_line(&self, _app: &AppState) -> Option<String> {
        let suspect = self.verdicts
            .values()
            .filter(|v| matches!(v.band, Band::Suspect | Band::Fraudulent))
            .count();
        if suspect > 0 {
            Some(format!("viewguard: {suspect} suspect"))
        } else {
            self.last_status.clone()
        }
    }

    fn status_slot(&self) -> StatusSlot { StatusSlot::Tray }

    fn properties_section(
        &self,
        job_id: uuid::Uuid,
        app: &AppState,
    ) -> Vec<Line<'static>> {
        let Some(job) = app.recordings.get(&job_id) else { return Vec::new() };
        let channel_id = &job.channel_id;
        // Prefer the persisted latest verdict from the DB; fall back to
        // the in-memory one for the currently-live session.
        let row = self.store.as_ref().and_then(|s| s.latest_verdict(channel_id).ok().flatten());
        let mut out = Vec::new();
        out.push(Line::from(Span::styled(
            "Viewbot risk".to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        )));
        if let Some(r) = row {
            let band: Band = match r.band.as_str() {
                "fraudulent" => Band::Fraudulent,
                "suspect" => Band::Suspect,
                "watch" => Band::Watch,
                _ => Band::Clean,
            };
            out.push(Line::from(vec![
                Span::raw("  Verdict: "),
                Span::styled(format!("[{}]", r.band), band_style(band)),
                Span::raw(format!("  score {:.2}", r.final_score)),
            ]));
            out.push(Line::from(Span::styled(
                format!("  Sampled {} → {}",
                    r.stream_started_at.format("%Y-%m-%d %H:%M"),
                    r.stream_ended_at.map(|t| t.format("%H:%M").to_string()).unwrap_or_else(|| "live".into()),
                ),
                Style::new().add_modifier(Modifier::DIM),
            )));
        } else if let Some(v) = self.verdicts.get(channel_id) {
            out.push(Line::from(vec![
                Span::raw("  Verdict: "),
                Span::styled(format!("[{}]", v.band.as_str()), band_style(v.band)),
                Span::raw(format!("  score {:.2}", v.final_score)),
                Span::styled("  (live)".to_string(), Style::new().add_modifier(Modifier::DIM)),
            ]));
        } else {
            out.push(Line::from(Span::styled(
                "  No samples yet.".to_string(),
                Style::new().add_modifier(Modifier::DIM),
            )));
        }
        out
    }

    fn as_any(&self) -> &dyn Any { self }
    fn as_any_mut(&mut self) -> &mut dyn Any { self }
}

impl ViewguardPlugin {
    fn live_channel_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.channels
            .iter()
            .filter(|(_, s)| s.session_started_at.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        ids.sort();
        ids
    }
}

fn band_style(band: Band) -> Style {
    match band {
        Band::Clean => Style::new().fg(Color::Green),
        Band::Watch => Style::new().fg(Color::Yellow),
        Band::Suspect => Style::new().fg(Color::Rgb(255, 140, 0)).add_modifier(Modifier::BOLD),
        Band::Fraudulent => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

/// Unicode sparkline. Maps a value slice into braille-ish blocks.
fn sparkline(values: &[u32], width: usize) -> String {
    if values.is_empty() || width == 0 { return String::new(); }
    const BLOCKS: [char; 8] = ['▁','▂','▃','▄','▅','▆','▇','█'];
    let n = values.len();
    let step = (n as f32 / width as f32).max(1.0);
    let mut buckets: Vec<u32> = Vec::with_capacity(width);
    let mut i = 0.0_f32;
    while i < n as f32 && buckets.len() < width {
        let start = i as usize;
        let end = ((i + step) as usize).min(n);
        let slice = &values[start..end.max(start + 1)];
        let avg = slice.iter().sum::<u32>() / slice.len().max(1) as u32;
        buckets.push(avg);
        i += step;
    }
    let max = *buckets.iter().max().unwrap_or(&1);
    let min = *buckets.iter().min().unwrap_or(&0);
    let range = (max - min).max(1) as f32;
    buckets.iter()
        .map(|&v| {
            let t = ((v - min) as f32 / range * 7.0).round() as usize;
            BLOCKS[t.min(7)]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_handles_flat_input() {
        let s = sparkline(&[5, 5, 5, 5], 4);
        assert_eq!(s.chars().count(), 4);
    }

    #[test]
    fn sparkline_empty() {
        assert_eq!(sparkline(&[], 10), "");
        assert_eq!(sparkline(&[1,2,3], 0), "");
    }

    #[test]
    fn sparkline_renders_gradient() {
        let s = sparkline(&[1, 50, 100], 3);
        assert_eq!(s.chars().count(), 3);
        // ramp up: last char should be tallest
        let last = s.chars().last().unwrap();
        assert_eq!(last, '█');
    }
}
