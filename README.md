<div align="center">
  <img src="docs/logo.svg" alt="ZRadio" width="128" height="128" />
  <h1>ZRadio</h1>
  <p>Local-first TUI radio player. AutoMix, taste-weighted queue, lyrics, covers, and spectrum visualization.</p>
  <p>
    <a href="https://www.rust-lang.org/">
      <img alt="Rust" src="https://img.shields.io/badge/Rust-dea584?logo=rust&logoColor=black&style=for-the-badge" />
    </a>
    <a href="LICENSE">
      <img alt="MIT License" src="https://img.shields.io/github/license/Sir1usZ/zradio?style=for-the-badge" />
    </a>
    <a href="https://github.com/Sir1usZ/zradio/stargazers">
      <img alt="GitHub stars" src="https://img.shields.io/github/stars/Sir1usZ/zradio?style=for-the-badge" />
    </a>
    <a href="https://github.com/Sir1usZ/zradio">
      <img alt="GitHub last commit" src="https://img.shields.io/github/last-commit/Sir1usZ/zradio?style=for-the-badge" />
    </a>
  </p>
</div>

Default library: `~/音乐`.

## Build

```sh
cargo build --release
./target/release/zradio
```

Optional argument: a library directory.

## Keys

| Key | Action |
|---|---|
| `q` / `e` | Previous / next tab |
| `1` | All library (music tab) |
| `2`–`9` | Jump user playlist (empty slot refuses) |
| `←` / `→` | Cycle playlists (music) / seek 5s (player) |
| `x` | Context menu: highlighted track (music) / now playing (player) |
| `y` | Large library window (whole library, not play range) |
| `j` `k` or arrows | Move |
| Space | Play / pause |
| `n` / `p` | Next / previous (current list) |
| `l` | Lyrics (player tab) |
| `g` | EQ |
| `t` | Settings |
| `s` | Cycle shuffle |
| `/` | Find local |
| `?` | Search Netease/QQ via SPlayer |
| `o` | Loop |
| `m` | Mix mode |
| `r` | Remix |
| `:playlist 名字` | Create user list (max 8) |
| `:playlist-rm` | Delete current user list |
| `Ctrl+F` | Open folder |
| `Ctrl+K` | Help |
| `Esc` | Close search / overlay (does not quit) |
| `Ctrl+Q` | Quit |

Playlists live in `~/.local/share/zradio/playlists.json`. Slot `1` is the whole library; slots `2`–`9` are user lists. The current list is the play range for `n` / shuffle / loop. Missing files stay in the JSON, render gray, and are skipped.

Settings: theme, transparent background, visualize (`off` / `bars` / `scope` / `cnm`), fetch lyrics, fetch covers, resume.

## Visualize

- `bars` — packed columns
- `scope` — midline waveform
- `cnm` — gapped density bars with a baseline and vertical gradient (look inspired by CNMPlayer, independently implemented)

If `cava` is on `PATH`, bars use it; otherwise internal FFT.

## Optional

- `cava` for live spectrum
- SPlayer at `127.0.0.1:25884` for search/download
- `yt-dlp` for YouTube / Bilibili import (`:url`)

## Remote Control API

HTTP API on port `18765`. Control, status, and lyrics are JSON. `GET /cover/:index` returns image bytes on success. `/status` and `/library/full` do not include cover bytes or LRC text.

```bash
# status (no cover bytes)
curl -s http://localhost:18765/status | jq

# full LRC
curl -s http://localhost:18765/lyrics/3 | jq -r '.data.lrc'

# cover image
curl -s http://localhost:18765/cover/3 -o cover.jpg

# track info (cover as base64)
curl -s http://localhost:18765/track/3 | jq '.data.cover.mime'

# play
curl -X POST http://localhost:18765/control -d '{"action":"play"}'
```

Full API: [docs/API.md](docs/API.md).

## License

MIT. See [LICENSE](LICENSE).
