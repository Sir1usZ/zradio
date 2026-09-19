use std::process::{Child, Command, Stdio};

use crate::vibe::Station;

#[derive(Default)]
pub struct RadioStream {
    child: Option<Child>,
    pub station: Option<Station>,
}

impl RadioStream {
    pub fn playing(&self) -> bool {
        self.child.is_some()
    }

    pub fn play(&mut self, station: Station) -> anyhow::Result<()> {
        self.stop();
        let child = Command::new("mpv")
            .args([
                "--no-video",
                "--really-quiet",
                "--volume=80",
                "--no-terminal",
                station.url,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.child = Some(child);
        self.station = Some(station);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.station = None;
    }

    pub fn poll(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                self.child = None;
            }
        }
    }
}

impl Drop for RadioStream {
    fn drop(&mut self) {
        self.stop();
    }
}
