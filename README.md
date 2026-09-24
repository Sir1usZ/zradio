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

HTTP API on port `18765`。控制、状态、歌词是 JSON；`GET /cover/:index` 成功时是图片字节。`/status` 和 `/library/full` 不含封面或 LRC。

```bash
# 状态（无封面字节）
curl -s http://localhost:18765/status | jq

# 完整 LRC
curl -s http://localhost:18765/lyrics/3 | jq -r '.data.lrc'

# 封面原图
curl -s http://localhost:18765/cover/3 -o cover.jpg

# 曲目详情（含封面 base64）
curl -s http://localhost:18765/track/3 | jq '.data.cover.mime'

# 播放
curl -X POST http://localhost:18765/control -d '{"action":"play"}'
```

完整接口见 [docs/API.md](docs/API.md)。

## License

MIT. See `LICENSE`.
