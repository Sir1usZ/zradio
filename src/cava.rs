use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct CavaFeed {
    child: Option<Child>,
    bars: Arc<Mutex<Vec<f32>>>,
}

impl CavaFeed {
    pub fn start(bars: usize) -> Option<Self> {
        let which = Command::new("sh")
            .args(["-lc", "command -v cava"])
            .output()
            .ok()?;
        if !which.status.success() {
            return None;
        }
        let n = bars.clamp(16, 96);
        let cfg = format!(
            "[general]\nframerate=30\nbars={n}\n[output]\nmethod=raw\nraw_target=/dev/stdout\ndata_format=ascii\nascii_max_range=100\nchannels=mono\n"
        );
        let mut child = Command::new("cava")
            .arg("-p")
            .arg("/dev/stdin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(cfg.as_bytes());
        }
        let stdout = child.stdout.take()?;
        let bars_state = Arc::new(Mutex::new(vec![0.0; n]));
        let shared = bars_state.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let parsed: Vec<f32> = line
                    .split(';')
                    .filter_map(|p| p.trim().parse::<f32>().ok())
                    .map(|v| (v / 100.0).clamp(0.0, 1.0))
                    .collect();
                if parsed.is_empty() {
                    continue;
                }
                if let Ok(mut g) = shared.lock() {
                    *g = parsed;
                }
            }
        });
        Some(Self {
            child: Some(child),
            bars: bars_state,
        })
    }

    pub fn latest(&self) -> Vec<f32> {
        self.bars.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl Drop for CavaFeed {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
