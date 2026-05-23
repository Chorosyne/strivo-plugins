pub mod analysis;
pub mod cost;
mod db;
mod pipeline;
pub mod render;
pub mod transcribe;
pub mod types;
pub mod voice_samples;

use std::any::Any;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::Frame;
use uuid::Uuid;

use strivo_core::app::{AppState, DaemonEvent};
use strivo_core::config::{CrunchrAnalysisConfig, CrunchrConfig};
use strivo_core::recording::job::RecordingState;

use strivo_core::plugin::{
    DaemonEventKind, PaneId, Plugin, PluginAction, PluginCommand, PluginContext,
};
use types::{
    AnalysisData, ConfigModalState, CrunchrView, PickerState, PipelineEvent, PipelineState,
    ProcessingJob, RecordingFilter, SearchResult, SpeakerModalState, SpeakerRow,
};

pub const PANE_ID: PaneId = "crunchr";

/// Sanitize a string for use in a filename — strip path separators,
/// control chars, and characters Windows hates. Spaces collapse to
/// underscores so users can shell-tab-complete the output.
fn sanitize_for_filename(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if c.is_whitespace() {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("clip");
    }
    out
}

/// Format a seek position as `HHhMMmSSs` for use in a filename.
fn format_timestamp(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else {
        format!("{m:02}m{s:02}s")
    }
}

/// Disambiguate a target path that already exists. Matches the
/// convention used by `recording::build_output_path` so users see one
/// rule across the codebase.
fn disambiguate(target: &std::path::Path) -> PathBuf {
    if !target.exists() {
        return target.to_path_buf();
    }
    let parent = target.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("clip");
    let ext = target.extension().and_then(|s| s.to_str()).unwrap_or("mkv");
    for n in 1..=999u32 {
        let candidate = parent.join(format!("{stem}_{n}.{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{stem}_{}.{ext}", uuid::Uuid::new_v4()))
}

/// Spawn `ffmpeg -ss <start> -to <end> -c copy <source> <dest>`. No
/// re-encode, sub-second elapsed for any reasonable clip length. The
/// container is matroska so we don't care about mp4-style atoms.
async fn export_clip(
    source: &std::path::Path,
    start: f64,
    end: f64,
    clips_dir: &std::path::Path,
    stem: &str,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(clips_dir)?;
    let initial = clips_dir.join(format!("{stem}.mkv"));
    let dest = disambiguate(&initial);
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-ss",
            &format!("{start:.3}"),
            "-to",
            &format!("{end:.3}"),
            "-i",
        ])
        .arg(source)
        .args(["-c", "copy", "-avoid_negative_ts", "make_zero"])
        .arg(&dest)
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("ffmpeg exited with status {status}");
    }
    Ok(dest)
}

/// Number of static config fields in the Crunchr config modal (before channel checklist).
/// Fields: enabled, backend, api_key, endpoint, whisper_model, analysis_enabled = 6 fields.
/// Channel checkboxes start at index CRUNCHR_STATIC_FIELDS (i.e., after index 5).
const CRUNCHR_STATIC_FIELDS: usize = 6;

pub struct CrunchrPlugin {
    db: Option<rusqlite::Connection>,
    data_dir: PathBuf,
    /// Concurrency guard: recording IDs currently being processed.
    in_flight: HashSet<Uuid>,
    /// Transcription backend (whisper-cli or voxtral).
    backend: Option<Arc<dyn transcribe::TranscriptionBackend>>,
    /// Analysis config for OpenRouter LLM.
    analysis_config: Option<CrunchrAnalysisConfig>,
    pub queue: Vec<ProcessingJob>,
    pub search_results: Vec<SearchResult>,
    pub search_query: String,
    pub search_mode: types::SearchMode,
    pub selected_result: usize,
    pub input_active: bool,
    pub word_frequencies: Vec<(String, i64)>,
    pub backend_available: bool,
    /// Last error message for display.
    pub last_error: Option<String>,
    /// Analysis data for the currently selected search result.
    pub selected_analysis: Option<AnalysisData>,
    /// Speaker label for the currently selected search result.
    pub selected_speaker: Option<String>,
    /// Previous selected_result index (for detecting selection changes).
    prev_selected: usize,

    // --- New: config modal, views, tandem ---
    /// Whether the plugin is enabled for tandem auto-processing.
    pub enabled: bool,
    /// Whether the first-run config has been completed.
    pub configured: bool,
    /// Tandem channels (auto-trigger on RecordingFinished).
    pub tandem_channels: Vec<String>,
    /// Tandem playlists.
    pub tandem_playlists: Vec<String>,
    /// Config modal state.
    pub config_modal: ConfigModalState,
    /// Draft config being edited in the modal.
    pub config_draft: Option<CrunchrConfig>,
    /// Speaker Editor modal state.
    pub speaker_modal: SpeakerModalState,
    /// Current view mode.
    pub view: CrunchrView,
    /// Recording picker state.
    pub picker: PickerState,
    /// Cached channel list for the config modal tandem checkboxes.
    pub cached_channels: Vec<(String, String)>, // (channel_key "Platform:id", display_name)

    /// Recording IDs that should transcribe with the diarization-capable
    /// fallback backend instead of the configured one. Populated when a
    /// crunchr-auto job arrives for a backend lacking diarization.
    diarize_override_jobs: HashSet<Uuid>,
    /// Lazily-initialized voxtral-api backend used as the diarization fallback.
    diarize_backend: Option<Arc<dyn transcribe::TranscriptionBackend>>,
    /// Env var to read the Mistral API key from (cached from `CrunchrConfig.api_key_env`
    /// at init time). Falls back to `MISTRAL_API_KEY`. Only consulted when initializing
    /// the diarization fallback backend.
    diarize_api_key_env: Option<String>,
    /// Mirror of `CrunchrConfig.embed_subs` cached at init so the
    /// TranscriptionComplete handler can decide whether to call mkvmerge.
    embed_subs: bool,
    /// Mirror of `CrunchrConfig.diarize` cached at init. Surfaced in the UI
    /// (Speaker Editor availability + no-backend hint).
    diarize_requested: bool,
}

impl Default for CrunchrPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl CrunchrPlugin {
    pub fn new() -> Self {
        Self {
            db: None,
            data_dir: PathBuf::new(),
            in_flight: HashSet::new(),
            backend: None,
            analysis_config: None,
            queue: Vec::new(),
            search_results: Vec::new(),
            search_query: String::new(),
            search_mode: types::SearchMode::FullText,
            selected_result: 0,
            input_active: false,
            word_frequencies: Vec::new(),
            backend_available: false,
            last_error: None,
            selected_analysis: None,
            selected_speaker: None,
            prev_selected: usize::MAX,
            enabled: false,
            configured: false,
            tandem_channels: Vec::new(),
            tandem_playlists: Vec::new(),
            config_modal: ConfigModalState::Hidden,
            config_draft: None,
            speaker_modal: SpeakerModalState::Hidden,
            view: CrunchrView::Search,
            picker: PickerState::default(),
            cached_channels: Vec::new(),
            diarize_override_jobs: HashSet::new(),
            diarize_backend: None,
            diarize_api_key_env: None,
            embed_subs: true,
            diarize_requested: false,
        }
    }

    /// If `recording_id` should transcribe with the diarization fallback,
    /// return a clone of that backend; otherwise None. Caller falls back to
    /// `self.backend` when this returns None.
    fn pick_backend_for(
        &self,
        recording_id: Uuid,
    ) -> Option<Arc<dyn transcribe::TranscriptionBackend>> {
        if self.diarize_override_jobs.contains(&recording_id) {
            self.diarize_backend.clone()
        } else {
            None
        }
    }

    /// Lazily initialize the voxtral-api backend used as the diarization fallback.
    /// Looks for `MISTRAL_API_KEY` (or whatever env var is configured under
    /// `crunchr.api_key_env`). Returns true if the fallback is now usable.
    fn try_init_diarize_backend(&mut self) -> bool {
        if self.diarize_backend.is_some() {
            return true;
        }
        let var = self
            .diarize_api_key_env
            .as_deref()
            .unwrap_or("MISTRAL_API_KEY");
        match std::env::var(var).ok() {
            Some(key) if !key.is_empty() => {
                self.diarize_backend = Some(Arc::new(
                    transcribe::voxtral_api::VoxtralApiBackend::new(key),
                ));
                tracing::info!(
                    "crunchr: initialized voxtral-api fallback for diarization (env: {var})"
                );
                true
            }
            _ => false,
        }
    }

    fn execute_search(&mut self) {
        if self.search_query.is_empty() {
            self.search_results.clear();
            return;
        }

        let Some(conn) = self.db.as_ref() else {
            self.last_error = Some("DB not available".to_string());
            return;
        };

        match db::fts_search(conn, &self.search_query, 50) {
            Ok(results) => {
                self.search_results = results;
                self.last_error = None;
            }
            Err(e) => {
                tracing::warn!("Search error: {e}");
                self.search_results.clear();
                self.last_error = Some(format!("Search error: {e}"));
            }
        }
        self.selected_result = 0;
        self.refresh_selected_analysis();
    }

    fn refresh_selected_analysis(&mut self) {
        self.selected_analysis = None;
        self.selected_speaker = None;

        let Some(result) = self.search_results.get(self.selected_result) else {
            return;
        };
        let Some(conn) = self.db.as_ref() else { return };

        self.selected_analysis = db::get_analysis_for_chunk(conn, result.chunk_id)
            .ok()
            .flatten();
        self.selected_speaker = db::get_speaker_for_chunk(conn, result.chunk_id)
            .ok()
            .flatten();
        self.prev_selected = self.selected_result;
    }

    fn refresh_word_frequencies(&mut self) {
        let Some(conn) = self.db.as_ref() else { return };
        match db::get_top_words(conn, 20) {
            Ok(words) => self.word_frequencies = words,
            Err(e) => tracing::warn!("Word frequency error: {e}"),
        }
    }

    /// Query transcript/analysis info for a recording (used by properties modal).
    pub fn recording_info(&self, recording_id: &str) -> Option<types::CrunchrRecordingInfo> {
        let conn = self.db.as_ref()?;

        // Get video status
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM videos WHERE recording_id = ?1",
                [recording_id],
                |row| row.get(0),
            )
            .ok();

        let status = status?;

        // Get video ID for further queries
        let video_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM videos WHERE recording_id = ?1",
                [recording_id],
                |row| row.get(0),
            )
            .ok();

        let video_id = video_id?;

        // Count segments
        let segment_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE video_id = ?1",
                [video_id],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .unwrap_or(0) as usize;

        // Count words (sum of word frequencies)
        let word_count: usize = conn
            .query_row(
                "SELECT COALESCE(SUM(count), 0) FROM word_frequency WHERE video_id = ?1",
                [video_id],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .unwrap_or(0) as usize;

        // Get analysis data
        let analysis: Option<(String, String, String)> = conn
            .query_row(
                "SELECT summary, topics, sentiment FROM video_analysis WHERE video_id = ?1",
                [video_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .ok();

        let (has_analysis, summary, topics, sentiment) = if let Some((s, t, sent)) = analysis {
            let topic_list: Vec<String> = serde_json::from_str(&t).unwrap_or_default();
            (true, Some(s), topic_list, Some(sent))
        } else {
            (false, None, Vec::new(), None)
        };

        // M5.6 — token usage + cost columns. The columns are guaranteed
        // present by the additive migration in db::open_and_init; default
        // values (0) flow through for un-analyzed rows.
        let (prompt_tokens, completion_tokens, cost_cents): (i64, i64, i64) = conn
            .query_row(
                "SELECT prompt_tokens, completion_tokens, cost_cents FROM videos WHERE id = ?1",
                [video_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap_or((0, 0, 0));

        Some(types::CrunchrRecordingInfo {
            status,
            segment_count,
            word_count,
            has_analysis,
            summary,
            topics,
            sentiment,
            prompt_tokens: prompt_tokens.max(0) as u64,
            completion_tokens: completion_tokens.max(0) as u64,
            cost_cents: cost_cents.max(0) as u64,
        })
    }

    fn queue_recording(
        &mut self,
        recording_id: Uuid,
        channel_name: String,
        title: String,
        video_path: PathBuf,
    ) -> Vec<PluginAction> {
        if self.in_flight.contains(&recording_id) {
            return Vec::new();
        }
        if self.queue.iter().any(|j| j.recording_id == recording_id) {
            return Vec::new();
        }

        self.in_flight.insert(recording_id);

        // Diarization fallback: if this job came from a catalog-pull (`.crunchr-auto`
        // marker present in the episode dir) and the configured backend doesn't
        // support diarization, transparently switch to voxtral-api when MISTRAL_API_KEY
        // is set. Otherwise fall through with a warning — non-diarized output is fine.
        let auto_marker = video_path
            .parent()
            .map(|p| p.join(".crunchr-auto").exists())
            .unwrap_or(false);
        let backend_supports_diarize = self
            .backend
            .as_ref()
            .map(|b| b.supports_diarization())
            .unwrap_or(false);
        if auto_marker && !backend_supports_diarize {
            if self.try_init_diarize_backend() {
                self.diarize_override_jobs.insert(recording_id);
            } else {
                tracing::warn!(
                    "crunchr: diarization requested for {recording_id} but no MISTRAL_API_KEY; falling back to non-diarized transcription"
                );
            }
        }

        if let Some(conn) = self.db.as_ref() {
            if let Err(e) = db::insert_video(
                conn,
                &recording_id.to_string(),
                &channel_name,
                &title,
                &video_path.to_string_lossy(),
            ) {
                self.last_error = Some(format!("DB insert error: {e}"));
                self.in_flight.remove(&recording_id);
                return vec![PluginAction::SetStatus(format!("CrunchR DB error: {e}"))];
            }
        }

        let job = ProcessingJob {
            recording_id,
            channel_name,
            title,
            video_path,
            audio_path: None,
            state: PipelineState::Pending,
            error: None,
        };
        self.queue.push(job);

        self.start_next_stage(recording_id)
    }

    fn start_next_stage(&mut self, recording_id: Uuid) -> Vec<PluginAction> {
        let Some(job_idx) = self
            .queue
            .iter()
            .position(|j| j.recording_id == recording_id)
        else {
            return Vec::new();
        };

        let current_state = self.queue[job_idx].state;

        match current_state {
            PipelineState::Pending => {
                let video_path = self.queue[job_idx].video_path.clone();
                self.queue[job_idx].state = PipelineState::ExtractingAudio;
                if let Some(conn) = self.db.as_ref() {
                    let _ = db::update_video_status(
                        conn,
                        &recording_id.to_string(),
                        "extracting_audio",
                        None,
                    );
                }
                let output_dir = self.data_dir.join("audio");
                vec![PluginAction::SpawnTask {
                    plugin_name: "crunchr",
                    future: Box::pin(pipeline::extract_audio(
                        recording_id,
                        video_path,
                        output_dir,
                    )),
                }]
            }
            PipelineState::ExtractingAudio => {
                if !self.backend_available {
                    self.queue[job_idx].state = PipelineState::Failed;
                    self.queue[job_idx].error =
                        Some("No transcription backend available".to_string());
                    self.in_flight.remove(&recording_id);
                    return vec![PluginAction::SetStatus(
                        "CrunchR: no transcription backend".to_string(),
                    )];
                }
                let audio_path = self.queue[job_idx].audio_path.clone().unwrap_or_default();
                self.queue[job_idx].state = PipelineState::Transcribing;
                if let Some(conn) = self.db.as_ref() {
                    let _ = db::update_video_status(
                        conn,
                        &recording_id.to_string(),
                        "transcribing",
                        None,
                    );
                }
                // Per-job backend override (used when a catalog-pull job needs
                // diarization but the configured backend can't provide it).
                let backend = self
                    .pick_backend_for(recording_id)
                    .or_else(|| self.backend.clone())
                    .unwrap();
                vec![PluginAction::SpawnTask {
                    plugin_name: "crunchr",
                    future: Box::pin(async move {
                        // Retry transient transcription errors. Three
                        // attempts with 5 / 10 / 30 s backoff matches the
                        // ROADMAP M1 brief; a single API timeout no
                        // longer kills the whole job.
                        const BACKOFFS_SECS: [u64; 3] = [5, 10, 30];
                        let mut last_err: Option<String> = None;
                        for (attempt, secs) in BACKOFFS_SECS.iter().enumerate() {
                            match backend.transcribe(&audio_path).await {
                                Ok(result) => {
                                    return Box::new(PipelineEvent::TranscriptionComplete {
                                        recording_id,
                                        segments: result.segments,
                                        full_text: result.full_text,
                                    })
                                        as Box<dyn Any + Send>;
                                }
                                Err(e) => {
                                    let msg = format!("{e}");
                                    tracing::warn!(
                                        recording_id = %recording_id,
                                        attempt = attempt + 1,
                                        error = %msg,
                                        "transcription attempt failed; retrying"
                                    );
                                    last_err = Some(msg);
                                    if attempt + 1 < BACKOFFS_SECS.len() {
                                        tokio::time::sleep(std::time::Duration::from_secs(*secs))
                                            .await;
                                    }
                                }
                            }
                        }
                        Box::new(PipelineEvent::StageError {
                            recording_id,
                            error: format!(
                                "Transcription failed after 3 attempts: {}",
                                last_err.unwrap_or_else(|| "unknown".into())
                            ),
                        }) as Box<dyn Any + Send>
                    }),
                }]
            }
            PipelineState::Transcribing => {
                self.queue[job_idx].state = PipelineState::Chunking;
                if let Some(conn) = self.db.as_ref() {
                    let _ =
                        db::update_video_status(conn, &recording_id.to_string(), "chunking", None);
                }

                // Read segments from DB (fast sync), then spawn CPU-intensive chunking as async task
                let rec_id_str = recording_id.to_string();
                let conn = self.db.as_ref();
                let video_id =
                    conn.and_then(|c| db::get_video_id_by_recording(c, &rec_id_str).ok().flatten());
                let segments = video_id
                    .and_then(|vid| conn.and_then(|c| db::get_segments_for_video(c, vid).ok()));

                if let (Some(vid), Some(segs)) = (video_id, segments) {
                    let seg_structs: Vec<types::Segment> = segs
                        .iter()
                        .map(|(idx, start, end, text)| types::Segment {
                            index: *idx,
                            start_sec: *start,
                            end_sec: *end,
                            text: text.clone(),
                            speaker: None,
                            confidence: None,
                        })
                        .collect();

                    // Spawn chunking + word frequency computation off the event loop
                    vec![PluginAction::SpawnTask {
                        plugin_name: "crunchr",
                        future: Box::pin(async move {
                            // CPU-intensive work in spawn_blocking
                            let result = tokio::task::spawn_blocking(move || {
                                let chunks = pipeline::chunk_segments(&seg_structs, 512);
                                let all_text: String = chunks
                                    .iter()
                                    .map(|c| c.text.as_str())
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                let freqs = pipeline::word_frequencies(&all_text);
                                let chunk_data: Vec<types::ChunkData> = chunks
                                    .into_iter()
                                    .map(|c| types::ChunkData {
                                        text: c.text,
                                        start_sec: c.start_sec,
                                        end_sec: c.end_sec,
                                        token_count: c.token_count,
                                    })
                                    .collect();
                                (chunk_data, freqs)
                            })
                            .await;

                            match result {
                                Ok((chunks, word_frequencies)) => {
                                    Box::new(PipelineEvent::ChunkingComplete {
                                        recording_id,
                                        video_id: vid,
                                        chunks,
                                        word_frequencies,
                                    }) as Box<dyn Any + Send>
                                }
                                Err(e) => Box::new(PipelineEvent::StageError {
                                    recording_id,
                                    error: format!("Chunking failed: {e}"),
                                }) as Box<dyn Any + Send>,
                            }
                        }),
                    }]
                } else {
                    if let Some(job) = self
                        .queue
                        .iter_mut()
                        .find(|j| j.recording_id == recording_id)
                    {
                        job.state = PipelineState::Failed;
                        job.error = Some("No segments found for chunking".to_string());
                    }
                    self.in_flight.remove(&recording_id);
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Open the config modal, cloning current config into draft.
    fn open_config_modal(&mut self, app: &AppState) {
        // Cache channels for the tandem checklist
        self.cached_channels = app
            .channels
            .iter()
            .map(|ch| {
                let key = format!("{}:{}", ch.platform, ch.id);
                let display = format!("[{}] {}", ch.platform, ch.display_name);
                (key, display)
            })
            .collect();

        // Clone current config into draft
        let mut draft = CrunchrConfig {
            enabled: self.enabled,
            configured: self.configured,
            backend: self.backend.as_ref().map_or_else(
                || "voxtral-openrouter".to_string(),
                |b| b.backend_name().to_string(),
            ),
            api_key_env: None,
            endpoint: None,
            whisper_model: None,
            whisper_timeout_secs: 7200,
            diarize: false,
            embed_subs: true,
            analysis: CrunchrAnalysisConfig::default(),
            tandem_channels: self.tandem_channels.clone(),
            tandem_playlists: self.tandem_playlists.clone(),
        };
        // Try to read current config values from analysis_config
        if let Some(ref ac) = self.analysis_config {
            draft.analysis = ac.clone();
        }
        self.config_draft = Some(draft);

        self.config_modal = ConfigModalState::Active {
            selected_field: 0,
            editing: false,
            static_field_count: CRUNCHR_STATIC_FIELDS,
        };
    }

    /// Handle keys while config modal is active.
    fn handle_config_modal_key(&mut self, key: KeyEvent, _app: &AppState) -> Vec<PluginAction> {
        let ConfigModalState::Active {
            ref mut selected_field,
            ref mut editing,
            static_field_count,
        } = self.config_modal
        else {
            return Vec::new();
        };
        let total_fields = static_field_count + self.cached_channels.len();

        // Indices: 0=enabled, 1=backend, 2=api_key, 3=endpoint, 4=whisper_model,
        //          5=analysis_enabled, 6..6+N=tandem channels, last=[Save]
        // We'll use: 0..static_field_count-1 for static fields, then channels, then Save
        let save_idx = total_fields; // Save button is after all fields

        if *editing {
            match key.code {
                KeyCode::Esc => {
                    *editing = false;
                }
                KeyCode::Enter => {
                    *editing = false;
                }
                KeyCode::Backspace => {
                    if let Some(ref mut draft) = self.config_draft {
                        match *selected_field {
                            2 => {
                                if let Some(s) = draft.api_key_env.as_mut() {
                                    s.pop();
                                }
                            }
                            3 => {
                                if let Some(s) = draft.endpoint.as_mut() {
                                    s.pop();
                                }
                            }
                            4 => {
                                if let Some(s) = draft.whisper_model.as_mut() {
                                    s.pop();
                                }
                            }
                            _ => {}
                        }
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(ref mut draft) = self.config_draft {
                        match *selected_field {
                            2 => draft.api_key_env.get_or_insert_with(String::new).push(c),
                            3 => draft.endpoint.get_or_insert_with(String::new).push(c),
                            4 => draft.whisper_model.get_or_insert_with(String::new).push(c),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            return Vec::new();
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                *selected_field = (*selected_field + 1).min(save_idx);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                *selected_field = selected_field.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if *selected_field == save_idx {
                    // Save: emit UpdateConfig action
                    if let Some(mut draft) = self.config_draft.take() {
                        draft.configured = true;
                        self.enabled = draft.enabled;
                        self.configured = true;
                        self.tandem_channels = draft.tandem_channels.clone();
                        self.tandem_playlists = draft.tandem_playlists.clone();
                        if draft.analysis.enabled {
                            self.analysis_config = Some(draft.analysis.clone());
                        } else {
                            self.analysis_config = None;
                        }
                        self.config_modal = ConfigModalState::Hidden;
                        return vec![PluginAction::UpdateConfig {
                            plugin_name: "crunchr",
                            config_update: Box::new(draft),
                        }];
                    }
                    self.config_modal = ConfigModalState::Hidden;
                } else if *selected_field == 0 {
                    // Toggle enabled
                    if let Some(ref mut draft) = self.config_draft {
                        draft.enabled = !draft.enabled;
                    }
                } else if *selected_field == 1 {
                    // Cycle backend
                    if let Some(ref mut draft) = self.config_draft {
                        draft.backend = match draft.backend.as_str() {
                            "voxtral-openrouter" => "voxtral-api".to_string(),
                            "voxtral-api" => "whisperx-local".to_string(),
                            "whisperx-local" => "voxtral-local".to_string(),
                            "voxtral-local" => "whisper-cli".to_string(),
                            _ => "voxtral-openrouter".to_string(),
                        };
                    }
                } else if *selected_field == 5 {
                    // Toggle analysis enabled
                    if let Some(ref mut draft) = self.config_draft {
                        draft.analysis.enabled = !draft.analysis.enabled;
                    }
                } else if *selected_field >= CRUNCHR_STATIC_FIELDS && *selected_field < save_idx {
                    // Tandem channel toggle
                    let ch_idx = *selected_field - CRUNCHR_STATIC_FIELDS;
                    if let Some(ref mut draft) = self.config_draft {
                        if let Some((key, _)) = self.cached_channels.get(ch_idx) {
                            if draft.tandem_channels.contains(key) {
                                draft.tandem_channels.retain(|k| k != key);
                            } else {
                                draft.tandem_channels.push(key.clone());
                                // Auto-enable when tandem channels are added
                                draft.enabled = true;
                            }
                        }
                    }
                } else if matches!(*selected_field, 2 | 3 | 4) {
                    // Text input fields - enter edit mode
                    *editing = true;
                }
            }
            KeyCode::Esc => {
                self.config_modal = ConfigModalState::Hidden;
                self.config_draft = None;
            }
            _ => {}
        }
        Vec::new()
    }

    /// Open the Speaker Editor modal for the currently-focused search result.
    /// No-op (with a status hint) when nothing usable is selected or when the
    /// transcript has no speaker labels.
    fn open_speaker_modal_for_selected(&mut self) -> Vec<PluginAction> {
        let Some(result) = self.search_results.get(self.selected_result).cloned() else {
            return vec![PluginAction::SetStatus(
                "Speaker editor: no transcript selected".to_string(),
            )];
        };
        let Some(video_path) = result.video_path.clone() else {
            return vec![PluginAction::SetStatus(
                "Speaker editor: selection has no recording path".to_string(),
            )];
        };
        let Some(conn) = self.db.as_ref() else {
            return vec![PluginAction::SetStatus(
                "Speaker editor: database unavailable".to_string(),
            )];
        };

        let Some((video_id, recording_id_str)) =
            db::lookup_video_by_path(conn, &video_path).ok().flatten()
        else {
            return vec![PluginAction::SetStatus(
                "Speaker editor: recording not found in CrunchR database".to_string(),
            )];
        };
        let recording_id = match Uuid::parse_str(&recording_id_str) {
            Ok(u) => u,
            Err(_) => {
                return vec![PluginAction::SetStatus(
                    "Speaker editor: invalid recording id".to_string(),
                )];
            }
        };
        let speakers = match db::load_speakers(conn, video_id) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("crunchr: load_speakers failed: {e}");
                return vec![PluginAction::SetStatus(
                    "Speaker editor: failed to load speakers".to_string(),
                )];
            }
        };
        if speakers.is_empty() {
            return vec![PluginAction::SetStatus(
                "Speaker editor: this recording has no diarized speakers".to_string(),
            )];
        }

        let samples_dir = self
            .data_dir
            .join("voice_samples")
            .join(recording_id.to_string());

        let rows: Vec<SpeakerRow> = speakers
            .into_iter()
            .map(|st| {
                let slug = voice_samples::slugify(&st.speaker);
                let path = samples_dir.join(format!("{slug}.wav"));
                let sample = path.exists().then_some(path);
                SpeakerRow {
                    original_label: st.speaker.clone(),
                    display_label: st.speaker,
                    segment_count: st.segment_count,
                    total_secs: st.total_secs,
                    sample_path: sample,
                }
            })
            .collect();

        self.speaker_modal = SpeakerModalState::Active {
            recording_id,
            video_id,
            video_path: PathBuf::from(video_path),
            rows,
            selected_row: 0,
            editing: false,
        };
        Vec::new()
    }

    /// Key dispatch for the Speaker Editor modal. Mirrors the shape of
    /// `handle_config_modal_key` but with a flat row list rather than a
    /// heterogeneous form.
    fn handle_speaker_modal_key(&mut self, key: KeyEvent) -> Vec<PluginAction> {
        // Pull the active fields out behind an early-return guard so we don't
        // need to repeatedly match the enum in every arm.
        let SpeakerModalState::Active {
            ref mut rows,
            ref mut selected_row,
            ref mut editing,
            video_id,
            ref video_path,
            ..
        } = self.speaker_modal
        else {
            return Vec::new();
        };

        if *editing {
            // Text-edit mode: only letters / digits / space / Backspace / Enter / Esc.
            match key.code {
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(row) = rows.get_mut(*selected_row) {
                        row.display_label.push(c);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(row) = rows.get_mut(*selected_row) {
                        row.display_label.pop();
                    }
                }
                KeyCode::Enter => {
                    *editing = false;
                }
                KeyCode::Esc => {
                    // Revert the in-flight edit on the focused row.
                    if let Some(row) = rows.get_mut(*selected_row) {
                        row.display_label = row.original_label.clone();
                    }
                    *editing = false;
                }
                _ => {}
            }
            return Vec::new();
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if !rows.is_empty() {
                    *selected_row = (*selected_row + 1) % rows.len();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !rows.is_empty() {
                    *selected_row = if *selected_row == 0 {
                        rows.len() - 1
                    } else {
                        *selected_row - 1
                    };
                }
            }
            KeyCode::Enter => {
                *editing = true;
            }
            KeyCode::Char(' ') => {
                if let Some(row) = rows.get(*selected_row) {
                    if let Some(ref sample) = row.sample_path {
                        return vec![PluginAction::PlayFile(sample.clone())];
                    }
                    return vec![PluginAction::SetStatus(
                        "Speaker editor: voice sample not yet sliced".to_string(),
                    )];
                }
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return self.commit_speaker_modal(video_id, video_path.clone());
            }
            KeyCode::Esc => {
                self.speaker_modal = SpeakerModalState::Hidden;
            }
            _ => {}
        }
        Vec::new()
    }

    /// Commit rename edits from the Speaker Editor modal: write to DB,
    /// regenerate sidecars from the persisted segments, optionally re-mux
    /// subs into the .mkv.
    fn commit_speaker_modal(&mut self, video_id: i64, video_path: PathBuf) -> Vec<PluginAction> {
        let rows_snapshot: Vec<(String, String)> = match self.speaker_modal {
            SpeakerModalState::Active { ref rows, .. } => rows
                .iter()
                .filter(|r| r.display_label != r.original_label)
                .map(|r| (r.original_label.clone(), r.display_label.clone()))
                .collect(),
            _ => Vec::new(),
        };

        if rows_snapshot.is_empty() {
            self.speaker_modal = SpeakerModalState::Hidden;
            return vec![PluginAction::SetStatus(
                "Speaker editor: nothing to save".to_string(),
            )];
        }

        let conn = match self.db.as_ref() {
            Some(c) => c,
            None => {
                self.speaker_modal = SpeakerModalState::Hidden;
                return vec![PluginAction::SetStatus(
                    "Speaker editor: database unavailable".to_string(),
                )];
            }
        };

        let mut updated_total = 0usize;
        for (old, new) in &rows_snapshot {
            match db::rewrite_speaker_label(conn, video_id, old, new) {
                Ok(n) => updated_total += n,
                Err(e) => {
                    tracing::warn!(
                        "crunchr: rewrite_speaker_label({old:?} -> {new:?}) failed: {e}"
                    );
                }
            }
        }

        // Regenerate sidecars from the persisted (now-renamed) segments.
        let segments = db::load_full_segments(conn, video_id).unwrap_or_default();
        if let Some(ep_dir) = video_path.parent() {
            let full_text = segments
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if let Err(e) = write_transcript_sidecars(ep_dir, &segments, &full_text) {
                tracing::warn!("crunchr: sidecar rewrite failed: {e}");
            }
            if self.embed_subs {
                mux_subs_into_mkv(&video_path, &ep_dir.join("transcript.vtt"));
            }
        }

        self.speaker_modal = SpeakerModalState::Hidden;
        vec![PluginAction::SetStatus(format!(
            "Speaker labels updated ({} segments rewritten)",
            updated_total
        ))]
    }

    /// Handle keys in the Search view (original behavior).
    fn handle_search_key(&mut self, key: KeyEvent, app: &AppState) -> Vec<PluginAction> {
        if self.input_active {
            match key.code {
                KeyCode::Esc => {
                    self.input_active = false;
                }
                KeyCode::Enter => {
                    self.input_active = false;
                    self.execute_search();
                }
                KeyCode::Backspace => {
                    self.search_query.pop();
                }
                KeyCode::Char(c) => {
                    self.search_query.push(c);
                }
                _ => {}
            }
            return Vec::new();
        }

        match key.code {
            KeyCode::Char('/') => {
                self.input_active = true;
            }
            KeyCode::Char('c') => {
                self.open_config_modal(app);
            }
            KeyCode::Tab => {
                self.view = CrunchrView::Queue;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.search_results.is_empty() {
                    self.selected_result = (self.selected_result + 1) % self.search_results.len();
                    self.refresh_selected_analysis();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.search_results.is_empty() {
                    self.selected_result = if self.selected_result == 0 {
                        self.search_results.len() - 1
                    } else {
                        self.selected_result - 1
                    };
                    self.refresh_selected_analysis();
                }
            }
            KeyCode::Enter => {
                if let Some(result) = self.search_results.get(self.selected_result) {
                    if let Some(ref path) = result.video_path {
                        // M5.2 — Enter on a chunk seeks mpv to start_sec
                        // so the transcript hit lands at the right point
                        // in the recording. PluginAction::PlayFile takes
                        // a plain PathBuf today; PlayFileAt extends it.
                        return vec![PluginAction::PlayFileAt(
                            PathBuf::from(path),
                            result.start_sec,
                        )];
                    }
                }
            }
            KeyCode::Char('x') => {
                // M5.1 — clip export. ffmpeg -ss start -to end -c copy
                // <recording> clips/<channel>-<HHMMSS>.mkv. Spawned as
                // a SpawnTask future; ClipExportComplete returns.
                if let Some(result) = self.search_results.get(self.selected_result).cloned() {
                    if let Some(ref path) = result.video_path {
                        let source = PathBuf::from(path);
                        let clips_dir = crate::dirs::clips_dir();
                        let start = result.start_sec.max(0.0);
                        let end = result.end_sec.max(start + 1.0);
                        let channel_slug = sanitize_for_filename(&result.channel_name);
                        let hhmmss = format_timestamp(start);
                        let stem = format!("{channel_slug}-{hhmmss}");
                        return vec![PluginAction::SpawnTask {
                            plugin_name: "crunchr",
                            future: Box::pin(async move {
                                let res = export_clip(&source, start, end, &clips_dir, &stem).await;
                                let event = match res {
                                    Ok(path) => {
                                        PipelineEvent::ClipExportComplete { path, error: None }
                                    }
                                    Err(e) => PipelineEvent::ClipExportComplete {
                                        path: clips_dir.join(format!("{stem}.mkv")),
                                        error: Some(e.to_string()),
                                    },
                                };
                                Box::new(event) as Box<dyn std::any::Any + Send>
                            }),
                        }];
                    }
                }
            }
            KeyCode::Esc => {
                return vec![PluginAction::NavigateBack];
            }
            _ => {}
        }
        Vec::new()
    }

    /// Handle keys in the Queue view.
    fn handle_queue_key(&mut self, key: KeyEvent) -> Vec<PluginAction> {
        match key.code {
            KeyCode::Tab => {
                self.view = CrunchrView::RecordingPicker;
            }
            KeyCode::Esc => {
                self.view = CrunchrView::Search;
            }
            _ => {}
        }
        Vec::new()
    }

    /// Handle keys in the RecordingPicker view.
    fn handle_picker_key(&mut self, key: KeyEvent, app: &AppState) -> Vec<PluginAction> {
        // Refresh visible recordings list
        self.refresh_picker_list(app);

        match key.code {
            KeyCode::Tab => {
                self.view = CrunchrView::Search;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.picker.visible_ids.is_empty() {
                    self.picker.selected =
                        (self.picker.selected + 1) % self.picker.visible_ids.len();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.picker.visible_ids.is_empty() {
                    self.picker.selected = if self.picker.selected == 0 {
                        self.picker.visible_ids.len() - 1
                    } else {
                        self.picker.selected - 1
                    };
                }
            }
            KeyCode::Char(' ') => {
                // Toggle multi-select
                if let Some(&id) = self.picker.visible_ids.get(self.picker.selected) {
                    if !self.picker.selections.remove(&id) {
                        self.picker.selections.insert(id);
                    }
                }
            }
            KeyCode::Char('a') => {
                // Select all visible
                for &id in &self.picker.visible_ids {
                    self.picker.selections.insert(id);
                }
            }
            KeyCode::Char('f') => {
                // Cycle filter
                self.cycle_picker_filter(app);
                self.refresh_picker_list(app);
            }
            KeyCode::Enter => {
                return self.process_selected_recordings(app);
            }
            KeyCode::Esc => {
                self.picker.selections.clear();
                self.view = CrunchrView::Search;
            }
            _ => {}
        }
        Vec::new()
    }

    /// Refresh the recording picker's visible list based on the current filter.
    fn refresh_picker_list(&mut self, app: &AppState) {
        let finished: Vec<_> = app
            .recordings
            .values()
            .filter(|r| r.state == RecordingState::Finished)
            .filter(|r| !self.in_flight.contains(&r.id))
            .filter(|r| match &self.picker.filter {
                RecordingFilter::All => true,
                RecordingFilter::ByChannel(ch) => {
                    let key = format!("{}:{}", r.platform, r.channel_id);
                    key == *ch
                }
                RecordingFilter::ByPlaylist(pl) => r.playlist.as_deref() == Some(pl.as_str()),
            })
            .collect();

        self.picker.visible_ids = finished.iter().map(|r| r.id).collect();
        if self.picker.selected >= self.picker.visible_ids.len() {
            self.picker.selected = self.picker.visible_ids.len().saturating_sub(1);
        }
    }

    /// Cycle through recording picker filters.
    fn cycle_picker_filter(&mut self, app: &AppState) {
        // Collect unique channels from finished recordings
        let channels: Vec<String> = {
            let mut seen = HashSet::new();
            app.recordings
                .values()
                .filter(|r| r.state == RecordingState::Finished)
                .filter_map(|r| {
                    let key = format!("{}:{}", r.platform, r.channel_id);
                    if seen.insert(key.clone()) {
                        Some(key)
                    } else {
                        None
                    }
                })
                .collect()
        };

        self.picker.filter = match &self.picker.filter {
            RecordingFilter::All => {
                if let Some(ch) = channels.first() {
                    RecordingFilter::ByChannel(ch.clone())
                } else {
                    RecordingFilter::All
                }
            }
            RecordingFilter::ByChannel(current) => {
                let idx = channels.iter().position(|c| c == current).unwrap_or(0);
                if idx + 1 < channels.len() {
                    RecordingFilter::ByChannel(channels[idx + 1].clone())
                } else {
                    RecordingFilter::All
                }
            }
            RecordingFilter::ByPlaylist(_) => RecordingFilter::All,
        };
        self.picker.selections.clear();
    }

    /// Process selected (or focused) recordings from the picker.
    fn process_selected_recordings(&mut self, app: &AppState) -> Vec<PluginAction> {
        let ids: Vec<Uuid> = if self.picker.selections.is_empty() {
            // Process just the focused recording
            self.picker
                .visible_ids
                .get(self.picker.selected)
                .copied()
                .into_iter()
                .collect()
        } else {
            self.picker.selections.drain().collect()
        };

        let mut actions = Vec::new();
        for id in ids {
            if let Some(rec) = app.recordings.get(&id) {
                let mut batch = self.queue_recording(
                    id,
                    rec.channel_name.clone(),
                    rec.stream_title
                        .clone()
                        .unwrap_or_else(|| "Untitled".to_string()),
                    rec.output_path.clone(),
                );
                actions.append(&mut batch);
            }
        }
        if !actions.is_empty() {
            self.view = CrunchrView::Queue;
        }
        actions
    }

    fn handle_pipeline_event(&mut self, event: PipelineEvent) -> Vec<PluginAction> {
        match event {
            PipelineEvent::AudioExtracted {
                recording_id,
                audio_path,
            } => {
                if let Some(job) = self
                    .queue
                    .iter_mut()
                    .find(|j| j.recording_id == recording_id)
                {
                    job.audio_path = Some(audio_path.clone());
                }
                if let Some(conn) = self.db.as_ref() {
                    let _ = db::update_video_audio_path(
                        conn,
                        &recording_id.to_string(),
                        &audio_path.to_string_lossy(),
                    );
                }
                self.start_next_stage(recording_id)
            }
            PipelineEvent::TranscriptionComplete {
                recording_id,
                segments,
                full_text,
            } => {
                if let Some(conn) = self.db.as_ref() {
                    let rec_id_str = recording_id.to_string();
                    let _ = db::update_video_transcript(conn, &rec_id_str, &full_text);

                    if let Ok(Some(video_id)) = db::get_video_id_by_recording(conn, &rec_id_str) {
                        let seg_data: Vec<(usize, f64, f64, &str, Option<&str>, Option<f64>)> =
                            segments
                                .iter()
                                .map(|s| {
                                    (
                                        s.index,
                                        s.start_sec,
                                        s.end_sec,
                                        s.text.as_str(),
                                        s.speaker.as_deref(),
                                        s.confidence,
                                    )
                                })
                                .collect();
                        let _ = db::insert_segments(conn, video_id, &seg_data);
                    }
                }

                // Sidecar artifacts: write transcript.json, transcript.vtt, and
                // diarization.json into the episode dir alongside video.mkv so
                // downstream tooling doesn't need to read SQLite.
                if let Some(job) = self.queue.iter().find(|j| j.recording_id == recording_id) {
                    let video_path = job.video_path.clone();
                    let audio_path = job.audio_path.clone();
                    let embed = self.embed_subs;
                    if let Some(ep_dir) = video_path.parent() {
                        let _ = write_transcript_sidecars(ep_dir, &segments, &full_text);
                        if embed {
                            mux_subs_into_mkv(&video_path, &ep_dir.join("transcript.vtt"));
                        }
                    }
                    // Cache voice samples for the Speaker Editor modal (only
                    // meaningful when diarization actually produced speakers).
                    if segments.iter().any(|s| s.speaker.is_some()) {
                        let samples_dir = self
                            .data_dir
                            .join("voice_samples")
                            .join(recording_id.to_string());
                        let segs_for_slice = segments.clone();
                        let video_for_slice = video_path.clone();
                        tokio::spawn(async move {
                            if let Err(e) = voice_samples::slice_samples(
                                &video_for_slice,
                                &segs_for_slice,
                                &samples_dir,
                            )
                            .await
                            {
                                tracing::warn!("crunchr: voice_samples slice failed: {e}");
                            }
                        });
                    }
                    // Clean up WAV file
                    if let Some(ref ap) = audio_path {
                        if let Err(e) = std::fs::remove_file(ap) {
                            tracing::debug!("Failed to clean up WAV: {e}");
                        }
                    }
                }

                self.start_next_stage(recording_id)
            }
            PipelineEvent::ChunkingComplete {
                recording_id,
                video_id,
                chunks,
                word_frequencies,
            } => {
                // Write chunk + word frequency results to DB (fast sync writes)
                if let Some(conn) = self.db.as_ref() {
                    let chunk_tuples: Vec<(usize, &str, f64, f64, usize)> = chunks
                        .iter()
                        .enumerate()
                        .map(|(i, c)| (i, c.text.as_str(), c.start_sec, c.end_sec, c.token_count))
                        .collect();
                    let _ = db::insert_chunks(conn, video_id, &chunk_tuples);
                    let _ = db::insert_word_frequencies(conn, video_id, &word_frequencies);
                }

                let rec_id_str = recording_id.to_string();

                // If analysis is enabled, start it
                if let Some(ref analysis_cfg) = self.analysis_config {
                    if analysis_cfg.enabled {
                        if let Some(conn) = self.db.as_ref() {
                            let _ = db::update_video_status(conn, &rec_id_str, "analyzing", None);
                        }
                        if let Some(job) = self
                            .queue
                            .iter_mut()
                            .find(|j| j.recording_id == recording_id)
                        {
                            job.state = PipelineState::Analyzing;
                        }

                        let transcript =
                            self.db
                                .as_ref()
                                .and_then(|c| {
                                    c.query_row(
                                    "SELECT transcript_text FROM videos WHERE recording_id = ?1",
                                    [&rec_id_str],
                                    |row| row.get::<_, Option<String>>(0),
                                ).ok().flatten()
                                })
                                .unwrap_or_default();

                        let channel_name = self
                            .queue
                            .iter()
                            .find(|j| j.recording_id == recording_id)
                            .map(|j| j.channel_name.clone())
                            .unwrap_or_default();
                        let title = self
                            .queue
                            .iter()
                            .find(|j| j.recording_id == recording_id)
                            .map(|j| j.title.clone())
                            .unwrap_or_default();

                        let cfg = analysis_cfg.clone();
                        self.refresh_word_frequencies();
                        return vec![PluginAction::SpawnTask {
                            plugin_name: "crunchr",
                            future: Box::pin(async move {
                                match analysis::analyze_transcript(
                                    &cfg,
                                    &channel_name,
                                    &title,
                                    &transcript,
                                )
                                .await
                                {
                                    Ok(result) => {
                                        let topics_json = serde_json::to_string(&result.topics)
                                            .unwrap_or_default();
                                        Box::new(PipelineEvent::AnalysisComplete {
                                            recording_id,
                                            summary: result.summary,
                                            topics: topics_json,
                                            sentiment: result.sentiment,
                                            prompt_tokens: result.prompt_tokens,
                                            completion_tokens: result.completion_tokens,
                                            cost_cents: result.cost_cents,
                                        })
                                            as Box<dyn Any + Send>
                                    }
                                    Err(e) => {
                                        tracing::warn!("Analysis failed (non-fatal): {e}");
                                        Box::new(PipelineEvent::AnalysisComplete {
                                            recording_id,
                                            summary: String::new(),
                                            topics: "[]".to_string(),
                                            sentiment: "unknown".to_string(),
                                            prompt_tokens: 0,
                                            completion_tokens: 0,
                                            cost_cents: 0,
                                        })
                                            as Box<dyn Any + Send>
                                    }
                                }
                            }),
                        }];
                    }
                }

                // No analysis, mark complete
                if let Some(conn) = self.db.as_ref() {
                    let _ = db::update_video_status(conn, &rec_id_str, "complete", None);
                }
                if let Some(job) = self
                    .queue
                    .iter_mut()
                    .find(|j| j.recording_id == recording_id)
                {
                    job.state = PipelineState::Complete;
                }
                self.in_flight.remove(&recording_id);
                self.refresh_word_frequencies();
                Vec::new()
            }
            PipelineEvent::AnalysisComplete {
                recording_id,
                summary,
                topics,
                sentiment,
                prompt_tokens,
                completion_tokens,
                cost_cents,
            } => {
                // Store analysis results in DB
                if let Some(conn) = self.db.as_ref() {
                    let _ = conn.execute(
                        "INSERT OR REPLACE INTO video_analysis (video_id, summary, topics, sentiment) \
                         SELECT id, ?1, ?2, ?3 FROM videos WHERE recording_id = ?4",
                        rusqlite::params![summary, topics, sentiment, recording_id.to_string()],
                    );
                    // M5.6: persist token usage + cost. INSERT-OR-REPLACE
                    // so re-analyses overwrite without piling up rows.
                    let _ = conn.execute(
                        "UPDATE videos SET prompt_tokens = ?1, completion_tokens = ?2, cost_cents = ?3 \
                         WHERE recording_id = ?4",
                        rusqlite::params![
                            prompt_tokens as i64,
                            completion_tokens as i64,
                            cost_cents as i64,
                            recording_id.to_string(),
                        ],
                    );
                    let _ =
                        db::update_video_status(conn, &recording_id.to_string(), "complete", None);
                }
                if let Some(job) = self
                    .queue
                    .iter_mut()
                    .find(|j| j.recording_id == recording_id)
                {
                    job.state = PipelineState::Complete;
                }
                self.in_flight.remove(&recording_id);
                self.refresh_word_frequencies();
                Vec::new()
            }
            PipelineEvent::StageError {
                recording_id,
                error,
            } => {
                if let Some(job) = self
                    .queue
                    .iter_mut()
                    .find(|j| j.recording_id == recording_id)
                {
                    job.state = PipelineState::Failed;
                    job.error = Some(error.clone());
                }
                if let Some(conn) = self.db.as_ref() {
                    let _ = db::update_video_status(
                        conn,
                        &recording_id.to_string(),
                        "failed",
                        Some(&error),
                    );
                }
                self.in_flight.remove(&recording_id);
                self.last_error = Some(error.clone());
                vec![PluginAction::SetStatus(format!("CrunchR error: {error}"))]
            }
            PipelineEvent::ClipExportComplete { path, error } => match error {
                None => {
                    let label = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    vec![PluginAction::SetStatus(format!("Clip exported: {label}"))]
                }
                Some(e) => {
                    self.last_error = Some(e.clone());
                    vec![PluginAction::SetStatus(format!("Clip export failed: {e}"))]
                }
            },
        }
    }
}

impl Plugin for CrunchrPlugin {
    fn name(&self) -> &'static str {
        "crunchr"
    }

    fn display_name(&self) -> &str {
        "CrunchR Intelligence"
    }

    fn init(&mut self, ctx: &PluginContext) -> anyhow::Result<()> {
        self.data_dir = ctx.data_dir.clone();

        std::fs::create_dir_all(&ctx.data_dir)?;
        std::fs::create_dir_all(&ctx.cache_dir)?;

        // Migrate old DB name if needed
        let old_db = ctx.data_dir.join("sloptube.db");
        let new_db = ctx.data_dir.join("crunchr.db");
        if old_db.exists() && !new_db.exists() {
            let _ = std::fs::rename(&old_db, &new_db);
        }

        // Open persistent DB connection
        let db_path = ctx.data_dir.join("crunchr.db");
        self.db = Some(db::open_and_init(&db_path)?);

        // Create transcription backend from config
        let crunchr_config = &ctx.config.crunchr;
        let backend = transcribe::create_backend(crunchr_config);
        let backend_name = backend.backend_name();

        // Check if the backend is actually usable. API backends are always
        // "available" (network/key issues surface at runtime); whisper-cli
        // requires the subprocess to be on PATH.
        self.backend_available = match backend_name {
            "whisper-cli" => pipeline::is_whisper_available(),
            "whisperx-local" => transcribe::whisperx_local::is_available(),
            "voxtral-openrouter" | "voxtral-api" | "voxtral-local" => true,
            _ => false,
        };

        if !self.backend_available && backend_name == "whisper-cli" {
            tracing::info!("CrunchR: whisper CLI not found, transcription disabled");
        }

        self.backend = Some(Arc::from(backend));

        // Cache the configured env-var name so try_init_diarize_backend can
        // honour `crunchr.api_key_env` instead of always hitting MISTRAL_API_KEY.
        self.diarize_api_key_env = crunchr_config.api_key_env.clone();
        self.embed_subs = crunchr_config.embed_subs;
        self.diarize_requested = crunchr_config.diarize;

        // Analysis config
        let analysis = &crunchr_config.analysis;
        if analysis.enabled {
            self.analysis_config = Some(analysis.clone());
            tracing::info!("CrunchR: analysis enabled (model: {})", analysis.model);
        }

        // Load tandem / enabled state from config
        self.enabled = crunchr_config.enabled;
        self.configured = crunchr_config.configured;
        self.tandem_channels = crunchr_config.tandem_channels.clone();
        self.tandem_playlists = crunchr_config.tandem_playlists.clone();

        // Load initial word frequencies
        self.refresh_word_frequencies();

        tracing::info!("CrunchR plugin initialized (backend: {backend_name}, enabled: {}, configured: {}, tandem_channels: {}, db: {})",
            self.enabled, self.configured, self.tandem_channels.len(), db_path.display());
        Ok(())
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        self.db.take();
        self.backend.take();
        tracing::info!("CrunchR plugin shutting down");
        Ok(())
    }

    fn event_filter(&self) -> Option<Vec<DaemonEventKind>> {
        Some(vec![DaemonEventKind::RecordingFinished])
    }

    fn on_event(&mut self, event: &DaemonEvent, app: &AppState) -> Vec<PluginAction> {
        if let DaemonEvent::RecordingFinished {
            job_id,
            final_state,
            ..
        } = event
        {
            if *final_state != RecordingState::Finished {
                return Vec::new();
            }

            // Only auto-trigger if enabled AND channel/playlist matches tandem config
            if !self.enabled {
                return Vec::new();
            }

            if let Some(rec) = app.recordings.get(job_id) {
                let channel_key = format!("{}:{}", rec.platform, rec.channel_id);
                let is_tandem = self.tandem_channels.contains(&channel_key)
                    || rec
                        .playlist
                        .as_ref()
                        .is_some_and(|p| self.tandem_playlists.contains(p));

                // Catalog-pull jobs drop a `.crunchr-auto` marker in the episode dir
                // so a one-shot `strivo pull` transcribes without editing tandem config.
                let crunchr_auto_marker = rec
                    .output_path
                    .parent()
                    .map(|p| p.join(".crunchr-auto"))
                    .map(|m| m.exists())
                    .unwrap_or(false);

                if is_tandem || crunchr_auto_marker {
                    let video_path = rec.output_path.clone();
                    let channel_name = rec.channel_name.clone();
                    let title = rec
                        .stream_title
                        .clone()
                        .unwrap_or_else(|| "Untitled".to_string());
                    return self.queue_recording(*job_id, channel_name, title, video_path);
                }
            }
        }
        Vec::new()
    }

    fn on_key(&mut self, key: KeyEvent, app: &AppState) -> Vec<PluginAction> {
        // --- First-run: auto-open config modal if not configured ---
        if !self.configured && self.config_modal == ConfigModalState::Hidden {
            self.open_config_modal(app);
        }

        // --- Config modal intercepts all keys when active ---
        if self.config_modal != ConfigModalState::Hidden {
            return self.handle_config_modal_key(key, app);
        }

        // --- Speaker Editor modal intercepts all keys when active ---
        if self.speaker_modal.is_active() {
            return self.handle_speaker_modal_key(key);
        }

        // --- Pane-level keybindings (apply across views) ---
        // `s` — open Speaker Editor for the focused/most-recent diarized
        // transcript. No-op when no diarized recording is available.
        if matches!(key.code, KeyCode::Char('s')) && key.modifiers.is_empty() {
            return self.open_speaker_modal_for_selected();
        }

        // --- View-specific key handling ---
        match self.view {
            CrunchrView::Search => self.handle_search_key(key, app),
            CrunchrView::Queue => self.handle_queue_key(key),
            CrunchrView::RecordingPicker => self.handle_picker_key(key, app),
        }
    }

    fn on_plugin_event(&mut self, event: Box<dyn Any + Send>) -> Vec<PluginAction> {
        if let Ok(pipeline_event) = event.downcast::<PipelineEvent>() {
            return self.handle_pipeline_event(*pipeline_event);
        }
        Vec::new()
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            name: "Intelligence",
            description: "CrunchR transcript search",
            key: KeyCode::Char('I'),
            modifiers: KeyModifiers::SHIFT,
        }]
    }

    fn panes(&self) -> Vec<PaneId> {
        vec![PANE_ID]
    }

    fn render_pane(&self, _pane_id: PaneId, frame: &mut Frame, area: Rect, app: &AppState) {
        // Render the active view
        match self.view {
            CrunchrView::Search => render::render(self, frame, area, app),
            CrunchrView::Queue => render::render_queue(self, frame, area),
            CrunchrView::RecordingPicker => render::render_recording_picker(self, frame, area, app),
        }

        // Overlay config modal if active
        if self.config_modal != ConfigModalState::Hidden {
            render::render_config_modal(self, frame, area);
        }

        // Overlay Speaker Editor modal if active (drawn last so it sits on top).
        if self.speaker_modal.is_active() {
            render::render_speaker_modal(self, frame, area);
        }
    }

    fn status_line(&self, _app: &AppState) -> Option<String> {
        let pending = self
            .queue
            .iter()
            .filter(|j| j.state != PipelineState::Complete && j.state != PipelineState::Failed)
            .count();

        if pending > 0 {
            Some(format!("CR:{pending}"))
        } else {
            None
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn properties_section(
        &self,
        job_id: uuid::Uuid,
        _app: &AppState,
    ) -> Vec<ratatui::text::Line<'static>> {
        use ratatui::style::{Modifier, Style};
        use ratatui::text::{Line, Span};
        use strivo_core::tui::theme::Theme;

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  Transcript",
            Style::new()
                .fg(Theme::secondary())
                .add_modifier(Modifier::BOLD),
        ));

        let Some(info) = self.recording_info(&job_id.to_string()) else {
            lines.push(Line::styled(
                "  Not processed",
                Style::new().fg(Theme::muted()),
            ));
            return lines;
        };

        let status_color = if info.status == "complete" {
            Theme::green()
        } else {
            Theme::fg()
        };
        lines.push(Line::from(vec![
            Span::styled("  Status:   ", Style::new().fg(Theme::dim())),
            Span::styled(info.status.clone(), Style::new().fg(status_color)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Segments: ", Style::new().fg(Theme::dim())),
            Span::styled(info.segment_count.to_string(), Style::new().fg(Theme::fg())),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Words:    ", Style::new().fg(Theme::dim())),
            Span::styled(info.word_count.to_string(), Style::new().fg(Theme::fg())),
        ]));

        if info.has_analysis {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "  Analysis",
                Style::new()
                    .fg(Theme::secondary())
                    .add_modifier(Modifier::BOLD),
            ));
            if let Some(ref summary) = info.summary {
                lines.push(Line::styled(
                    format!("  {summary}"),
                    Style::new().fg(Theme::fg()),
                ));
            }
            if !info.topics.is_empty() {
                let topics_str = info.topics.join(", ");
                lines.push(Line::from(vec![
                    Span::styled("  Topics:   ", Style::new().fg(Theme::dim())),
                    Span::styled(topics_str, Style::new().fg(Theme::primary())),
                ]));
            }
            if let Some(ref sentiment) = info.sentiment {
                let color = match sentiment.as_str() {
                    "positive" => Theme::green(),
                    "negative" => Theme::red(),
                    _ => Theme::muted(),
                };
                lines.push(Line::from(vec![
                    Span::styled("  Sentiment:", Style::new().fg(Theme::dim())),
                    Span::styled(format!(" {sentiment}"), Style::new().fg(color)),
                ]));
            }
            // M5.6 — token usage + estimated cost. Surfaced only when
            // analyzed because tokens are 0 otherwise; "$0.00" is
            // technically correct but adds noise.
            if info.prompt_tokens > 0 || info.completion_tokens > 0 {
                lines.push(Line::from(vec![
                    Span::styled("  Tokens:   ", Style::new().fg(Theme::dim())),
                    Span::styled(
                        format!("{} in · {} out", info.prompt_tokens, info.completion_tokens),
                        Style::new().fg(Theme::fg()),
                    ),
                ]));
            }
            if info.cost_cents > 0 {
                lines.push(Line::from(vec![
                    Span::styled("  Cost:     ", Style::new().fg(Theme::dim())),
                    Span::styled(
                        cost::format_cents(info.cost_cents),
                        Style::new().fg(Theme::green()),
                    ),
                ]));
            }
        }

        lines
    }
}

/// Emit `transcript.json`, `transcript.vtt`, and `diarization.json` (when speakers
/// are present) into the recording's parent directory. Best-effort — errors are
/// logged but never propagated, since the SQLite write is the canonical record.
fn write_transcript_sidecars(
    episode_dir: &std::path::Path,
    segments: &[types::Segment],
    full_text: &str,
) -> std::io::Result<()> {
    if !episode_dir.exists() {
        std::fs::create_dir_all(episode_dir)?;
    }
    // transcript.json — full structured form
    let json = serde_json::json!({
        "full_text": full_text,
        "segments": segments.iter().map(|s| serde_json::json!({
            "index": s.index,
            "start_sec": s.start_sec,
            "end_sec": s.end_sec,
            "text": s.text,
            "speaker": s.speaker,
            "confidence": s.confidence,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(
        episode_dir.join("transcript.json"),
        serde_json::to_string_pretty(&json).unwrap_or_default(),
    )?;

    // transcript.vtt — speaker-labeled cues for any video player
    let mut vtt = String::from("WEBVTT\n\n");
    for s in segments {
        let start = vtt_timestamp(s.start_sec);
        let end = vtt_timestamp(s.end_sec);
        vtt.push_str(&format!("{start} --> {end}\n"));
        if let Some(spk) = s.speaker.as_deref().filter(|s| !s.is_empty()) {
            vtt.push_str(&format!("<v {spk}>{}\n\n", s.text));
        } else {
            vtt.push_str(&format!("{}\n\n", s.text));
        }
    }
    std::fs::write(episode_dir.join("transcript.vtt"), vtt)?;

    // transcript.srt — same cues as VTT but with `,` ms separator and inline
    // `[Speaker] text` labels (SRT has no native speaker cue).
    let mut srt = String::new();
    for (i, s) in segments.iter().enumerate() {
        let start = srt_timestamp(s.start_sec);
        let end = srt_timestamp(s.end_sec);
        srt.push_str(&format!("{}\n{start} --> {end}\n", i + 1));
        match s.speaker.as_deref().filter(|s| !s.is_empty()) {
            Some(spk) => srt.push_str(&format!("[{spk}] {}\n\n", s.text)),
            None => srt.push_str(&format!("{}\n\n", s.text)),
        }
    }
    std::fs::write(episode_dir.join("transcript.srt"), srt)?;

    // diarization.json — only if at least one segment carries a speaker label
    if segments.iter().any(|s| s.speaker.is_some()) {
        let dz = serde_json::json!(segments
            .iter()
            .filter_map(|s| s.speaker.as_ref().map(|spk| {
                serde_json::json!({
                    "start": s.start_sec,
                    "end": s.end_sec,
                    "speaker": spk,
                })
            }))
            .collect::<Vec<_>>());
        std::fs::write(
            episode_dir.join("diarization.json"),
            serde_json::to_string_pretty(&dz).unwrap_or_default(),
        )?;
    }

    Ok(())
}

fn vtt_timestamp(secs: f64) -> String {
    let total_ms = (secs * 1000.0) as u64;
    let h = total_ms / 3_600_000;
    let m = (total_ms / 60_000) % 60;
    let s = (total_ms / 1000) % 60;
    let ms = total_ms % 1000;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

fn srt_timestamp(secs: f64) -> String {
    // SRT uses `,` instead of `.` as the millisecond separator. Other than that
    // it is identical to the VTT format.
    vtt_timestamp(secs).replace('.', ",")
}

/// Soft-mux the generated `.vtt` subtitles into the recording's `.mkv` via
/// `mkvmerge`. Writes to a sibling `.tmp.mkv` and atomically renames over the
/// original on success. Silently skips when `mkvmerge` is missing on PATH or
/// when the inputs don't exist — this is a best-effort decoration of the
/// recording, never a hard requirement.
fn mux_subs_into_mkv(video_path: &std::path::Path, vtt_path: &std::path::Path) {
    if which::which("mkvmerge").is_err() {
        tracing::debug!("crunchr: mkvmerge not on PATH; skipping subtitle mux");
        return;
    }
    if !video_path.exists() || !vtt_path.exists() {
        tracing::debug!(
            "crunchr: mux skipped, missing {} or {}",
            video_path.display(),
            vtt_path.display()
        );
        return;
    }
    let tmp = video_path.with_extension("mkv.tmp");
    let status = std::process::Command::new("mkvmerge")
        .arg("-o")
        .arg(&tmp)
        .arg(video_path)
        .arg("--language")
        .arg("0:eng")
        .arg("--track-name")
        .arg("0:CrunchR transcript")
        .arg(vtt_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {
            if let Err(e) = std::fs::rename(&tmp, video_path) {
                tracing::warn!(
                    "crunchr: mkvmerge succeeded but rename failed: {e} ({} -> {})",
                    tmp.display(),
                    video_path.display()
                );
                let _ = std::fs::remove_file(&tmp);
            } else {
                tracing::info!("crunchr: muxed subtitles into {}", video_path.display());
            }
        }
        Ok(s) => {
            tracing::warn!("crunchr: mkvmerge exited with {s}");
            let _ = std::fs::remove_file(&tmp);
        }
        Err(e) => {
            tracing::warn!("crunchr: failed to spawn mkvmerge: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crunchr::types::Segment;

    fn seg(idx: usize, start: f64, end: f64, text: &str, speaker: Option<&str>) -> Segment {
        Segment {
            index: idx,
            start_sec: start,
            end_sec: end,
            text: text.to_string(),
            speaker: speaker.map(|s| s.to_string()),
            confidence: None,
        }
    }

    #[test]
    fn srt_timestamp_uses_comma_separator() {
        assert_eq!(srt_timestamp(0.0), "00:00:00,000");
        assert_eq!(srt_timestamp(3661.123), "01:01:01,123");
    }

    #[test]
    fn vtt_timestamp_uses_dot_separator() {
        assert_eq!(vtt_timestamp(0.0), "00:00:00.000");
        assert_eq!(vtt_timestamp(3661.123), "01:01:01.123");
    }

    #[test]
    fn sidecar_writer_emits_all_artifacts_and_srt_format() {
        let tmp = tempfile_dir("sidecar");
        let segs = vec![
            seg(0, 0.0, 1.5, "hello world", Some("Speaker 0")),
            seg(1, 1.5, 3.0, "no speaker line", None),
        ];
        write_transcript_sidecars(&tmp, &segs, "hello world no speaker line").unwrap();

        // All three primary sidecars exist.
        assert!(tmp.join("transcript.json").exists());
        assert!(tmp.join("transcript.vtt").exists());
        assert!(tmp.join("transcript.srt").exists());
        assert!(tmp.join("diarization.json").exists());

        let srt = std::fs::read_to_string(tmp.join("transcript.srt")).unwrap();
        // SRT cue indexing is 1-based and the comma separator is used.
        assert!(srt.starts_with("1\n00:00:00,000 --> 00:00:01,500\n"));
        assert!(srt.contains("[Speaker 0] hello world"));
        // Unspeakered line carries no `[label]` prefix.
        assert!(srt.contains("\nno speaker line\n"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sidecar_writer_skips_diarization_when_no_speakers() {
        let tmp = tempfile_dir("nodia");
        let segs = vec![seg(0, 0.0, 1.0, "alone", None)];
        write_transcript_sidecars(&tmp, &segs, "alone").unwrap();
        assert!(!tmp.join("diarization.json").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn tempfile_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("strivo-crunchr-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
