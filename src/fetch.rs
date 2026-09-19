use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

pub fn looks_like_media_url(raw: &str) -> bool {
    let s = raw.trim();
    s.contains("youtube.com/")
        || s.contains("youtu.be/")
        || s.contains("bilibili.com/")
        || s.contains("b23.tv/")
        || s.contains("music.youtube.com/")
}

pub fn library_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("音乐")
}

fn ytdlp() -> Option<Command> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut candidates = vec![PathBuf::from("yt-dlp"), PathBuf::from("yt-dlp-git")];
    if let Some(home) = &home {
        candidates.insert(0, home.join(".local/bin/yt-dlp"));
    }
    for bin in candidates {
        if bin.as_os_str() == "yt-dlp" || bin.as_os_str() == "yt-dlp-git" {
            if which(&bin.to_string_lossy()) {
                return Some(with_path(Command::new(bin)));
            }
        } else if bin.is_file() {
            return Some(with_path(Command::new(bin)));
        }
    }
    if Command::new("python3")
        .args(["-c", "import yt_dlp"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        let mut cmd = Command::new("python3");
        cmd.args(["-m", "yt_dlp"]);
        return Some(with_path(cmd));
    }
    None
}

fn with_path(mut cmd: Command) -> Command {
    if let Some(home) = std::env::var_os("HOME") {
        let extra = PathBuf::from(home).join(".local/bin");
        let path = std::env::var("PATH").unwrap_or_default();
        let merged = format!("{}:{path}", extra.display());
        cmd.env("PATH", merged);
    }
    cmd
}

fn which(bin: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {bin} >/dev/null")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn ytdlp_ready() -> bool {
    ytdlp().is_some()
}

pub fn download_audio(
    url: &str,
    dest: &Path,
    progress: Option<Sender<String>>,
) -> anyhow::Result<String> {
    let url = url.trim();
    if !looks_like_media_url(url) {
        anyhow::bail!("need a youtube or bilibili url");
    }
    let url = clean_media_url(url);
    let mut cmd =
        ytdlp().ok_or_else(|| anyhow::anyhow!("yt-dlp not found (~/.local/bin/yt-dlp)"))?;
    let template = dest.join("%(title)s.%(ext)s");
    cmd.arg("-x")
        .args(["--audio-format", "mp3"])
        .args(["--audio-quality", "0"])
        .arg("--no-playlist")
        .args(["-o", template.to_str().unwrap_or("%(title)s.%(ext)s")])
        .arg("--restrict-filenames")
        .arg("--newline")
        .arg("--progress")
        .args(["--js-runtimes", "node"])
        .args(["--extractor-args", "youtube:player_client=web,tv"]);
    if let Some(browser) = cookie_browser() {
        cmd.args(["--cookies-from-browser", browser]);
        if let Some(tx) = progress.as_ref() {
            let _ = tx.send(format!("cookies {browser}"));
        }
    }
    cmd.arg(&url);
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (meta_tx, meta_rx) = std::sync::mpsc::channel::<(Option<String>, Option<String>)>();
    if let Some(stderr) = child.stderr.take() {
        let progress = progress.clone();
        let meta_tx = meta_tx.clone();
        std::thread::spawn(move || {
            let mut last_path = None;
            let mut last_err = None;
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                last_err = Some(line.clone());
                if let Some(msg) = progress_line(&line) {
                    if let Some(tx) = progress.as_ref() {
                        let _ = tx.send(msg);
                    }
                } else if let Some(tx) = progress.as_ref() {
                    if line.contains("ERROR") || line.contains("Sign in") || line.contains("cookie")
                    {
                        let _ = tx.send(short_err(&line));
                    }
                }
                if let Some(path) = destination_line(&line) {
                    last_path = Some(path);
                }
            }
            let _ = meta_tx.send((last_path, last_err));
        });
    }
    if let Some(stdout) = child.stdout.take() {
        let meta_tx = meta_tx.clone();
        std::thread::spawn(move || {
            let mut last_path = None;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if looks_like_path(&line) {
                    last_path = Some(line.trim().to_string());
                }
            }
            let _ = meta_tx.send((last_path, None));
        });
    }
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if started.elapsed().as_secs() > 90 {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("yt-dlp hung. login youtube.com in Chrome, then retry");
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    };
    let mut last_path = String::new();
    let mut last_err = String::new();
    while let Ok((path, err)) = meta_rx.try_recv() {
        if let Some(path) = path {
            last_path = path;
        }
        if let Some(err) = err {
            last_err = err;
        }
    }
    if !status.success() {
        anyhow::bail!("{}", short_err(&last_err));
    }
    if last_path.is_empty() {
        anyhow::bail!("downloaded, but no file path returned");
    }
    Ok(last_path)
}

pub fn clean_media_url(url: &str) -> String {
    let url = url.trim();
    if let Some(id) = youtube_id(url) {
        return format!("https://www.youtube.com/watch?v={id}");
    }
    url.to_string()
}

fn youtube_id(url: &str) -> Option<String> {
    let url = url.trim();
    if let Some(rest) = url.split("v=").nth(1) {
        let id = rest.split('&').next()?.trim();
        if id.len() >= 8 {
            return Some(id.to_string());
        }
    }
    if let Some(rest) = url.split("youtu.be/").nth(1) {
        let id = rest.split(['?', '&', '/']).next()?.trim();
        if id.len() >= 8 {
            return Some(id.to_string());
        }
    }
    None
}

fn cookie_browser() -> Option<&'static str> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if home.join(".config/google-chrome/Default/Cookies").is_file() {
        return Some("chrome");
    }
    if home.join(".mozilla/firefox").is_dir() {
        return Some("firefox");
    }
    None
}

fn short_err(line: &str) -> String {
    let line = line.trim();
    if let Some(rest) = line.split("ERROR: ").nth(1) {
        return rest.chars().take(120).collect();
    }
    line.chars().take(120).collect()
}

pub fn parse_percent(msg: &str) -> Option<f64> {
    let msg = msg.trim();
    let pct = msg.split('%').next()?;
    let num = pct
        .rsplit(|c: char| !(c.is_ascii_digit() || c == '.'))
        .next()?;
    num.parse::<f64>().ok().map(|n| (n / 100.0).clamp(0.0, 1.0))
}

fn progress_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.contains("[download]") && line.contains('%') {
        return Some(line.trim_start_matches("[download]").trim().to_string());
    }
    if line.contains("[ExtractAudio]") {
        return Some("extract audio".into());
    }
    None
}

fn destination_line(line: &str) -> Option<String> {
    for prefix in [
        "[ExtractAudio] Destination: ",
        "[download] Destination: ",
        "Destination: ",
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(rest.trim().to_string());
        }
    }
    if line.contains(" has already been downloaded") {
        return line
            .split(" has already been downloaded")
            .next()
            .map(|s| s.trim().trim_start_matches("[download]").trim().to_string());
    }
    None
}

fn looks_like_path(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('/')
        && (line.ends_with(".mp3") || line.ends_with(".m4a") || line.ends_with(".opus"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_youtube_and_bilibili() {
        assert!(looks_like_media_url(
            "https://www.youtube.com/watch?v=tRsQsTMvPNg"
        ));
        assert!(looks_like_media_url("https://youtu.be/tRsQsTMvPNg"));
        assert!(looks_like_media_url(
            "https://www.bilibili.com/video/BV1xx411c7mD"
        ));
        assert!(looks_like_media_url("https://b23.tv/abcdef"));
        assert!(!looks_like_media_url("https://example.com/song"));
    }

    #[test]
    fn strips_youtube_playlist() {
        let dirty = "https://www.youtube.com/watch?v=QW_NiuNJvhs&list=RDQW_NiuNJvhs&start_radio=1";
        assert_eq!(
            clean_media_url(dirty),
            "https://www.youtube.com/watch?v=QW_NiuNJvhs"
        );
    }

    #[test]
    fn parse_download_progress() {
        assert_eq!(
            progress_line("[download]  12.3% of 4.10MiB at 1.20MiB/s ETA 00:03").as_deref(),
            Some("12.3% of 4.10MiB at 1.20MiB/s ETA 00:03")
        );
        assert_eq!(
            destination_line("[ExtractAudio] Destination: /home/xender/音乐/song.mp3").as_deref(),
            Some("/home/xender/音乐/song.mp3")
        );
        assert!((parse_percent("12.3% of 4.10MiB").unwrap() - 0.123).abs() < 0.001);
    }

    #[test]
    fn finds_user_local_ytdlp_path() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let Some(home) = home else { return };
        let bin = home.join(".local/bin/yt-dlp");
        if bin.is_file() {
            assert!(ytdlp_ready());
        }
    }
}
