# ZRadio

Local-first TUI radio player. AutoMix, taste-weighted queue, lyrics, covers, and spectrum visualization.

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

HTTP JSON API on port `18765`，手机/浏览器/脚本可远程控制播放器。

```bash
# 查看状态
curl -s http://localhost:18765/status | jq

# 播放
curl -X POST http://localhost:18765/control -d '{"action":"play"}'

# 设置随机为口味加权
curl -X POST http://localhost:18765/control -d '{"action":"set_shuffle","query":"taste"}'
```

**[→ 完整 API 文档](docs/API.md)**

## License

MIT. See `LICENSE`.
