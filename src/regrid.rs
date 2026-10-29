//! Rescoring tracks with Beat This! off the UI thread, one at a time, for the tracks about to
//! be mixed: the one playing and the one predicted next.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use crate::analysis::TrackAnalysis;

/// Whether this build can run the model.
pub const AVAILABLE: bool = cfg!(feature = "beat-this");
/// How many tracks may wait; older ones are dropped, the listener has moved past them.
const WAITING: usize = 4;

/// Tracks waiting to be scored, each tried once a session.
#[derive(Debug)]
pub struct Queue<Id> {
    waiting: VecDeque<Id>,
    busy: Option<Id>,
    tried: HashSet<PathBuf>,
}

impl<Id> Default for Queue<Id> {
    fn default() -> Self {
        Self {
            waiting: VecDeque::new(),
            busy: None,
            tried: HashSet::new(),
        }
    }
}

impl<Id: Clone + PartialEq> Queue<Id> {
    /// Puts the track playing first, ahead of everything waiting.
    pub fn push_front(&mut self, id: Id, path: &Path) {
        if self.busy.as_ref() == Some(&id) || self.tried.contains(path) {
            return;
        }
        self.waiting.retain(|w| *w != id);
        self.waiting.push_front(id);
        self.waiting.truncate(WAITING);
    }

    pub fn push_back(&mut self, id: Id, path: &Path) {
        let known = self.busy.as_ref() == Some(&id) || self.waiting.contains(&id);
        if known || self.tried.contains(path) || self.waiting.len() >= WAITING {
            return;
        }
        self.waiting.push_back(id);
    }

    /// The next track to score, unless one is being scored.
    pub fn take(&mut self) -> Option<Id> {
        if self.busy.is_some() {
            return None;
        }
        self.busy = self.waiting.pop_front();
        self.busy.clone()
    }

    /// Marks the track being scored done, whatever came of it.
    pub fn finish(&mut self, path: &Path) {
        self.busy = None;
        self.tried.insert(path.to_path_buf());
    }

    pub fn clear(&mut self) {
        self.waiting.clear();
    }
}

/// The cached analysis a decode may reuse: one Beat This! scored only while it is on, so
/// turned off, the track is analysed afresh, back on the onset tracker's grid.
pub fn reusable(cached: Option<TrackAnalysis>, beat_this: bool) -> Option<TrackAnalysis> {
    cached.filter(|a| beat_this || !a.beat_this)
}

/// The analysis Beat This! rescored after a decode read the cache, which the decode's must
/// not overwrite there.
pub fn rescored_since(
    stored: Option<TrackAnalysis>,
    decoded: &TrackAnalysis,
    beat_this: bool,
) -> Option<TrackAnalysis> {
    stored.filter(|a| beat_this && a.beat_this && !decoded.beat_this)
}

/// A track to score: its file, and its analysis so far when there is one.
pub struct Job<Id> {
    pub id: Id,
    pub path: PathBuf,
    pub base: Option<TrackAnalysis>,
}

pub struct Done<Id> {
    pub id: Id,
    pub path: PathBuf,
    pub analysis: Result<TrackAnalysis, String>,
}

/// One thread scoring the jobs sent to it in turn; it ends when the worker is dropped.
pub struct Worker<Id> {
    jobs: Sender<Job<Id>>,
    done: Receiver<Done<Id>>,
}

type Score = dyn Fn(&Path, Option<TrackAnalysis>) -> anyhow::Result<TrackAnalysis> + Send;

impl<Id: Send + 'static> Worker<Id> {
    pub fn spawn(score: Box<Score>) -> Self {
        let (jobs, rx) = mpsc::channel::<Job<Id>>();
        let (tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            for job in rx {
                let (path, base) = (job.path.clone(), job.base);
                // A model that panics on one track must not take the worker down with it.
                let scored =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| score(&path, base)));
                let analysis = match scored {
                    Ok(Ok(a)) => Ok(a),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("Beat This! crashed on this track".into()),
                };
                let done = Done {
                    id: job.id,
                    path: job.path,
                    analysis,
                };
                if tx.send(done).is_err() {
                    break;
                }
            }
        });
        Self { jobs, done }
    }

    pub fn send(&self, job: Job<Id>) -> bool {
        self.jobs.send(job).is_ok()
    }

    pub fn try_recv(&self) -> Option<Done<Id>> {
        self.done.try_recv().ok()
    }
}

/// The scorer the app runs, when this build has the model.
pub fn scorer(rate: u32, channels: usize) -> Option<Box<Score>> {
    #[cfg(feature = "beat-this")]
    {
        Some(Box::new(move |path: &Path, base: Option<TrackAnalysis>| {
            score_file(path, base, rate, channels, &crate::beat_this::model_path())
        }))
    }
    #[cfg(not(feature = "beat-this"))]
    {
        let _ = (rate, channels);
        None
    }
}

/// Decodes the file as the player would, scores it and fits a grid through the beats.
#[cfg(feature = "beat-this")]
fn score_file(
    path: &Path,
    base: Option<TrackAnalysis>,
    rate: u32,
    channels: usize,
    model: &Path,
) -> anyhow::Result<TrackAnalysis> {
    use crate::beat_this::{to_model_rate, Model};
    // Stamped before decoding, so a file changed meanwhile is not taken for this one.
    let (mtime, size) = crate::analysis::file_stamp(path)?;
    let buf = crate::engine::load_track(path, rate, channels)?;
    let mut base = base.unwrap_or_else(|| crate::analysis::analyze_buffer(&buf));
    (base.mtime, base.size) = (mtime, size);
    let mono = to_model_rate(&buf.samples, buf.channels, buf.sample_rate);
    drop(buf);
    let beats = Model::load(model)?.beats(&mono)?;
    Ok(base.with_grid(beats.grid()))
}

/// Fetches the model, reporting progress as "42%".
pub fn fetch_model(progress: Option<Sender<String>>) -> anyhow::Result<PathBuf> {
    #[cfg(feature = "beat-this")]
    {
        crate::beat_this::download_model(progress)
    }
    #[cfg(not(feature = "beat-this"))]
    {
        let _ = progress;
        anyhow::bail!("this build has no Beat This!")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn p(name: &str) -> PathBuf {
        PathBuf::from(format!("/music/{name}.flac"))
    }

    fn push(q: &mut Queue<&'static str>, front: bool, name: &'static str) {
        if front {
            q.push_front(name, &p(name));
        } else {
            q.push_back(name, &p(name));
        }
    }

    #[test]
    fn the_track_playing_is_scored_before_the_ones_waiting() {
        let mut q = Queue::default();
        push(&mut q, false, "next");
        push(&mut q, true, "now");
        push(&mut q, false, "next");
        assert_eq!(q.take(), Some("now"));
        // One at a time.
        assert_eq!(q.take(), None);
        q.finish(&p("now"));
        assert_eq!(q.take(), Some("next"));
        q.finish(&p("next"));
        assert_eq!(q.take(), None);
    }

    #[test]
    fn a_track_is_tried_once_and_never_while_it_is_being_scored() {
        let mut q = Queue::default();
        push(&mut q, true, "a");
        assert_eq!(q.take(), Some("a"));
        push(&mut q, true, "a");
        push(&mut q, false, "a");
        q.finish(&p("a"));
        assert_eq!(q.take(), None);
        // Its path, not its place in the library, is what was tried.
        q.push_front("a again", &p("a"));
        assert_eq!(q.take(), None);
    }

    #[test]
    fn tracks_skipped_past_fall_out_of_the_queue() {
        let mut q = Queue::default();
        for name in ["a", "b", "c", "d", "e", "f"] {
            push(&mut q, true, name);
        }
        push(&mut q, false, "g");
        let order: Vec<_> = std::iter::from_fn(|| {
            let id = q.take()?;
            q.finish(&p(id));
            Some(id)
        })
        .collect();
        assert_eq!(order, ["f", "e", "d", "c"]);
        q.push_front("x", &p("x"));
        q.clear();
        assert_eq!(q.take(), None);
    }

    fn plain() -> TrackAnalysis {
        crate::analysis::analyze_buffer(&crate::decode::AudioBuf {
            samples: Vec::new(),
            sample_rate: 44_100,
            channels: 2,
        })
    }

    fn scored() -> TrackAnalysis {
        plain().with_grid(None)
    }

    #[test]
    fn a_model_grid_is_reused_only_while_beat_this_is_on() {
        assert!(reusable(Some(plain()), true).is_some());
        assert!(reusable(Some(plain()), false).is_some());
        assert!(reusable(Some(scored()), true).is_some());
        assert!(reusable(Some(scored()), false).is_none());
        assert!(reusable(None, true).is_none());
    }

    #[test]
    fn a_decode_that_read_the_cache_first_does_not_undo_a_rescore() {
        let kept = rescored_since(Some(scored()), &plain(), true);
        assert!(kept.is_some_and(|a| a.beat_this));
        // Turned off, or nothing newer there: the decode's analysis is the one cached.
        assert!(rescored_since(Some(scored()), &plain(), false).is_none());
        assert!(rescored_since(Some(plain()), &plain(), true).is_none());
        assert!(rescored_since(Some(scored()), &scored(), true).is_none());
        assert!(rescored_since(None, &plain(), true).is_none());
    }

    fn wait<Id: Send + 'static>(worker: &Worker<Id>) -> Done<Id> {
        let start = Instant::now();
        loop {
            if let Some(done) = worker.try_recv() {
                return done;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the worker never answered"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_worker_scores_off_the_caller_s_thread_and_outlives_a_crash() {
        let caller = std::thread::current().id();
        let worker = Worker::spawn(Box::new(move |path: &Path, base| {
            assert_ne!(std::thread::current().id(), caller);
            match path.to_str() {
                Some("/crash") => panic!("tract fell over"),
                Some("/fail") => anyhow::bail!("no such file"),
                _ => Ok(base.expect("a base analysis")),
            }
        }));
        let job = |id: u32, path: &str, base| Job {
            id,
            path: PathBuf::from(path),
            base,
        };
        let base = || {
            crate::analysis::analyze_buffer(&crate::decode::AudioBuf {
                samples: Vec::new(),
                sample_rate: 44_100,
                channels: 2,
            })
        };
        assert!(worker.send(job(1, "/crash", None)));
        assert!(worker.send(job(2, "/fail", None)));
        assert!(worker.send(job(3, "/ok", Some(base()))));
        let crashed = wait(&worker);
        assert_eq!(crashed.id, 1);
        assert!(crashed.analysis.is_err());
        let failed = wait(&worker);
        assert_eq!(
            (failed.id, failed.analysis.unwrap_err()),
            (2, "no such file".into())
        );
        let scored = wait(&worker);
        assert_eq!((scored.id, scored.path), (3, PathBuf::from("/ok")));
        assert!(scored.analysis.is_ok());
    }

    /// Needs the model: ZRADIO_BEAT_THIS_MODEL=path/to/beat_this.onnx cargo test -- --ignored
    #[cfg(feature = "beat-this")]
    #[test]
    #[ignore]
    fn a_click_track_is_scored_from_its_file() {
        let Some(model) = std::env::var_os("ZRADIO_BEAT_THIS_MODEL") else {
            panic!("set ZRADIO_BEAT_THIS_MODEL to the model file");
        };
        // 120 BPM from 0.3 s, the first of every four beats louder, for 40 s.
        let rate = 44_100;
        let mut x = vec![0.0f32; 40 * rate as usize];
        for k in 0..80 {
            let start = ((0.3 + 0.5 * k as f64) * f64::from(rate)) as usize;
            let amp = if k % 4 == 0 { 0.9 } else { 0.5 };
            for (i, s) in x[start..].iter_mut().take(rate as usize / 5).enumerate() {
                let t = i as f64 / f64::from(rate);
                let kick = (std::f64::consts::TAU * 55.0 * t).sin() * (-t * 25.0).exp();
                let tick = (std::f64::consts::TAU * 1_200.0 * t).sin() * (-t * 120.0).exp();
                *s += (amp * (kick + 0.5 * tick)) as f32;
            }
        }
        let path = std::env::temp_dir().join(format!("zradio-regrid-{}.wav", std::process::id()));
        crate::mixdown::write_wav(&path, &x, rate, 1).unwrap();
        let a = score_file(&path, None, 48_000, 2, Path::new(&model));
        let stamp = crate::analysis::file_stamp(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let a = a.unwrap();
        assert!(a.beat_this);
        assert_eq!((a.mtime, a.size), stamp);
        let bpm = a.bpm.expect("a tempo");
        assert!((bpm - 120.0).abs() < 0.2, "{bpm} BPM");
        let off = (a.first_beat - 0.3).rem_euclid(0.5);
        assert!(off.min(0.5 - off) < 0.02, "first beat at {}", a.first_beat);
    }
}
