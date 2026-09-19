use std::path::PathBuf;

use anyhow::Context;
use zradio::ui::App;

fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| zradio::prefs::Prefs::load().library_path());
    App::new(root).context("start zradio")?.run()
}
