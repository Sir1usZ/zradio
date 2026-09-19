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
| `1` / `2` | Library / artists |
| `j` `k` or arrows | Move |
| Space | Play / pause |
| `n` / `p` | Next / previous |
| `l` | Lyrics (player tab) |
| `g` | EQ |
| `t` | Settings |
| `/` | Find local |
| `?` | Search Netease/QQ via SPlayer |
| `o` | Loop |
| `m` | Mix mode |
| `r` | Remix |
| `v` | Vibe radio |
| `Ctrl+F` | Open folder |
| `Ctrl+K` | Help |
| `Ctrl+Q` | Quit |

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

## License

MIT. See `LICENSE`.
