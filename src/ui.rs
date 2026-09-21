use std::collections::{HashSet, VecDeque};
use std::io::{self, stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::{
    Alignment, Constraint, CrosstermBackend, Direction, Layout, Rect, Style, Stylize, Terminal,
};
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph};

use crate::analysis::{analyze_buffer, AnalysisStore, TrackAnalysis};
use crate::api;
use crate::browse;
use crate::cava::CavaFeed;
use crate::control::ControlCmd;
use crate::cover;
use crate::decode::AudioBuf;
use crate::dsp::MixMode;
use crate::engine::{load_track, Player, Snapshot};
use crate::eq::Equalizer;
use crate::fetch::{self, looks_like_media_url};
use crate::library::{scan_library, sort_indices, track_from_path, Track};
use crate::meta::{
    apply_peek, load_meta, lyric_fill, lyric_progress, lyric_window, needs_fetch, peek_tags,
    TrackMeta,
};
use crate::mpris::{MediaCmd, Mpris};
use crate::prefs::{Palette, Prefs, ShuffleMode};
use crate::progress::{self, TransferBar};
use crate::remix::{apply_remix, RemixMode};
use crate::session_store::SessionState;
use crate::shell::{format_listen_time, Tab};
use crate::splayer;
use crate::taste::TasteStore;

struct DecodeJob {
    index: usize,
    transition: bool,
    skip: bool,
    buf: AudioBuf,
    analysis: TrackAnalysis,
    path: PathBuf,
}

struct PeekJob {
    index: usize,
    meta: TrackMeta,
}

struct RemoteMetaJob {
    index: usize,
    lyrics: Option<String>,
    cover: Option<Vec<u8>>,
    artist: Option<String>,
    title: Option<String>,
    album: Option<String>,
    path: PathBuf,
}

pub struct App {
    tracks: Vec<Track>,
    list_state: ListState,
    player: Player,
    status: String,
    decoding: bool,
    prefetch_attempt: Option<usize>,
    analysis: AnalysisStore,
    decode_rx: Receiver<Result<DecodeJob, (usize, String)>>,
    decode_tx: std::sync::mpsc::Sender<Result<DecodeJob, (usize, String)>>,
    inflight: Option<usize>,
    metas: Vec<Option<TrackMeta>>,
    peek_rx: Receiver<PeekJob>,
    peek_tx: std::sync::mpsc::Sender<PeekJob>,
    remote_rx: Receiver<RemoteMetaJob>,
    remote_tx: std::sync::mpsc::Sender<RemoteMetaJob>,
    mpris: Mpris,
    tick: u64,
    command: Option<String>,
    import_rx: Receiver<Result<String, String>>,
    import_tx: std::sync::mpsc::Sender<Result<String, String>>,
    progress_rx: Receiver<String>,
    progress_tx: std::sync::mpsc::Sender<String>,
    control_rx: Receiver<ControlCmd>,
    job: Option<String>,
    job_ratio: f64,
    search_hits: Vec<splayer::Hit>,
    search_idx: usize,
    search_rx: Receiver<Result<Vec<splayer::Hit>, String>>,
    search_tx: std::sync::mpsc::Sender<Result<Vec<splayer::Hit>, String>>,
    login: splayer::Login,
    local_filter: String,
    local_hits: Vec<usize>,
    local_idx: usize,
    taste: TasteStore,
    last_taste: Option<usize>,
    tab: Tab,
    show_lyrics: bool,
    prefs: Prefs,
    overlay: Overlay,
    settings_idx: usize,
    eq_band: usize,
    music_mode: MusicMode,
    artist_idx: usize,
    artist_state: ListState,
    artist_pages: Vec<browse::ArtistPage>,
    album_pages: Vec<browse::ArtistPage>,
    cava: Option<CavaFeed>,
    pending_seek: Option<f32>,
    library_idx: usize,
    browse_kind: BrowseKind,
    meta_queue: VecDeque<usize>,
    meta_inflight: HashSet<usize>,
    /// 远程控制 API 共享状态
    api_state: api::SharedState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Settings,
    Help,
    Eq,
    Library,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MusicMode {
    Library,
    Artists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowseKind {
    All,
    Tagged,
    Untagged,
    Artists,
    Albums,
}

impl BrowseKind {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Tagged,
            Self::Tagged => Self::Untagged,
            Self::Untagged => Self::Artists,
            Self::Artists => Self::Albums,
            Self::Albums => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Tagged => "有标签",
            Self::Untagged => "缺标签",
            Self::Artists => "歌手",
            Self::Albums => "专辑",
        }
    }
}

impl App {
    pub fn new(root: PathBuf) -> anyhow::Result<Self> {
        let player = Player::start().context("audio output")?;
        let (decode_tx, decode_rx) = mpsc::channel();
        let (import_tx, import_rx) = mpsc::channel();
        let (progress_tx, progress_rx) = mpsc::channel();
        let (search_tx, search_rx) = mpsc::channel();
        let (peek_tx, peek_rx) = mpsc::channel();
        let (remote_tx, remote_rx) = mpsc::channel();
        let (control_tx, control_rx) = mpsc::channel();
        // 新版 JSON API 替代旧 TCP 控制，监听 0.0.0.0:18765
        let api_state = api::new_shared_state();
        api::start(control_tx, Arc::clone(&api_state));
        let mut prefs = Prefs::load();
        if prefs.library.trim().is_empty() {
            prefs.set_library(&root);
        }
        let root = if root.exists() {
            root
        } else {
            prefs.library_path()
        };
        let tracks = scan_library(&root)?;
        let mut mixer = player.mixer.lock().expect("mixer");
        mixer.set_tracks(tracks.clone());
        drop(mixer);
        let mut list_state = ListState::default();
        if !tracks.is_empty() {
            list_state.select(Some(0));
        }
        let status = if tracks.is_empty() {
            format!("no audio in {}", root.display())
        } else {
            format!("{} · {} tracks", root.display(), tracks.len())
        };
        let metas = vec![None; tracks.len()];
        let mut app = Self {
            tracks,
            list_state,
            player,
            status,
            decoding: false,
            prefetch_attempt: None,
            analysis: AnalysisStore::load(),
            decode_rx,
            decode_tx,
            inflight: None,
            metas,
            peek_rx,
            peek_tx,
            remote_rx,
            remote_tx,
            mpris: Mpris::start(),
            tick: 0,
            command: None,
            import_rx,
            import_tx,
            progress_rx,
            progress_tx,
            control_rx,
            job: None,
            job_ratio: 0.0,
            search_hits: Vec::new(),
            search_idx: 0,
            search_rx,
            search_tx,
            login: splayer::login_status(),
            local_filter: String::new(),
            local_hits: Vec::new(),
            local_idx: 0,
            taste: TasteStore::load(),
            last_taste: None,
            tab: Tab::Music,
            show_lyrics: true,
            prefs,
            overlay: Overlay::None,
            settings_idx: 0,
            eq_band: 0,
            music_mode: MusicMode::Library,
            artist_idx: 0,
            artist_state: ListState::default(),
            artist_pages: Vec::new(),
            album_pages: Vec::new(),
            cava: CavaFeed::start(24),
            pending_seek: None,
            library_idx: 0,
            browse_kind: BrowseKind::All,
            meta_queue: VecDeque::new(),
            meta_inflight: HashSet::new(),
            api_state,
        };
        app.push_taste();
        if let Ok(mut mixer) = app.player.mixer.lock() {
            mixer.set_eq_db(app.prefs.eq_db);
            mixer.set_shuffle_mode(app.prefs.shuffle_mode);
        }
        app.spawn_peek();
        app.resume_if_needed();
        Ok(app)
    }

    fn selected(&self) -> usize {
        let row = self.list_state.selected().unwrap_or(0);
        self.visible_tracks().get(row).copied().unwrap_or(row)
    }

    fn move_local(&mut self, delta: i32) {
        if self.local_hits.is_empty() {
            return;
        }
        let len = self.local_hits.len() as i32;
        self.local_idx = (self.local_idx as i32 + delta).rem_euclid(len) as usize;
        self.list_state.select(Some(self.local_idx));
    }

    fn select_delta(&mut self, delta: i32) {
        let visible = self.visible_tracks();
        if visible.is_empty() {
            return;
        }
        let len = visible.len() as i32;
        let next =
            (self.list_state.selected().unwrap_or(0) as i32 + delta).rem_euclid(len) as usize;
        self.list_state.select(Some(next));
    }

    fn play_index(&mut self, index: usize, transition: bool) {
        self.pending_seek = None;
        self.note_leave(transition);
        self.sync_list_cursor(index);
        self.spawn_decode(index, transition, transition);
    }

    fn sync_list_cursor(&mut self, track_index: usize) {
        let visible = self.visible_tracks();
        if let Some(row) = visible_row(&visible, track_index) {
            if !self.local_filter.is_empty() {
                self.local_idx = row;
            }
            self.list_state.select(Some(row));
            return;
        }
        self.list_state.select(Some(track_index));
    }

    fn resume_if_needed(&mut self) {
        if !self.prefs.resume {
            return;
        }
        let session = SessionState::load();
        let Some(index) = session.track_index(&self.tracks) else {
            return;
        };
        self.play_index(index, false);
        self.pending_seek = Some(session.position_secs);
    }

    fn note_leave(&mut self, skipped: bool) {
        let (idx, ratio) = {
            let mixer = self.player.mixer.lock().expect("mixer");
            let Some(idx) = mixer.snapshot(0).current else {
                return;
            };
            (idx, mixer.listen_ratio())
        };
        if self.last_taste == Some(idx) {
            return;
        }
        let Some(track) = self.tracks.get(idx) else {
            return;
        };
        let artist = self
            .metas
            .get(idx)
            .and_then(|m| m.as_ref())
            .map(|m| m.artist.as_str())
            .unwrap_or("");
        let duration_secs = {
            let mixer = self.player.mixer.lock().expect("mixer");
            let snap = mixer.snapshot(0);
            if snap.sample_rate == 0 {
                0.0
            } else {
                snap.duration_frames as f32 / snap.sample_rate as f32
            }
        };
        self.taste.record_leave(
            &track.path,
            &track.title,
            artist,
            ratio,
            skipped,
            duration_secs,
        );
        self.last_taste = Some(idx);
        self.push_taste();
    }

    fn push_taste(&mut self) {
        let artists: Vec<String> = (0..self.tracks.len())
            .map(|i| {
                self.metas
                    .get(i)
                    .and_then(|m| m.as_ref())
                    .map(|m| m.artist.clone())
                    .unwrap_or_default()
            })
            .collect();
        let boosts = self.taste.boosts_for(&self.tracks, &artists);
        self.player
            .mixer
            .lock()
            .expect("mixer")
            .set_taste_boosts(boosts);
    }

    fn spawn_decode(&mut self, index: usize, transition: bool, skip: bool) {
        let Some(track) = self.tracks.get(index).cloned() else {
            return;
        };
        self.decoding = true;
        self.inflight = Some(index);
        self.status = format!("decode {}", track.title);
        let rate = self.player.sample_rate;
        let ch = self.player.channels;
        let cached = self.analysis.get(&track.path);
        let tx = self.decode_tx.clone();
        thread::spawn(move || {
            let result = load_track(&track.path, rate, ch).map(|buf| {
                let analysis = cached.unwrap_or_else(|| analyze_buffer(&buf));
                DecodeJob {
                    index,
                    transition,
                    skip,
                    buf,
                    analysis,
                    path: track.path,
                }
            });
            let _ = tx.send(result.map_err(|err| (index, err.to_string())));
        });
    }

    fn drain_decode(&mut self) {
        loop {
            match self.decode_rx.try_recv() {
                Ok(Ok(job)) => self.take_decode(job),
                Ok(Err((index, err))) => {
                    if self.inflight == Some(index) {
                        self.decoding = false;
                        self.inflight = None;
                        self.status = format!("fail {err}");
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn take_decode(&mut self, mut job: DecodeJob) {
        if let Ok(meta) = std::fs::metadata(&job.path) {
            job.analysis.mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            job.analysis.size = meta.len();
        }
        self.analysis.insert(&job.path, job.analysis.clone());
        let remix = self.player.mixer.lock().expect("mixer").remix();
        let buf = apply_remix(&job.buf, remix);
        let mut mixer = self.player.mixer.lock().expect("mixer");
        mixer.set_analysis(job.index, job.analysis);
        let meta = load_meta(
            &job.path,
            &self
                .tracks
                .get(job.index)
                .map(|t| t.title.clone())
                .unwrap_or_default(),
        );
        mixer.set_track_title(job.index, meta.title.clone());
        if let Some(track) = self.tracks.get_mut(job.index) {
            track.title = meta.title.clone();
        }
        if job.index >= self.metas.len() {
            self.metas.resize(job.index + 1, None);
        }
        self.metas[job.index] = Some(meta.clone());
        let queue_remote = self.wants_remote(&meta);
        drop(mixer);
        if queue_remote {
            self.queue_meta(job.index);
        }
        self.refresh_artists();
        self.push_taste();
        let mut mixer = self.player.mixer.lock().expect("mixer");
        if job.transition && mixer.mix().uses_fade() {
            mixer.start_transition(job.index, buf, job.skip);
        } else {
            mixer.play_decoded(job.index, buf);
        }
        if let Some(secs) = self.pending_seek.take() {
            let snap = mixer.snapshot(job.index);
            let duration = if snap.sample_rate == 0 {
                0.0
            } else {
                snap.duration_frames as f32 / snap.sample_rate as f32
            };
            mixer.seek_to(crate::session_store::resume_seek(secs, duration));
        }
        self.last_taste = None;
        self.status = mixer.snapshot(job.index).status;
        drop(mixer);
        if self.inflight == Some(job.index) {
            self.decoding = false;
            self.inflight = None;
        }
        self.prefetch_attempt = None;
    }

    fn wants_remote(&self, meta: &TrackMeta) -> bool {
        needs_fetch(meta, self.prefs.lyrics_fetch, self.prefs.cover_fetch)
    }

    fn queue_meta(&mut self, index: usize) {
        if self.meta_inflight.contains(&index) || self.meta_queue.contains(&index) {
            return;
        }
        self.meta_queue.push_back(index);
    }

    fn spawn_remote_meta(&self, index: usize, path: PathBuf, meta: TrackMeta) {
        let tx = self.remote_tx.clone();
        let lyrics_on = meta.lyrics.is_empty() && self.prefs.lyrics_fetch;
        let need_tags = meta.artist.trim().is_empty() || meta.album.trim().is_empty();
        let cover_on = meta.cover.is_none() && meta.cover_path.is_none() && self.prefs.cover_fetch;
        thread::spawn(move || {
            let itunes = if need_tags || cover_on {
                crate::remote_meta::fetch_itunes(&meta.title, &meta.artist, &meta.album)
            } else {
                None
            };
            let lyrics = if lyrics_on {
                let artist = itunes
                    .as_ref()
                    .map(|h| h.artist.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(meta.artist.as_str());
                let title = itunes
                    .as_ref()
                    .map(|h| h.title.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(meta.title.as_str());
                crate::remote_meta::fetch_lyrics(title, artist)
            } else {
                None
            };
            let cover = if cover_on {
                itunes
                    .as_ref()
                    .and_then(|h| h.artwork.as_deref())
                    .and_then(crate::remote_meta::fetch_bytes)
                    .or_else(|| {
                        crate::remote_meta::fetch_cover(&meta.title, &meta.artist, &meta.album)
                    })
            } else {
                None
            };
            let _ = tx.send(RemoteMetaJob {
                index,
                lyrics,
                cover,
                artist: itunes
                    .as_ref()
                    .map(|h| h.artist.clone())
                    .filter(|s| !s.is_empty()),
                title: itunes
                    .as_ref()
                    .map(|h| h.title.clone())
                    .filter(|s| !s.is_empty()),
                album: itunes
                    .as_ref()
                    .map(|h| h.album.clone())
                    .filter(|s| !s.is_empty()),
                path,
            });
        });
    }

    fn drain_remote_meta(&mut self) {
        let mut changed = false;
        while let Ok(job) = self.remote_rx.try_recv() {
            self.meta_inflight.remove(&job.index);
            if job.index >= self.metas.len() {
                continue;
            }
            if self.metas[job.index].is_none() {
                self.metas[job.index] = Some(TrackMeta::default());
            }
            let Some(slot) = self.metas[job.index].as_mut() else {
                continue;
            };
            if slot.artist.trim().is_empty() {
                if let Some(artist) = job.artist.filter(|s| !s.trim().is_empty()) {
                    slot.artist = artist;
                    changed = true;
                }
            }
            if slot.title.trim().is_empty() {
                if let Some(title) = job.title.filter(|s| !s.trim().is_empty()) {
                    slot.title = title.clone();
                    if let Some(track) = self.tracks.get_mut(job.index) {
                        track.title = title;
                    }
                    changed = true;
                }
            }
            if slot.album.trim().is_empty() {
                if let Some(album) = job.album.filter(|s| !s.trim().is_empty()) {
                    slot.album = album;
                    changed = true;
                }
            }
            if let Some(raw) = job.lyrics {
                crate::remote_meta::save_sidecar_lrc(&job.path, &raw);
                slot.lyrics = crate::meta::parse_lyrics(&raw);
                changed = true;
            }
            if let Some(bytes) = job.cover {
                if let Some((cover_path, cover)) = crate::meta::cache_cover(&job.path, &bytes) {
                    slot.cover_path = Some(cover_path);
                    slot.cover = cover;
                    changed = true;
                }
            }
        }
        if changed {
            self.refresh_artists();
            self.push_taste();
        }
    }

    fn spawn_peek(&self) {
        let tracks: Vec<(usize, PathBuf, String)> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.path.clone(), t.title.clone()))
            .collect();
        let tx = self.peek_tx.clone();
        thread::spawn(move || {
            for (index, path, title) in tracks {
                let meta = peek_tags(&path, &title);
                if tx.send(PeekJob { index, meta }).is_err() {
                    break;
                }
            }
        });
    }

    fn drain_peek(&mut self) {
        let mut n = 0;
        loop {
            match self.peek_rx.try_recv() {
                Ok(job) => {
                    if job.index >= self.metas.len() {
                        self.metas.resize(job.index + 1, None);
                    }
                    if apply_peek(&mut self.metas[job.index], job.meta) {
                        if let Some(title) = self.metas[job.index]
                            .as_ref()
                            .map(|m| m.title.clone())
                            .filter(|t| !t.is_empty())
                        {
                            if let Some(track) = self.tracks.get_mut(job.index) {
                                track.title = title;
                            }
                        }
                        n += 1;
                        if self
                            .metas
                            .get(job.index)
                            .and_then(|m| m.as_ref())
                            .is_some_and(|m| self.wants_remote(m))
                        {
                            self.queue_meta(job.index);
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if n > 0 {
            self.refresh_artists();
            self.push_taste();
        }
    }

    fn refresh_artists(&mut self) {
        self.artist_pages = browse::artist_pages(&self.metas);
        self.album_pages = browse::album_pages(&self.metas);
        if self.browse_kind == BrowseKind::Artists || self.browse_kind == BrowseKind::Albums {
            self.reset_group_cursor();
        }
    }

    fn group_pages(&self) -> &[browse::ArtistPage] {
        match self.browse_kind {
            BrowseKind::Albums => &self.album_pages,
            _ => &self.artist_pages,
        }
    }

    fn reset_group_cursor(&mut self) {
        let len = self.group_pages().len();
        if len == 0 {
            self.artist_idx = 0;
            self.artist_state.select(None);
            return;
        }
        if self.artist_idx >= len {
            self.artist_idx = 0;
        }
        self.artist_state.select(Some(self.artist_idx));
    }

    fn apply_browse(&mut self) {
        self.local_hits.clear();
        self.local_filter.clear();
        self.local_idx = 0;
        match self.browse_kind {
            BrowseKind::All => {
                self.music_mode = MusicMode::Library;
                self.list_state.select(if self.tracks.is_empty() {
                    None
                } else {
                    Some(0)
                });
                self.status = format!("browse {}", self.browse_kind.label());
            }
            BrowseKind::Tagged => {
                self.music_mode = MusicMode::Library;
                self.local_filter = "有标签".into();
                self.local_hits = browse::tagged_indices(&self.metas);
                self.list_state.select(if self.local_hits.is_empty() {
                    None
                } else {
                    Some(0)
                });
                self.status = format!("有标签 · {} tracks", self.local_hits.len());
            }
            BrowseKind::Untagged => {
                self.music_mode = MusicMode::Library;
                self.local_filter = "缺标签".into();
                self.local_hits = browse::untagged_indices(&self.metas);
                self.list_state.select(if self.local_hits.is_empty() {
                    None
                } else {
                    Some(0)
                });
                self.status = format!("缺标签 · {} tracks", self.local_hits.len());
            }
            BrowseKind::Artists | BrowseKind::Albums => {
                self.music_mode = MusicMode::Artists;
                self.reset_group_cursor();
                self.status = format!(
                    "{} · {} groups",
                    self.browse_kind.label(),
                    self.group_pages().len()
                );
            }
        }
    }

    fn maybe_prefetch(&mut self) {
        if self.inflight.is_some() {
            return;
        }
        let (need, index, mix) = {
            let mixer = self.player.mixer.lock().expect("mixer");
            (mixer.should_prefetch(), mixer.next_index(), mixer.mix())
        };
        if !need {
            return;
        }
        let Some(index) = index else {
            return;
        };
        if self.prefetch_attempt == Some(index) {
            return;
        }
        self.prefetch_attempt = Some(index);
        if !mix.uses_fade() {
            let finished = self.player.mixer.lock().expect("mixer").take_finished();
            if finished {
                self.note_leave(false);
                self.sync_list_cursor(index);
                self.spawn_decode(index, false, false);
            }
            return;
        }
        self.note_leave(false);
        self.spawn_decode(index, true, false);
    }

    pub fn run(mut self) -> anyhow::Result<()> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        if self.prefs.transparent {
            execute!(
                stdout,
                EnterAlternateScreen,
                EnableBracketedPaste,
                crossterm::style::SetBackgroundColor(crossterm::style::Color::Reset)
            )?;
        } else {
            execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
        }
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        let tick = Duration::from_millis(33);
        let mut last = Instant::now();
        let result = loop {
            terminal.draw(|frame| self.draw(frame))?;
            let timeout = tick.saturating_sub(last.elapsed());
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if self.handle_key(key) {
                            break Ok(());
                        }
                    }
                    Event::Paste(text) => self.handle_paste(&text),
                    _ => {}
                }
            }
            if last.elapsed() >= tick {
                self.tick = self.tick.wrapping_add(1);
                self.drain_decode();
                self.drain_peek();
                self.drain_remote_meta();
                self.pump_meta_queue();
                self.drain_progress();
                self.drain_import();
                self.drain_search();
                self.drain_control();
                if self.tick.is_multiple_of(90) {
                    self.login = splayer::login_status();
                }
                self.drain_mpris();
                self.publish_mpris();
                self.update_api_state();
                self.maybe_prefetch();
                if let Ok(mut mixer) = self.player.mixer.lock() {
                    if let Some(cava) = self.cava.as_ref() {
                        let bars = cava.latest();
                        if !bars.is_empty() {
                            mixer.ingest_cava(&bars);
                        } else {
                            mixer.refresh_spectrum();
                        }
                    } else {
                        mixer.refresh_spectrum();
                    }
                }
                last = Instant::now();
            }
        };
        self.save_session();
        self.taste.flush();
        disable_raw_mode()?;
        execute!(
            io::stdout(),
            crossterm::event::DisableBracketedPaste,
            LeaveAlternateScreen
        )?;
        result
    }

    fn handle_key(&mut self, key: event::KeyEvent) -> bool {
        if let Some(buf) = self.command.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.command = None;
                    self.status = "cancelled".into();
                }
                KeyCode::Enter => {
                    let raw = buf.clone();
                    self.command = None;
                    self.submit_command(&raw);
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return false;
        }
        if self.overlay != Overlay::None {
            return self.handle_overlay_key(key);
        }
        match key.code {
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.save_session();
                return true;
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.overlay = Overlay::Help;
            }
            KeyCode::Char('t') => {
                self.overlay = Overlay::Settings;
                self.settings_idx = 0;
            }
            KeyCode::Char('y') => {
                self.overlay = Overlay::Library;
                self.library_idx = 0;
            }
            KeyCode::Char('g') if self.tab == Tab::Player => {
                self.overlay = Overlay::Eq;
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.command = Some("open ".into());
                self.status = "open 本地文件夹".into();
            }
            KeyCode::Char('1') => {
                self.browse_kind = BrowseKind::All;
                self.apply_browse();
            }
            KeyCode::Char('2') => {
                self.browse_kind = BrowseKind::Artists;
                self.apply_browse();
            }
            KeyCode::Char('q') => {
                self.tab = self.tab.prev();
                self.status = format!("tab {}", self.tab.label());
            }
            KeyCode::Char('e') => {
                self.tab = self.tab.next();
                self.status = format!("tab {}", self.tab.label());
            }
            KeyCode::Char('l') if self.tab == Tab::Player => {
                self.show_lyrics = !self.show_lyrics;
                self.status = if self.show_lyrics {
                    "lyrics on".into()
                } else {
                    "lyrics off".into()
                };
            }
            KeyCode::Esc => match esc_outcome(
                !self.search_hits.is_empty(),
                !self.local_hits.is_empty() || !self.local_filter.is_empty(),
            ) {
                EscOutcome::CloseSearch => {
                    self.search_hits.clear();
                    self.status = "search closed".into();
                }
                EscOutcome::CloseLocal => {
                    self.local_hits.clear();
                    self.local_filter.clear();
                    self.status = "local search closed".into();
                }
                EscOutcome::Dismiss => {
                    self.status = "Ctrl+Q 退出".into();
                }
            },
            KeyCode::Char(':') => {
                self.command = Some(String::new());
                self.status = "url / search 歌名 / find 本地".into();
            }
            KeyCode::Char('/') => {
                self.command = Some("find ".into());
                self.status = "find 本地歌名".into();
            }
            KeyCode::Char('?') => {
                self.command = Some("search ".into());
                self.status = "search 网易/QQ".into();
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_focus(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_focus(-1),
            KeyCode::Enter => {
                if !self.search_hits.is_empty() {
                    self.download_hit(self.search_idx);
                } else if self.tab == Tab::Music && self.music_mode == MusicMode::Artists {
                    if let Some((name, tracks)) =
                        browse::open_artist(self.group_pages(), self.artist_idx)
                    {
                        self.music_mode = MusicMode::Library;
                        self.local_filter = name.clone();
                        self.local_hits = tracks;
                        self.local_idx = 0;
                        self.list_state.select(Some(0));
                        self.status = format!("{name} · {} tracks", self.local_hits.len());
                    }
                } else if !self.local_hits.is_empty() {
                    let i = self.local_hits[self.local_idx];
                    self.play_index(i, false);
                } else {
                    let i = self.selected();
                    self.play_index(i, false);
                }
            }
            KeyCode::Char(' ') => {
                let mut mixer = self.player.mixer.lock().expect("mixer");
                if mixer.tracks().is_empty() {
                    drop(mixer);
                } else if mixer.snapshot(0).current.is_none() {
                    drop(mixer);
                    let i = self.selected();
                    self.play_index(i, false);
                } else {
                    mixer.toggle_pause();
                    self.status = mixer.snapshot(self.selected()).status;
                }
            }
            KeyCode::Char('n') => {
                let next = self.player.mixer.lock().expect("mixer").next_index();
                if let Some(i) = next {
                    self.play_index(i, true);
                }
            }
            KeyCode::Char('p') => {
                let prev = self.player.mixer.lock().expect("mixer").prev_index();
                if let Some(i) = prev {
                    self.play_index(i, true);
                }
            }
            KeyCode::Char('m') => {
                let mut mixer = self.player.mixer.lock().expect("mixer");
                mixer.cycle_mix();
                self.status = mixer.snapshot(self.selected()).status;
            }
            KeyCode::Char('r') => {
                let mut mixer = self.player.mixer.lock().expect("mixer");
                mixer.cycle_remix();
                self.status = mixer.snapshot(self.selected()).status;
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                let mut mixer = self.player.mixer.lock().expect("mixer");
                mixer.bump_volume(0.05);
                self.status = mixer.snapshot(self.selected()).status;
            }
            KeyCode::Char('-') => {
                let mut mixer = self.player.mixer.lock().expect("mixer");
                mixer.bump_volume(-0.05);
                self.status = mixer.snapshot(self.selected()).status;
            }
            KeyCode::Char('s') => {
                let next = {
                    let mixer = self.player.mixer.lock().expect("mixer");
                    mixer.shuffle_mode().next()
                };
                self.apply_shuffle(next);
            }
            KeyCode::Char('o') => {
                let mut mixer = self.player.mixer.lock().expect("mixer");
                mixer.cycle_loop();
                self.status = mixer.snapshot(self.selected()).status;
            }
            KeyCode::Left => {
                let mut mixer = self.player.mixer.lock().expect("mixer");
                mixer.seek_by(-5.0);
                self.status = mixer.snapshot(self.selected()).status;
            }
            KeyCode::Right => {
                let mut mixer = self.player.mixer.lock().expect("mixer");
                mixer.seek_by(5.0);
                self.status = mixer.snapshot(self.selected()).status;
            }
            _ => {}
        }
        false
    }

    fn move_focus(&mut self, delta: i32) {
        if !self.search_hits.is_empty() {
            let len = self.search_hits.len() as i32;
            self.search_idx = (self.search_idx as i32 + delta).rem_euclid(len) as usize;
            return;
        }
        if self.tab == Tab::Music && self.music_mode == MusicMode::Artists {
            let len = self.group_pages().len() as i32;
            if len == 0 {
                return;
            }
            self.artist_idx = (self.artist_idx as i32 + delta).rem_euclid(len) as usize;
            self.artist_state.select(Some(self.artist_idx));
            return;
        }
        if !self.local_filter.is_empty() {
            self.move_local(delta);
        } else {
            self.select_delta(delta);
        }
    }

    fn handle_overlay_key(&mut self, key: event::KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('t') | KeyCode::Char('g') | KeyCode::Char('y') => {
                self.overlay = Overlay::None;
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.overlay = Overlay::None;
            }
            KeyCode::Char('j') | KeyCode::Down if self.overlay == Overlay::Settings => {
                self.settings_idx = (self.settings_idx + 1) % 7;
            }
            KeyCode::Char('k') | KeyCode::Up if self.overlay == Overlay::Settings => {
                self.settings_idx = (self.settings_idx + 6) % 7;
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.overlay == Overlay::Settings => {
                self.toggle_setting();
            }
            KeyCode::Char('j') | KeyCode::Down if self.overlay == Overlay::Library => {
                self.library_idx = (self.library_idx + 1) % 5;
            }
            KeyCode::Char('k') | KeyCode::Up if self.overlay == Overlay::Library => {
                self.library_idx = (self.library_idx + 4) % 5;
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.overlay == Overlay::Library => {
                self.toggle_library();
            }
            KeyCode::Char('j') | KeyCode::Down if self.overlay == Overlay::Eq => {
                self.eq_band = (self.eq_band + 1) % 5;
            }
            KeyCode::Char('k') | KeyCode::Up if self.overlay == Overlay::Eq => {
                self.eq_band = (self.eq_band + 4) % 5;
            }
            KeyCode::Left if self.overlay == Overlay::Eq => self.bump_eq(-1.0),
            KeyCode::Right if self.overlay == Overlay::Eq => self.bump_eq(1.0),
            KeyCode::Char('r') if self.overlay == Overlay::Eq => {
                if let Ok(mut mixer) = self.player.mixer.lock() {
                    mixer.reset_eq();
                    self.prefs.reset_eq();
                    self.prefs.save();
                    self.status = mixer.snapshot(self.selected()).status;
                }
            }
            _ => {}
        }
        false
    }

    fn toggle_setting(&mut self) {
        match self.settings_idx {
            0 => self.prefs.theme = self.prefs.theme.next(),
            1 => self.prefs.transparent = !self.prefs.transparent,
            2 => self.prefs.visualize = self.prefs.visualize.next(),
            3 => self.prefs.lyrics_fetch = !self.prefs.lyrics_fetch,
            4 => self.prefs.cover_fetch = !self.prefs.cover_fetch,
            5 => self.prefs.resume = !self.prefs.resume,
            6 => self.overlay = Overlay::None,
            _ => {}
        }
        self.prefs.save();
        self.status = format!(
            "theme {}  viz {}  fetch {}/{}",
            self.prefs.theme.label(),
            self.prefs.visualize.label(),
            self.prefs.lyrics_fetch,
            self.prefs.cover_fetch
        );
    }

    fn toggle_library(&mut self) {
        match self.library_idx {
            0 => {
                let keep = self.selected();
                self.prefs.sort_mode = self.prefs.sort_mode.next();
                self.prefs.save();
                self.sync_list_cursor(keep);
                self.status = format!("sort {}", self.prefs.sort_mode.label());
            }
            1 => {
                let next = self.prefs.shuffle_mode.next();
                self.apply_shuffle(next);
            }
            2 => {
                self.browse_kind = self.browse_kind.next();
                self.apply_browse();
                self.overlay = Overlay::Library;
            }
            3 => {
                let n = self.enqueue_missing_meta();
                self.pump_meta_queue();
                self.status = if n == 0 {
                    "meta scan idle".into()
                } else {
                    format!("meta queue {n}")
                };
            }
            4 => self.overlay = Overlay::None,
            _ => {}
        }
    }

    fn apply_shuffle(&mut self, mode: ShuffleMode) {
        self.prefs.shuffle_mode = mode;
        self.prefs.save();
        if let Ok(mut mixer) = self.player.mixer.lock() {
            mixer.set_shuffle_mode(mode);
            self.status = mixer.snapshot(self.selected()).status;
        }
    }

    fn enqueue_missing_meta(&mut self) -> usize {
        let mut n = 0;
        for i in 0..self.tracks.len() {
            if self.meta_inflight.contains(&i) || self.meta_queue.contains(&i) {
                continue;
            }
            let missing = self
                .metas
                .get(i)
                .and_then(|m| m.as_ref())
                .is_none_or(|m| self.wants_remote(m));
            if missing {
                self.meta_queue.push_back(i);
                n += 1;
            }
        }
        n
    }

    fn pump_meta_queue(&mut self) {
        const META_PARALLEL: usize = 6;
        while self.meta_inflight.len() < META_PARALLEL {
            let Some(index) = self.meta_queue.pop_front() else {
                break;
            };
            if self.meta_inflight.contains(&index) {
                continue;
            }
            let Some(track) = self.tracks.get(index) else {
                continue;
            };
            let meta = self
                .metas
                .get(index)
                .and_then(|m| m.clone())
                .unwrap_or_else(|| TrackMeta {
                    title: track.title.clone(),
                    ..TrackMeta::default()
                });
            if !self.wants_remote(&meta) {
                continue;
            }
            self.meta_inflight.insert(index);
            self.spawn_remote_meta(index, track.path.clone(), meta);
        }
        if !self.meta_inflight.is_empty() {
            self.job = Some(format!(
                "meta {} queued · {} inflight",
                self.meta_queue.len(),
                self.meta_inflight.len()
            ));
        }
    }

    fn bump_eq(&mut self, delta: f32) {
        if let Ok(mut mixer) = self.player.mixer.lock() {
            mixer.bump_eq(self.eq_band, delta);
            self.prefs.eq_db = mixer.eq_db();
            self.prefs.save();
            self.status = mixer.snapshot(self.selected()).status;
        }
    }

    fn save_session(&self) {
        if !self.prefs.resume {
            return;
        }
        let mixer = self.player.mixer.lock().ok();
        let Some(mixer) = mixer else {
            return;
        };
        let snap = mixer.snapshot(0);
        let Some(idx) = snap.current else {
            return;
        };
        let Some(track) = self.tracks.get(idx) else {
            return;
        };
        let secs = if snap.sample_rate == 0 {
            0.0
        } else {
            snap.position_frames as f32 / snap.sample_rate as f32
        };
        SessionState::save_for(&track.path, secs);
    }

    fn palette(&self) -> Palette {
        Palette::for_theme(self.prefs.theme)
    }

    fn handle_paste(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(buf) = self.command.as_mut() {
            buf.push_str(text);
            return;
        }
        if looks_like_media_url(text) {
            self.command = Some(text.to_string());
            self.status = "enter to import pasted url".into();
        }
    }

    fn submit_command(&mut self, raw: &str) {
        let raw = raw.trim();
        if raw.is_empty() {
            return;
        }
        if looks_like_media_url(raw) {
            self.start_import(raw);
            return;
        }
        if let Some(rest) = raw.strip_prefix("search ") {
            self.search_splayer(rest);
            return;
        }
        if let Some(rest) = raw.strip_prefix("find ") {
            self.search_local(rest);
            return;
        }
        if let Some(rest) = raw.strip_prefix("open ") {
            self.open_folder(PathBuf::from(rest.trim()));
            return;
        }
        self.search_local(raw);
    }

    fn open_folder(&mut self, root: PathBuf) {
        match scan_library(&root) {
            Ok(tracks) => {
                self.prefs.set_library(&root);
                self.prefs.save();
                self.tracks = tracks;
                self.metas = vec![None; self.tracks.len()];
                self.meta_queue.clear();
                self.meta_inflight.clear();
                if let Ok(mut mixer) = self.player.mixer.lock() {
                    mixer.set_tracks(self.tracks.clone());
                    mixer.set_shuffle_mode(self.prefs.shuffle_mode);
                }
                self.spawn_peek();
                self.refresh_artists();
                self.list_state.select(if self.tracks.is_empty() {
                    None
                } else {
                    Some(0)
                });
                self.status = format!("open {} · {} tracks", root.display(), self.tracks.len());
            }
            Err(err) => self.status = format!("open fail {err}"),
        }
    }

    fn rescan_library(&mut self) {
        let root = self.prefs.library_path();
        match scan_library(&root) {
            Ok(tracks) => {
                self.tracks = tracks;
                self.metas = vec![None; self.tracks.len()];
                self.meta_queue.clear();
                self.meta_inflight.clear();
                if let Ok(mut mixer) = self.player.mixer.lock() {
                    mixer.set_tracks(self.tracks.clone());
                    mixer.set_shuffle_mode(self.prefs.shuffle_mode);
                }
                self.spawn_peek();
                self.refresh_artists();
                self.list_state.select(if self.tracks.is_empty() {
                    None
                } else {
                    Some(0)
                });
                self.status = format!("rescan {} · {} tracks", root.display(), self.tracks.len());
            }
            Err(err) => self.status = format!("rescan fail {err}"),
        }
    }

    fn search_local(&mut self, query: &str) {
        self.browse_kind = BrowseKind::All;
        self.music_mode = MusicMode::Library;
        self.local_filter = query.trim().to_string();
        self.local_hits = browse::meta_search(&self.tracks, &self.metas, query);
        self.local_idx = 0;
        self.list_state.select(if self.local_hits.is_empty() {
            None
        } else {
            Some(0)
        });
        self.job = Some(format!("local {} · {} hits", query, self.local_hits.len()));
        self.status = format!("local {} · {} hits", query, self.local_hits.len());
    }

    fn start_import(&mut self, url: &str) {
        self.job = Some(format!("import {url}"));
        self.job_ratio = 0.02;
        self.status = format!("import {url}");
        let dest = fetch::library_dir();
        let tx = self.import_tx.clone();
        let progress = self.progress_tx.clone();
        let url = url.to_string();
        thread::spawn(move || {
            let result =
                fetch::download_audio(&url, &dest, Some(progress)).map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    fn search_splayer(&mut self, query: &str) {
        self.status = format!("search {query}");
        self.job = Some(format!("search {query}"));
        self.job_ratio = 0.05;
        let tx = self.search_tx.clone();
        let query = query.to_string();
        thread::spawn(move || {
            let result = splayer::search(&query, 12).map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    fn drain_search(&mut self) {
        while let Ok(result) = self.search_rx.try_recv() {
            match result {
                Ok(hits) => {
                    self.search_hits = hits;
                    self.search_idx = 0;
                    let n = self.search_hits.len();
                    self.job = None;
                    self.status = format!("{n} hits · enter download · esc close");
                }
                Err(err) => {
                    self.search_hits.clear();
                    self.job = None;
                    self.status = format!("search fail {err}");
                }
            }
        }
    }

    fn download_hit(&mut self, index: usize) {
        let Some(hit) = self.search_hits.get(index).cloned() else {
            return;
        };
        self.job = Some(format!("dl {} — {}", hit.artist, hit.title));
        self.job_ratio = 0.08;
        self.status = format!("dl {} — {}", hit.artist, hit.title);
        let dest = fetch::library_dir();
        let tx = self.import_tx.clone();
        let progress = self.progress_tx.clone();
        thread::spawn(move || {
            let _ = progress.send(format!("resolving {} {}", hit.source, hit.id));
            let result = splayer::download(&hit, &dest, Some(progress))
                .map(|p| p.display().to_string())
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    fn add_imported(&mut self, path: PathBuf, play: bool) {
        let Some(track) = track_from_path(path) else {
            return;
        };
        let existing = self.tracks.iter().position(|t| t.path == track.path);
        let index = if let Some(i) = existing {
            i
        } else {
            let peeked = peek_tags(&track.path, &track.title);
            self.tracks.push(track);
            self.metas.push(None);
            apply_peek(&mut self.metas[self.tracks.len() - 1], peeked);
            if let Ok(mut mixer) = self.player.mixer.lock() {
                mixer.set_tracks(self.tracks.clone());
            }
            self.refresh_artists();
            self.tracks.len() - 1
        };
        self.search_hits.clear();
        self.music_mode = MusicMode::Library;
        self.tab = Tab::Music;
        self.sync_list_cursor(index);
        if play {
            self.play_index(index, false);
        }
    }

    fn drain_progress(&mut self) {
        while let Ok(msg) = self.progress_rx.try_recv() {
            if let Some(ratio) = fetch::parse_percent(&msg) {
                self.job_ratio = ratio;
            } else if self.job_ratio < 0.95 {
                self.job_ratio = (self.job_ratio + 0.04).min(0.95);
            }
            self.status = format!("dl {msg}");
            self.job = Some(format!("dl {msg}"));
        }
    }

    fn drain_import(&mut self) {
        loop {
            match self.import_rx.try_recv() {
                Ok(Ok(path)) => {
                    self.job_ratio = 1.0;
                    self.job = Some(format!("imported {path}"));
                    self.status = format!("imported {path}");
                    self.add_imported(PathBuf::from(path), true);
                }
                Ok(Err(err)) => {
                    self.job_ratio = 0.0;
                    self.job = Some(format!("fail {err}"));
                    self.status = format!("fail {err}");
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    /// 将当前播放状态同步到 API 共享状态，供远程客户端读取
    fn update_api_state(&self) {
        let Ok(mut snap) = self.api_state.lock() else {
            return;
        };

        // 播放快照
        let snapshot = self.player.mixer.lock().ok().map(|m| m.snapshot(0));

        // 当前曲目元数据
        let current_idx = snapshot.as_ref().and_then(|s| s.current);
        let track = current_idx.and_then(|idx| {
            let track = self.tracks.get(idx)?;
            let meta = self.metas.get(idx).and_then(|m| m.as_ref());
            Some(api::TrackInfo {
                index: idx,
                title: meta
                    .map(|m| m.title.clone())
                    .unwrap_or_else(|| track.title.clone()),
                artist: meta.map(|m| m.artist.clone()).unwrap_or_default(),
                album: meta.map(|m| m.album.clone()).unwrap_or_default(),
                path: track.path.display().to_string(),
                has_cover: meta.is_some_and(|m| m.cover.is_some() || m.cover_path.is_some()),
                has_lyrics: meta.is_some_and(|m| !m.lyrics.is_empty()),
            })
        });

        // EQ
        let eq = self
            .player
            .mixer
            .lock()
            .ok()
            .map(|m| m.eq_db())
            .unwrap_or([0.0; 5]);

        // Taste 排行
        let taste_top: Vec<api::TasteEntry> = self
            .taste
            .top_tracks(10)
            .into_iter()
            .map(|(title, artist, score)| api::TasteEntry {
                title,
                artist,
                score,
            })
            .collect();

        // 完整资料库（每 tick 重建，开销可接受）
        let library: Vec<api::TrackInfo> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let meta = self.metas.get(i).and_then(|m| m.as_ref());
                api::TrackInfo {
                    index: i,
                    title: meta
                        .map(|m| m.title.clone())
                        .unwrap_or_else(|| t.title.clone()),
                    artist: meta.map(|m| m.artist.clone()).unwrap_or_default(),
                    album: meta.map(|m| m.album.clone()).unwrap_or_default(),
                    path: t.path.display().to_string(),
                    has_cover: meta.is_some_and(|m| m.cover.is_some() || m.cover_path.is_some()),
                    has_lyrics: meta.is_some_and(|m| !m.lyrics.is_empty()),
                }
            })
            .collect();

        // 当前歌词
        let lyrics: Vec<api::LyricLine> = current_idx
            .and_then(|idx| self.metas.get(idx))
            .and_then(|m| m.as_ref())
            .map(|meta| {
                meta.lyrics
                    .iter()
                    .map(|l| api::LyricLine {
                        time: l.time,
                        text: l.text.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 偏好快照
        let prefs = Some(api::PrefsSnapshot {
            theme: self.prefs.theme.label().into(),
            transparent: self.prefs.transparent,
            visualize: self.prefs.visualize.label().into(),
            lyrics_fetch: self.prefs.lyrics_fetch,
            cover_fetch: self.prefs.cover_fetch,
            resume: self.prefs.resume,
            library: self.prefs.library.clone(),
            sort_mode: self.prefs.sort_mode.label().into(),
            shuffle_mode: self.prefs.shuffle_mode.label().into(),
        });

        snap.snapshot = snapshot;
        snap.track = track;
        snap.tracks_total = self.tracks.len();
        snap.eq = eq;
        snap.taste_top = taste_top;
        snap.total_listen_secs = self.taste.total_listen_secs();
        snap.library = library;
        snap.lyrics = lyrics;
        snap.prefs = prefs;
    }

    fn drain_control(&mut self) {
        while let Ok(cmd) = self.control_rx.try_recv() {
            match cmd {
                // ── 基础播放控制 ──
                ControlCmd::Play => {
                    let i = self.selected();
                    let empty = self
                        .player
                        .mixer
                        .lock()
                        .ok()
                        .is_none_or(|m| m.snapshot(0).current.is_none());
                    if empty {
                        self.play_index(i, false);
                    } else if let Ok(mut mixer) = self.player.mixer.lock() {
                        if mixer.snapshot(0).paused {
                            mixer.toggle_pause();
                        }
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }
                ControlCmd::Pause => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        if !mixer.snapshot(0).paused {
                            mixer.toggle_pause();
                        }
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }
                ControlCmd::TogglePause => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        mixer.toggle_pause();
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }
                ControlCmd::Next => {
                    if let Some(i) = self.player.mixer.lock().ok().and_then(|m| m.next_index()) {
                        self.play_index(i, true);
                    }
                }
                ControlCmd::Prev => {
                    if let Some(i) = self.player.mixer.lock().ok().and_then(|m| m.prev_index()) {
                        self.play_index(i, true);
                    }
                }
                ControlCmd::PlayIndex(idx) => {
                    if idx < self.tracks.len() {
                        self.play_index(idx, true);
                    }
                }
                ControlCmd::PlayPath(path) => {
                    if let Some(idx) = self
                        .tracks
                        .iter()
                        .position(|t| t.path == std::path::Path::new(&path))
                    {
                        self.play_index(idx, true);
                    } else {
                        self.status = format!("path not found: {path}");
                    }
                }

                // ── 跳转 ──
                ControlCmd::Seek(secs) => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        mixer.seek_by(secs);
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }
                ControlCmd::SeekTo(secs) => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        mixer.seek_to(secs);
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }

                // ── 音量 ──
                ControlCmd::Volume(delta) => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        mixer.bump_volume(delta);
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }
                ControlCmd::SetVolume(v) => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        let current = mixer.snapshot(0).volume;
                        mixer.bump_volume(v - current);
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }

                // ── 混音模式 ──
                ControlCmd::CycleMix => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        mixer.cycle_mix();
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }
                ControlCmd::SetMix(mode) => {
                    use crate::dsp::MixMode;
                    let target = match mode.to_lowercase().as_str() {
                        "cut" => Some(MixMode::Cut),
                        "crossfade" => Some(MixMode::Crossfade),
                        "automix" => Some(MixMode::AutoMix),
                        _ => None,
                    };
                    if let Some(target) = target {
                        if let Ok(mut mixer) = self.player.mixer.lock() {
                            while mixer.mix() != target {
                                mixer.cycle_mix();
                            }
                            self.status = mixer.snapshot(self.selected()).status;
                        }
                    }
                }

                // ── Remix 模式 ──
                ControlCmd::CycleRemix => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        mixer.cycle_remix();
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }
                ControlCmd::SetRemix(mode) => {
                    use crate::remix::RemixMode;
                    let target = match mode.to_lowercase().as_str() {
                        "off" | "raw" => Some(RemixMode::Off),
                        "chill" => Some(RemixMode::Chill),
                        "club" => Some(RemixMode::Club),
                        "ncore" | "nightcore" => Some(RemixMode::Nightcore),
                        _ => None,
                    };
                    if let Some(target) = target {
                        if let Ok(mut mixer) = self.player.mixer.lock() {
                            while mixer.remix() != target {
                                mixer.cycle_remix();
                            }
                            self.status = mixer.snapshot(self.selected()).status;
                        }
                    }
                }

                // ── 随机播放 ──
                ControlCmd::ToggleShuffle => {
                    let next = {
                        let mixer = self.player.mixer.lock().expect("mixer");
                        mixer.shuffle_mode().next()
                    };
                    self.apply_shuffle(next);
                }
                ControlCmd::SetShuffle(mode) => {
                    use crate::prefs::ShuffleMode;
                    let target = match mode.to_lowercase().as_str() {
                        "off" => Some(ShuffleMode::Off),
                        "random" => Some(ShuffleMode::Random),
                        "norepeat" | "no_repeat" | "no-repeat" => Some(ShuffleMode::NoRepeat),
                        "taste" => Some(ShuffleMode::Taste),
                        _ => None,
                    };
                    if let Some(target) = target {
                        self.apply_shuffle(target);
                    }
                }

                // ── 循环模式 ──
                ControlCmd::CycleLoop => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        mixer.cycle_loop();
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }
                ControlCmd::SetLoop(mode) => {
                    use crate::engine::LoopMode;
                    let target = match mode.to_lowercase().as_str() {
                        "off" => Some(LoopMode::Off),
                        "one" | "1" => Some(LoopMode::One),
                        "all" => Some(LoopMode::All),
                        _ => None,
                    };
                    if let Some(target) = target {
                        if let Ok(mut mixer) = self.player.mixer.lock() {
                            while mixer.snapshot(0).loop_mode != target {
                                mixer.cycle_loop();
                            }
                            self.status = mixer.snapshot(self.selected()).status;
                        }
                    }
                }

                // ── 选中 ──
                ControlCmd::Select(idx) => {
                    if idx < self.visible_tracks().len() {
                        self.list_state.select(Some(idx));
                    }
                }

                // ── 资料库 ──
                ControlCmd::RescanLibrary => {
                    self.rescan_library();
                }
                ControlCmd::MetaScan => {
                    let n = self.enqueue_missing_meta();
                    self.pump_meta_queue();
                    self.status = if n == 0 {
                        "meta scan idle".into()
                    } else {
                        format!("meta queue {n}")
                    };
                }

                // ── EQ ──
                ControlCmd::SetEq(band, db) => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        mixer.bump_eq(band, db);
                        self.prefs.eq_db = mixer.eq_db();
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }
                ControlCmd::ResetEq => {
                    if let Ok(mut mixer) = self.player.mixer.lock() {
                        mixer.reset_eq();
                        self.prefs.eq_db = mixer.eq_db();
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }

                // ── 排序 ──
                ControlCmd::SetSort(mode) => {
                    use crate::prefs::SortMode;
                    let target = match mode.to_lowercase().as_str() {
                        "path" => Some(SortMode::Path),
                        "title" => Some(SortMode::Title),
                        "artist" => Some(SortMode::Artist),
                        "album" => Some(SortMode::Album),
                        _ => None,
                    };
                    if let Some(target) = target {
                        let keep = self.selected();
                        self.prefs.sort_mode = target;
                        self.prefs.save();
                        self.sync_list_cursor(keep);
                        self.status = format!("sort {}", self.prefs.sort_mode.label());
                    }
                }

                // ── 主题/可视化/偏好 ──
                ControlCmd::SetTheme(mode) => {
                    use crate::prefs::ThemeName;
                    let target = match mode.to_lowercase().as_str() {
                        "system" => Some(ThemeName::System),
                        "latte" => Some(ThemeName::Latte),
                        "frappe" => Some(ThemeName::Frappe),
                        "macchiato" => Some(ThemeName::Macchiato),
                        "mocha" => Some(ThemeName::Mocha),
                        _ => None,
                    };
                    if let Some(target) = target {
                        self.prefs.theme = target;
                        self.prefs.save();
                        self.status = format!("theme {}", self.prefs.theme.label());
                    }
                }
                ControlCmd::SetVisualize(mode) => {
                    use crate::prefs::VisualizeMode;
                    let target = match mode.to_lowercase().as_str() {
                        "off" => Some(VisualizeMode::Off),
                        "bars" => Some(VisualizeMode::Bars),
                        "scope" | "oscilloscope" => Some(VisualizeMode::Oscilloscope),
                        "cnm" => Some(VisualizeMode::Cnm),
                        _ => None,
                    };
                    if let Some(target) = target {
                        self.prefs.visualize = target;
                        self.prefs.save();
                        self.status = format!("viz {}", self.prefs.visualize.label());
                    }
                }
                ControlCmd::ToggleTransparent => {
                    self.prefs.transparent = !self.prefs.transparent;
                    self.prefs.save();
                    self.status = format!("transparent {}", self.prefs.transparent);
                }
                ControlCmd::ToggleLyricsFetch => {
                    self.prefs.lyrics_fetch = !self.prefs.lyrics_fetch;
                    self.prefs.save();
                    self.status = format!("lyrics_fetch {}", self.prefs.lyrics_fetch);
                }
                ControlCmd::ToggleCoverFetch => {
                    self.prefs.cover_fetch = !self.prefs.cover_fetch;
                    self.prefs.save();
                    self.status = format!("cover_fetch {}", self.prefs.cover_fetch);
                }
                ControlCmd::ToggleResume => {
                    self.prefs.resume = !self.prefs.resume;
                    self.prefs.save();
                    self.status = format!("resume {}", self.prefs.resume);
                }

                // ── 保留兼容 ──
                ControlCmd::Import(url) => self.start_import(&url),
                ControlCmd::Search(query) => self.search_splayer(&query),
                ControlCmd::Status => {}
            }
        }
    }

    fn drain_mpris(&mut self) {
        for cmd in self.mpris.poll() {
            match cmd {
                MediaCmd::PlayPause => {
                    let mut mixer = self.player.mixer.lock().expect("mixer");
                    if mixer.snapshot(0).current.is_none() {
                        drop(mixer);
                        let i = self.selected();
                        self.play_index(i, false);
                    } else {
                        mixer.toggle_pause();
                        self.status = mixer.snapshot(self.selected()).status;
                    }
                }
                MediaCmd::Next => {
                    let next = self.player.mixer.lock().expect("mixer").next_index();
                    if let Some(i) = next {
                        self.play_index(i, true);
                    }
                }
                MediaCmd::Previous => {
                    let prev = self.player.mixer.lock().expect("mixer").prev_index();
                    if let Some(i) = prev {
                        self.play_index(i, true);
                    }
                }
                MediaCmd::Stop => {
                    let mut mixer = self.player.mixer.lock().expect("mixer");
                    if !mixer.snapshot(0).paused {
                        mixer.toggle_pause();
                    }
                    self.status = "paused".into();
                }
                MediaCmd::Seek(secs) => {
                    let mut mixer = self.player.mixer.lock().expect("mixer");
                    mixer.seek_by(secs);
                    self.status = mixer.snapshot(self.selected()).status;
                }
            }
        }
    }

    fn publish_mpris(&mut self) {
        let snap = self
            .player
            .mixer
            .lock()
            .expect("mixer")
            .snapshot(self.selected());
        let meta = snap
            .current
            .and_then(|i| self.metas.get(i).and_then(|m| m.as_ref()));
        let cover = meta.and_then(|m| m.cover_path.as_deref());
        self.mpris.publish(&snap, meta, cover);
    }

    fn current_meta(&self, snap: &Snapshot) -> Option<&TrackMeta> {
        snap.current
            .and_then(|i| self.metas.get(i).and_then(|m| m.as_ref()))
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let snap = self
            .player
            .mixer
            .lock()
            .expect("mixer")
            .snapshot(self.selected());
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(2),
                Constraint::Min(8),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
            .split(frame.area());
        self.draw_header(frame, chunks[0], &snap);
        self.draw_transfer(frame, chunks[1]);
        match self.tab {
            Tab::Music => self.draw_music(frame, chunks[2], &snap),
            Tab::Player => self.draw_player(frame, chunks[2], &snap),
            Tab::Me => self.draw_me(frame, chunks[2]),
        }
        self.draw_now(frame, chunks[3], &snap);
        self.draw_help(frame, chunks[4]);
        if self.overlay != Overlay::None {
            self.draw_overlay_scrim(frame);
        }
        match self.overlay {
            Overlay::Settings => self.draw_settings(frame),
            Overlay::Library => self.draw_library(frame),
            Overlay::Help => self.draw_help_modal(frame),
            Overlay::Eq => self.draw_eq(frame),
            Overlay::None => {}
        }
    }

    fn draw_music(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect, snap: &Snapshot) {
        if !self.search_hits.is_empty() {
            self.draw_search(frame, area);
            return;
        }
        match self.music_mode {
            MusicMode::Library => self.draw_list(frame, area, snap),
            MusicMode::Artists => self.draw_artists(frame, area),
        }
    }

    fn draw_artists(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let pages = self.group_pages();
        let items: Vec<ListItem> = pages
            .iter()
            .enumerate()
            .map(|(i, page)| {
                let mark = if i == self.artist_idx { "▸ " } else { "  " };
                ListItem::new(format!("{mark}{}  {} tracks", page.name, page.tracks.len()))
            })
            .collect();
        if self.artist_state.selected().is_none() && !pages.is_empty() {
            self.artist_state
                .select(Some(self.artist_idx.min(pages.len() - 1)));
        }
        let pal = self.palette();
        let list = List::new(items)
            .block(
                Block::default()
                    .title(format!(" {} · 1列表 2作者 ", self.browse_kind.label()))
                    .borders(Borders::ALL)
                    .border_style(pal.dim_style()),
            )
            .style(pal.text_style())
            .highlight_style(pal.highlight())
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, area, &mut self.artist_state);
    }

    fn draw_player(&self, frame: &mut ratatui::Frame<'_>, area: Rect, snap: &Snapshot) {
        let meta = self.current_meta(snap);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(7)])
            .split(area);
        if self.show_lyrics {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
                .split(rows[0]);
            self.draw_cover_only(frame, cols[0], snap);
            self.draw_lyrics(frame, cols[1], snap, meta);
        } else {
            self.draw_cover_only(frame, rows[0], snap);
        }
        if self.prefs.visualize != crate::prefs::VisualizeMode::Off {
            self.draw_spectrum(frame, rows[1], snap);
        }
    }

    fn draw_cover_only(&self, frame: &mut ratatui::Frame<'_>, area: Rect, snap: &Snapshot) {
        let meta = self.current_meta(snap);
        let artist = meta
            .map(|m| m.artist.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown");
        let album = meta
            .map(|m| m.album.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("—");
        let inner = area.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        });
        let cover_lines = letterbox_cover(
            meta.and_then(|m| m.cover.as_ref()),
            inner.width,
            inner.height.saturating_sub(1),
        );
        let cover = Paragraph::new(cover_lines)
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title(format!(" {artist} / {album} "))
                    .borders(Borders::ALL)
                    .border_style(Style::default().magenta()),
            );
        frame.render_widget(cover, area);
    }

    fn draw_me(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let pal = self.palette();
        let listened = format_listen_time(self.taste.total_listen_secs());
        let mut lines = vec![
            Line::from(vec![
                Span::styled("听歌时长  ", pal.dim_style()),
                Span::styled(listened, pal.peak_style().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(""),
            Line::from(Span::styled("最爱曲目", pal.text_style())),
        ];
        let tracks = self.taste.top_tracks(6);
        if tracks.is_empty() {
            lines.push(Line::from(Span::styled(
                "还没有完整听完的歌",
                pal.dim_style(),
            )));
        } else {
            for (i, (title, artist, score)) in tracks.iter().enumerate() {
                lines.push(Line::from(Span::styled(
                    format!(" {:>2}  {title}  {artist}  {score:.1}", i + 1),
                    pal.text_style(),
                )));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("最爱歌手", pal.text_style())));
        let artists = self.taste.top_artists(5);
        if artists.is_empty() {
            lines.push(Line::from(Span::styled(
                "听一会儿就会出现",
                pal.dim_style(),
            )));
        } else {
            for (name, score) in artists {
                lines.push(Line::from(Span::styled(
                    format!("  {name}  {score:.1}"),
                    pal.text_style(),
                )));
            }
        }
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(pal.dim_style()),
            ),
            area,
        );
    }

    fn draw_header(&self, frame: &mut ratatui::Frame<'_>, area: Rect, snap: &Snapshot) {
        let pal = self.palette();
        let mix = match snap.mix {
            MixMode::Cut => {
                Span::styled("CUT", Style::default().red().add_modifier(Modifier::BOLD))
            }
            MixMode::Crossfade => {
                Span::styled("FADE", Style::default().cyan().add_modifier(Modifier::BOLD))
            }
            MixMode::AutoMix => Span::styled(
                "AUTO",
                Style::default().magenta().add_modifier(Modifier::BOLD),
            ),
        };
        let remix = match snap.remix {
            RemixMode::Off => Span::styled("RAW", pal.dim_style().add_modifier(Modifier::BOLD)),
            RemixMode::Chill => Span::styled(
                "CHILL",
                Style::default().blue().add_modifier(Modifier::BOLD),
            ),
            RemixMode::Club => Span::styled(
                "CLUB",
                Style::default().yellow().add_modifier(Modifier::BOLD),
            ),
            RemixMode::Nightcore => Span::styled(
                "NCORE",
                Style::default().magenta().add_modifier(Modifier::BOLD),
            ),
        };
        let state = if self.decoding {
            Span::styled("DEC", Style::default().yellow())
        } else if snap.paused {
            Span::styled("PAUSE", pal.dim_style())
        } else if snap.fading {
            Span::styled("MIX", Style::default().magenta())
        } else {
            Span::styled("LIVE", Style::default().green())
        };
        let mut tabs = Vec::new();
        for tab in Tab::all() {
            let active = tab == self.tab;
            tabs.push(Span::styled(
                format!(" {} ", tab.label()),
                if active {
                    pal.peak_style()
                        .fg(Color::Black)
                        .bg(Palette::rgb(pal.peak))
                        .add_modifier(Modifier::BOLD)
                } else {
                    pal.dim_style()
                },
            ));
            tabs.push(Span::raw(" "));
        }
        let mut title_spans = vec![Span::styled(" ZRADIO ", pal.highlight()), "  ".into()];
        title_spans.append(&mut tabs);
        title_spans.extend([
            state,
            "  ".into(),
            mix,
            " ".into(),
            remix,
            "  ".into(),
            match self.prefs.shuffle_mode {
                ShuffleMode::Off => Span::styled("SEQ", pal.dim_style()),
                ShuffleMode::Random => Span::styled("SHUF", Style::default().yellow()),
                ShuffleMode::NoRepeat => Span::styled("NOREP", Style::default().yellow()),
                ShuffleMode::Taste => Span::styled("TASTE", Style::default().yellow()),
            },
            "  ".into(),
            if self.login.logged_in {
                if self.login.vip {
                    Span::styled("VIP", Style::default().yellow())
                } else {
                    Span::styled(self.login.name.clone(), Style::default().green())
                }
            } else {
                Span::styled("ANON", Style::default().red())
            },
        ]);
        let title = Paragraph::new(ratatui::text::Line::from(title_spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().cyan()),
        );
        frame.render_widget(title, area);
    }

    fn visible_tracks(&self) -> Vec<usize> {
        if !self.local_filter.is_empty() {
            self.local_hits.clone()
        } else {
            sort_indices(&self.tracks, &self.metas, self.prefs.sort_mode)
        }
    }

    fn draw_list(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect, snap: &Snapshot) {
        let metas: Vec<String> = {
            let mixer = self.player.mixer.lock().expect("mixer");
            self.tracks
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    mixer
                        .analysis_at(i)
                        .map(|a| {
                            format!(
                                "  {} {}",
                                a.bpm
                                    .map(|b| format!("{b:.0}"))
                                    .unwrap_or_else(|| "--".into()),
                                a.camelot.map(|c| c.label()).unwrap_or_else(|| "--".into())
                            )
                        })
                        .unwrap_or_default()
                })
                .collect()
        };
        let visible = self.visible_tracks();
        let items: Vec<ListItem> = visible
            .iter()
            .filter_map(|&i| self.tracks.get(i).map(|track| (i, track)))
            .map(|(i, track)| {
                let mark = if snap.current == Some(i) {
                    "▸ "
                } else {
                    "  "
                };
                let artist = self
                    .metas
                    .get(i)
                    .and_then(|m| m.as_ref())
                    .map(|m| m.artist.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("");
                let text = if artist.is_empty() {
                    format!(
                        "{mark}{}{}",
                        track.title,
                        metas.get(i).cloned().unwrap_or_default()
                    )
                } else {
                    format!(
                        "{mark}{artist} — {}{}",
                        track.title,
                        metas.get(i).cloned().unwrap_or_default()
                    )
                };
                let mut item = ListItem::new(text);
                if snap.current == Some(i) {
                    item = item.style(Style::default().magenta());
                }
                item
            })
            .collect();
        let pal = self.palette();
        let list = List::new(items)
            .block(
                Block::default()
                    .title(if self.local_filter.is_empty() {
                        format!(
                            " 曲库 · {} · {} · {} ",
                            self.browse_kind.label(),
                            self.prefs.sort_mode.label_zh(),
                            self.prefs.shuffle_mode.label_zh()
                        )
                    } else {
                        format!(
                            " {} · {} · {} ",
                            self.local_filter,
                            self.local_hits.len(),
                            self.prefs.sort_mode.label_zh()
                        )
                    })
                    .borders(Borders::ALL)
                    .border_style(pal.dim_style()),
            )
            .style(pal.text_style())
            .highlight_style(pal.highlight())
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_lyrics(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        snap: &Snapshot,
        meta: Option<&TrackMeta>,
    ) {
        let inner = area.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        });
        let secs = if snap.sample_rate == 0 {
            0.0
        } else {
            snap.position_frames as f32 / snap.sample_rate as f32
        };
        let lyrics = meta.map(|m| m.lyrics.as_slice()).unwrap_or(&[]);
        let rows = inner.height.max(1) as usize;
        let view = lyric_window(lyrics, secs, rows);
        let pal = self.palette();
        let lines: Vec<Line> = if view.lines.is_empty() {
            let pad = rows.saturating_sub(1) / 2;
            let mut out = vec![Line::from(""); pad];
            out.push(
                Line::from(Span::styled("no lyrics", pal.dim_style())).alignment(Alignment::Center),
            );
            out
        } else {
            view.lines
                .iter()
                .map(|line| {
                    if line.current {
                        let fill = lyric_fill(&line.text, lyric_progress(lyrics, secs));
                        Line::from(vec![
                            Span::styled(fill.done, pal.peak_style().add_modifier(Modifier::BOLD)),
                            Span::styled(fill.rest, pal.dim_style()),
                        ])
                        .alignment(Alignment::Center)
                    } else {
                        let style = if line.distance == 1 {
                            pal.text_style()
                        } else {
                            pal.dim_style()
                        };
                        Line::from(Span::styled(line.text.clone(), style))
                            .alignment(Alignment::Center)
                    }
                })
                .collect()
        };
        let lyric = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(pal.dim_style()),
        );
        frame.render_widget(lyric, area);
    }

    fn draw_spectrum(&self, frame: &mut ratatui::Frame<'_>, area: Rect, snap: &Snapshot) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let pal = self.palette();
        let spec = crate::spectrum::Spectrum::from_levels(snap.spectrum_levels);
        let mode = self.prefs.visualize;
        let rows = match mode {
            crate::prefs::VisualizeMode::Oscilloscope => spec.scope_sized(area.height, area.width),
            crate::prefs::VisualizeMode::Cnm => spec.cnm_sized(area.height, area.width),
            _ => spec.bars_sized(area.height, area.width),
        };
        let peak = spec.peak();
        let h = area.height as usize;
        let buf = frame.buffer_mut();
        for (row, line) in rows.iter().enumerate() {
            let y = area.y + row as u16;
            let t = if h <= 1 {
                1.0
            } else {
                row as f32 / (h - 1) as f32
            };
            let cnm_fg = mix_rgb(pal.accent, pal.peak, t);
            let from_bottom = h.saturating_sub(1).saturating_sub(row);
            for (col, ch) in line.chars().enumerate() {
                let x = area.x + col as u16;
                let color = if ch == ' ' {
                    Color::Reset
                } else if mode == crate::prefs::VisualizeMode::Cnm {
                    Color::Rgb(cnm_fg.0, cnm_fg.1, cnm_fg.2)
                } else if mode == crate::prefs::VisualizeMode::Oscilloscope {
                    if ch == '━' || ch == '─' {
                        Palette::rgb(pal.peak)
                    } else {
                        Palette::rgb(pal.accent)
                    }
                } else if peak > 0.72 && from_bottom + 1 >= (peak * h as f32 * 1.8) as usize {
                    Palette::rgb(pal.peak)
                } else if from_bottom <= 1 {
                    Palette::rgb(pal.dim)
                } else {
                    Palette::rgb(pal.accent)
                };
                buf[(x, y)].set_char(ch).set_fg(color).set_bg(Color::Reset);
            }
        }
    }

    fn draw_now(&self, frame: &mut ratatui::Frame<'_>, area: Rect, snap: &Snapshot) {
        let ratio = if snap.duration_frames == 0 {
            0.0
        } else {
            snap.position_frames as f64 / snap.duration_frames as f64
        };
        let now = snap
            .current
            .and_then(|i| self.tracks.get(i))
            .map(|t| t.title.as_str())
            .unwrap_or("—");
        let bpm = snap
            .current_bpm
            .map(|b| format!("{b:.0}"))
            .unwrap_or_else(|| "--".into());
        let key = snap.current_key.clone().unwrap_or_else(|| "--".into());
        let hint = snap.next_hint.clone().unwrap_or_default();
        let label = format!(
            " {}  {} {}  {} / {}  {} ",
            now,
            bpm,
            key,
            fmt_time(snap.position_frames, snap.sample_rate),
            fmt_time(snap.duration_frames, snap.sample_rate),
            hint
        );
        let gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" now "))
            .gauge_style(Style::default().cyan())
            .ratio(ratio.clamp(0.0, 1.0))
            .label(label);
        frame.render_widget(gauge, area);
    }

    fn draw_search(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let items: Vec<ListItem> = self
            .search_hits
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                let mark = if i == self.search_idx { "▸ " } else { "  " };
                let text = format!("{mark}{}  {} — {}", hit.source, hit.artist, hit.title);
                let mut item = ListItem::new(text);
                if i == self.search_idx {
                    item = item.style(self.palette().highlight());
                }
                item
            })
            .collect();
        let list = List::new(items).block(
            Block::default()
                .title(" splayer search · enter download · esc close ")
                .borders(Borders::ALL)
                .border_style(Style::default().cyan()),
        );
        frame.render_widget(list, area);
    }

    fn draw_transfer(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let text = if let Some(cmd) = self.command.as_ref() {
            format!(":{cmd}")
        } else if let Some(job) = self.job.as_ref() {
            job.clone()
        } else {
            self.status.clone()
        };
        let label = progress::bar_label(self.job_ratio, &text);
        frame.render_widget(
            TransferBar {
                ratio: self.job_ratio,
                label: &label,
                active: self.job.is_some()
                    && self.job_ratio < 1.0
                    && !self.status.starts_with("fail"),
                pal: self.palette(),
            },
            area,
        );
    }

    fn draw_help(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let help = Paragraph::new(if self.command.is_some() {
            format!(":{}", self.command.clone().unwrap_or_default())
        } else {
            match self.tab {
                Tab::Music => "q/e tab  1曲库 2作者  /find  y库  t设置  ^k帮助  ^q退出".into(),
                Tab::Player => {
                    "q/e tab  l歌词  g EQ  space  n/p  ↑↓音量  t设置  ^k帮助  ^q退出".into()
                }
                Tab::Me => "q/e tab  听歌时长/最爱  t设置  ^k帮助  ^q退出".into(),
            }
        })
        .alignment(Alignment::Center)
        .style(self.palette().dim_style());
        frame.render_widget(help, area);
    }

    fn draw_overlay_scrim(&self, frame: &mut ratatui::Frame<'_>) {
        if self.overlay == Overlay::Library {
            return;
        }
        let pal = self.palette();
        let area = overlay_scrim(frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default().style(Style::default().bg(pal.scrim())),
            area,
        );
    }

    fn overlay_block<'a>(&self, title: &'a str, border: Style) -> Block<'a> {
        let pal = self.palette();
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(border)
            .style(Style::default().bg(pal.panel()).fg(Palette::rgb(pal.text)))
    }

    fn draw_settings(&self, frame: &mut ratatui::Frame<'_>) {
        let rows = [
            format!("主题        {}", self.prefs.theme.label()),
            format!("透明背景    {}", on_off(self.prefs.transparent)),
            format!("可视化      {}", self.prefs.visualize.label()),
            format!("拉歌词      {}", on_off(self.prefs.lyrics_fetch)),
            format!("拉封面      {}", on_off(self.prefs.cover_fetch)),
            format!("续播        {}", on_off(self.prefs.resume)),
            "关闭".into(),
        ];
        let items: Vec<ListItem> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let mark = if i == self.settings_idx { "▸ " } else { "  " };
                ListItem::new(format!("{mark}{row}"))
            })
            .collect();
        let area = centered(frame.area(), 42, 12);
        let pal = self.palette();
        frame.render_widget(Clear, area);
        frame.render_widget(
            List::new(items)
                .block(self.overlay_block(" settings ", Style::default().yellow()))
                .style(pal.text_style())
                .highlight_style(pal.highlight()),
            area,
        );
    }

    fn draw_library(&self, frame: &mut ratatui::Frame<'_>) {
        let queued = self.meta_queue.len() + self.meta_inflight.len();
        let tagged = browse::tagged_indices(&self.metas).len();
        let untagged = self.tracks.len().saturating_sub(tagged);
        let rows = [
            format!("排序        {}", self.prefs.sort_mode.label_zh()),
            format!("随机        {}", self.prefs.shuffle_mode.label_zh()),
            format!(
                "分类        {}  {}/{}",
                self.browse_kind.label(),
                tagged,
                untagged
            ),
            format!(
                "扫元数据    {}",
                if queued == 0 {
                    "6路并行".into()
                } else {
                    format!("排队 {queued}")
                }
            ),
            "关闭".into(),
        ];
        let items: Vec<ListItem> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let mark = if i == self.library_idx { "▸ " } else { "  " };
                ListItem::new(format!("{mark}{row}"))
            })
            .collect();
        let area = centered(frame.area(), 36, 10);
        let pal = self.palette();
        frame.render_widget(Clear, area);
        frame.render_widget(
            List::new(items)
                .block(self.overlay_block(" library ", Style::default().yellow()))
                .style(pal.text_style())
                .highlight_style(pal.highlight()),
            area,
        );
    }

    fn draw_help_modal(&self, frame: &mut ratatui::Frame<'_>) {
        let text = "q/e 切栏   1曲库 2作者   / 元数据检索\n空格 播放暂停   n/p 下一首上一首\nl 歌词   g 均衡器   t 设置   y 曲库\nEsc 关搜索/弹窗   Ctrl+F 打开文件夹   Ctrl+K 帮助\nCtrl+Q 退出   +/- 音量   ←→ 快进快退";
        let area = centered(frame.area(), 52, 10);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(text)
                .style(self.palette().text_style())
                .block(self.overlay_block(" keys ", Style::default().cyan())),
            area,
        );
    }

    fn draw_eq(&self, frame: &mut ratatui::Frame<'_>) {
        let db = self
            .player
            .mixer
            .lock()
            .map(|m| m.eq_db())
            .unwrap_or([0.0; 5]);
        let mut lines = vec![Line::from("←/→ 调  r 重置  esc 关")];
        for (i, (label, gain)) in Equalizer::labels().iter().zip(db).enumerate() {
            let mark = if i == self.eq_band { "▸" } else { " " };
            lines.push(Line::from(format!("{mark} {label:>4}  {gain:+4.0} dB")));
        }
        let area = centered(frame.area(), 28, 10);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines)
                .style(self.palette().text_style())
                .block(self.overlay_block(" eq ", Style::default().magenta())),
            area,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscOutcome {
    CloseSearch,
    CloseLocal,
    Dismiss,
}

fn esc_outcome(search_open: bool, local_open: bool) -> EscOutcome {
    if search_open {
        EscOutcome::CloseSearch
    } else if local_open {
        EscOutcome::CloseLocal
    } else {
        EscOutcome::Dismiss
    }
}

fn visible_row(visible: &[usize], track_index: usize) -> Option<usize> {
    visible.iter().position(|&i| i == track_index)
}

fn on_off(v: bool) -> &'static str {
    if v {
        "on"
    } else {
        "off"
    }
}

fn mix_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    (
        (a.0 as f32 + (b.0 as f32 - a.0 as f32) * t) as u8,
        (a.1 as f32 + (b.1 as f32 - a.1 as f32) * t) as u8,
        (a.2 as f32 + (b.2 as f32 - a.2 as f32) * t) as u8,
    )
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn overlay_scrim(area: Rect) -> Rect {
    area
}

fn letterbox_cover(
    cover_art: Option<&cover::CoverArt>,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let lines = match cover_art {
        Some(c) => c.lines(width, height),
        None => cover::placeholder(width, height),
    };
    if lines.is_empty() {
        return lines;
    }
    let used_w = lines[0].spans.len() as u16;
    let used_h = lines.len() as u16;
    let pad_x = width.saturating_sub(used_w) / 2;
    let pad_y = height.saturating_sub(used_h) / 2;
    let mut out = vec![Line::from(" ".repeat(width as usize)); pad_y as usize];
    for mut line in lines {
        if pad_x > 0 {
            let mut spans = vec![ratatui::text::Span::raw(" ".repeat(pad_x as usize))];
            spans.append(&mut line.spans);
            out.push(Line::from(spans));
        } else {
            out.push(line);
        }
    }
    while (out.len() as u16) < height {
        out.push(Line::from(" ".repeat(width as usize)));
    }
    out
}

fn fmt_time(frames: usize, sample_rate: u32) -> String {
    if sample_rate == 0 {
        return "0:00".into();
    }
    let secs = frames as u64 / u64::from(sample_rate);
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_panel_is_smaller_than_scrim() {
        let frame = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let scrim = overlay_scrim(frame);
        let panel = centered(frame, 36, 10);
        assert_eq!(scrim, frame);
        assert!(panel.width < scrim.width);
        assert!(panel.height < scrim.height);
        assert!(panel.x > scrim.x);
        assert!(panel.y > scrim.y);
    }

    #[test]
    fn esc_does_not_quit() {
        assert_eq!(esc_outcome(true, false), EscOutcome::CloseSearch);
        assert_eq!(esc_outcome(false, true), EscOutcome::CloseLocal);
        assert_eq!(esc_outcome(false, false), EscOutcome::Dismiss);
    }

    #[test]
    fn filtered_list_cursor_uses_visible_row() {
        let visible = [4, 9, 12];
        assert_eq!(visible_row(&visible, 9), Some(1));
        assert_eq!(visible_row(&visible, 0), None);
    }
}
