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

`cargo build --release --no-default-features` leaves out [Beat This!](#beat-this) and the [voice handoff](#voice-handoff); the binary drops from about 40 MB to 10 MB.

## Keys

| Key | Action |
|---|---|
| `q` / `e` | Previous / next tab |
| `p` / `Alt+P` | Toggle bottom playlist drawer |
| `1`–`9` | Jump slot inside the drawer (empty slot refuses) |
| `←` / `→` | Seek 5s (player tab) |
| `x` | Context menu: highlighted track (music) / now playing (player) |
| `y` | Large library window (whole library, not play range) |
| `j` `k` or arrows | Move |
| Space | Play / pause |
| `n` / `b` | Next / previous (current list) |
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

Playlists live in `~/.local/share/zradio/playlists.json`. Slot `1` is the whole library; slots `2`–`9` are user lists. The music tab shows the current list only; `p` slides a 9-slot picker up from the bottom. The current list is the play range for `n` / shuffle / loop. Missing files stay in the JSON, render gray, and are skipped.

Settings: theme, transparent background, visualize (`off` / `bars` / `scope` / `cnm`), fetch lyrics, fetch covers, resume, [Beat This!](#beat-this), [voice handoff](#voice-handoff) (人声交接).

Missing tags / covers / lyrics fill from iTunes + lrclib, then Netease, QQ, and Kugou. Existing fields are kept. Lyrics land in a sidecar `lrc/` file; audio files are not rewritten. SPlayer is only for `?` search/download.

## AutoMix

`m` cycles `CUT` / `FADE` / `AUTO`. In `AUTO` each track is analysed once, cached in `~/.cache/zradio/analysis.json`, for tempo and beat phase, key (Camelot), BS.1770 loudness, peak, and where the music starts and ends.

- Next track: with shuffle off and loop not `all`, the best scoring track in the play range by key, tempo and energy, plus taste. Tempos lock within 8%, or an octave apart (80 BPM beats with 160).
- Level: every track plays at -14 LUFS, or lower if its peak through the crossover would pass -1 dBFS.
- Transition: when the tempos lock, the outgoing tail glides to the incoming tempo. The incoming track starts on the outgoing beat phase and is never resampled. The style follows the pair: `bass-swap` (a 160 Hz Linkwitz-Riley crossover hands the low end over mid-overlap), `filter` when the keys clash or the energy jumps, a 4 s `quick` when both do, a 4 s `echo-out` when the tempos do not lock.

Render transitions offline through the same mixer:

```sh
cargo run --release --example automix_render -- -o /tmp/mix a.flac b.flac c.flac
```

Each neighbouring pair becomes `NN.wav`: 12 s before the overlap, the overlap, then 6 s after. The log line gives the style, bend, fade, score, peak, and clipped samples. `--analyze` prints the analysis only. `--skip SECS` mixes as a skip at that point. `--mode fade|cut`, `--before`, `--after` and `--rate` are also accepted. `--beat-this path/to/beat_this.onnx` mixes on the model's grid. `--stems path/to/htdemucs.onnx` hands the voices over as the [voice handoff](#voice-handoff) does, and logs how long each separation took and where the voices change hands.

### Beat This!

Off by default. Turning it on in settings fetches the [Beat This!](https://github.com/CPJKU/beat_this) beat and downbeat model (Foscarin, Schlüter and Widmer, ISMIR 2024; MIT): 83 MB, the ONNX export in [beat_this_cpp](https://github.com/mosynthkey/beat_this_cpp) (MIT), pinned to a commit and checked against its sha256, into `~/.local/share/zradio/models/`. Nothing but the request for that file leaves the machine.

While it is on, the track playing and the one predicted next are scored on one background thread, 15 to 25 s of CPU and up to 340 MB of memory a track. The model's grid replaces the onset tracker's tempo and beat phase and adds the bar line, so the incoming track mixes in on the first bar its music is heard on. A rescored track reaches the mixer only between mixes, never under one already planned. A track the model hears no steady grid in keeps its own. Turned off, tracks go back to the onset grid as they play.

On 40 library tracks, the model held a steady grid in 35. On 27 of those it heard the onset tracker's tempo, and put the bar line elsewhere on 9 of the 27; on 7 it heard twice or half the tempo, on 1 three halves.

### Voice handoff

Off by default. Turning it on in settings (人声交接) fetches the htdemucs source separation model (Rouard, Massa and Défossez, ICASSP 2023; [Meta Demucs](https://github.com/facebookresearch/demucs), MIT): 130 MB, the ONNX export by [StemSplitio](https://huggingface.co/StemSplitio/htdemucs-onnx) (MIT) with its STFT rebuilt from ONNX `DFT` by [itamiArika](https://huggingface.co/itamiArika/htdemucs-int8-memory) (MIT), the fp16 build run in fp32 without quantization, pinned to a revision and checked against its sha256, into `~/.local/share/zradio/models/`. Nothing but the request for that file leaves the machine.

While it is on, each `AUTO` mix has the voices of both tracks separated over the overlap on one background thread, about 20 to 35 s of CPU and up to 1 GB of memory a mix. Where both voices would be heard at once for a second or more, within 12 dB of each other, the outgoing voice fades out over 0.3 s up to a beat of the incoming track and the incoming voice fades in from it; the instruments mix as before. Of the beats that take the least voice away, the handoff takes the one nearest the middle of the overlap. A mix whose voices never clash plays as it would without the setting.

To leave time for the separation, the next track is fetched 45 s sooner. A mix the separation is not ready for keeps both voices: one a skip starts at once, or one replanned by a seek just before it. So do the mixes armed and under way when the setting is turned off.

On 6 transitions between 7 library tracks, the voices clashed in 1, a 4 s `echo-out`, and changed hands 2 s in; the other 5 rendered to the same samples as without it.

## Visualize

- `bars` — packed columns
- `scope` — real PCM oscilloscope (rising-edge trigger, braille envelope)
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
