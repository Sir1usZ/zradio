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

ZRadio plays a folder of music files in the terminal. It can mix one track into the next on the beat, learns which tracks you finish and which you skip, and shows lyrics, cover art and a spectrum. It needs no account. What it sends over the network is listed under [Network use](#network-use).

## Build and run

You need a Rust toolchain, a C compiler, `pkg-config`, and the ALSA and D-Bus headers:

```sh
# Arch
sudo pacman -S --needed base-devel rust alsa-lib dbus
# Debian / Ubuntu (Rust from rustup)
sudo apt install build-essential pkg-config libasound2-dev libdbus-1-dev
```

```sh
cargo build --release
./target/release/zradio            # the saved library, ~/音乐 until you pick one
./target/release/zradio ~/Music    # another folder for this run
```

`:open DIR` (or `Ctrl+F`) opens a folder and saves it as the library. ZRadio scans the folder and its subfolders, skipping hidden ones, for `mp3`, `flac`, `ogg` (Vorbis), `wav`, `aiff`, `m4a` and `aac` files. `.opus` files are listed but do not play yet, because the decoder (symphonia) has no Opus codec.

`cargo build --release --no-default-features` leaves out [Beat This!](#beat-this) and the [voice handoff](#voice-handoff); the binary drops from about 40 MB to 10 MB.

Optional programs, used when found:

| Program | Used for |
|---|---|
| `curl` | Everything ZRadio fetches: lyrics, covers, tag lookup, search, downloads, models |
| `cava` | Spectrum data for the `bars` and `cnm` visualizers (otherwise ZRadio's own FFT) |
| `yt-dlp` (and Node.js for YouTube) | Importing YouTube and Bilibili links; looked for in `~/.local/bin`, on `PATH`, and as the `yt_dlp` Python module |
| SPlayer | Extra search and download sources, through its local API at `127.0.0.1:25884` |

## Screens and keys

ZRadio has three tabs, switched with `q` and `e`: 音乐 (the current list), 播放器 (cover, lyrics and visualizer of the track playing) and 我 (listening time, favourite tracks and artists). `Ctrl+K` shows the keys in the app.

| Key | Action |
|---|---|
| `j` `k` or arrows | Move |
| `Enter` | Play the selected track, or download the selected online search result |
| `Space` | Play / pause |
| `n` / `b` | Next / previous in the current list |
| `←` / `→` | Seek 5 s (播放器 tab) |
| `+` / `-` | Volume up / down 5% |
| `m` | Mix mode: `CUT` / `FADE` / `AUTO` |
| `s` | Shuffle: `off` / `random` / `norepeat` / `taste` |
| `o` | Loop: `OFF` / `ONE` / `ALL` |
| `r` | Remix: `RAW` / `CHILL` / `CLUB` / `NCORE` |
| `l` | Lyrics on / off (播放器 tab) |
| `g` | Equalizer (播放器 tab) |
| `t` | Settings |
| `y` | Library window: the whole library, not just the current list |
| `p` | Playlist drawer |
| `x` | Track menu for the selected track (音乐) or the one playing (播放器) |
| `:` | Command line |
| `/` | Find in the library by title, artist, album or path |
| `?` | Search online |
| `Ctrl+F` | Open a folder (`:open `) |
| `Esc` | Close search results; it never quits |
| `Ctrl+Q` | Quit, saving the track and position for resume |

### Windows

**Library** (`y`)

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | View: 全部 (all), 有标签 (has artist or album), 缺标签 (has neither), 歌手 (by artist), 专辑 (by album) |
| `Enter` | Open an artist or album, or play a track |
| `Backspace` | Back out of an artist or album |
| `.` | Sort by path, title, artist or album |
| `s` | Cycle shuffle |
| `u` / `U` | Look up the selected track / every track still missing something (see [Tags](#lyrics-covers-and-tags)) |
| `x` | Track menu |
| `Esc` / `y` | Close |

**Playlist drawer** (`p`): `1`–`9`, or `j` `k` and `Enter`, switch to a list; an empty slot does not open. `Esc` or `p` closes it.

**Track menu** (`x`): 加到播放列表 (add to a user list), 补全这首 (look this track up online), 播放 (play), and 从当前列表移除 (remove from the current user list) when the track is in it.

**Equalizer** (`g`): five bands at 60, 250, 1k, 4k and 12k Hz. `j` `k` pick a band, `←` `→` move it 1 dB within ±12 dB, `r` resets, `Esc` or `g` closes. The bands are saved.

**Settings** (`t`): `j` `k` move, `Enter` or `Space` changes the row, `Esc` or `t` closes. Every row is saved.

| Row | Default | What it does |
|---|---|---|
| 主题 | `frappe` | `system`, or a Catppuccin flavor: `latte`, `frappe`, `macchiato`, `mocha`. `system` uses `latte` when `COLORFGBG` says the terminal is light, `frappe` otherwise |
| 透明背景 | off | Reset the terminal's background colour to its default at start; takes effect on the next start |
| 可视化 | `bars` | `off`, `bars`, `scope` or `cnm` (see [Visualizer](#visualizer)) |
| 拉歌词 | off | Fetch missing lyrics online |
| 拉封面 | off | Fetch missing covers online |
| 续播 | on | At start, play the last track from where it stopped |
| Beat This! | off | [Beat This!](#beat-this) beat and downbeat model for AutoMix |
| 人声交接 | off | [Voice handoff](#voice-handoff) between the vocals of two tracks in AutoMix |
| 局域网遥控 | off | Open the [remote control API](#remote-control-api) to other devices |

### Command line

`:` opens it; `Enter` runs it, `Esc` cancels.

| Command | Action |
|---|---|
| `playlist NAME` | Create a user list (8 at most) |
| `playlist-rm` | Delete the current user list |
| `find TEXT` | Find in the library (what `/` opens) |
| `search TEXT` | Search online (what `?` opens) |
| `open DIR` | Open a folder and save it as the library (what `Ctrl+F` opens) |
| a YouTube or Bilibili link | Import it with `yt-dlp` (see [Search and import](#search-and-import)) |
| anything else | Find in the library |

Pasting a link while the command line is closed opens it with the link in it; `Enter` imports.

## Playlists

Slot `1` of the drawer is the whole library; slots `2`–`9` are user lists, kept in `~/.local/share/zradio/playlists.json`. The 音乐 tab shows the current list, and the current list is the play range: `n`, `b`, shuffle, loop and AutoMix only pick tracks from it. Add tracks with `x` → 加到播放列表. A file that has gone missing stays in the list, shows in gray, and is skipped.

## Playback

- **Mix** (`m`): `CUT` starts the next track at once, `FADE` crossfades over 6 s (3.5 s when you skip), `AUTO` mixes with [AutoMix](#automix). Each run starts in `FADE`.
- **Shuffle** (`s`): `random`; `norepeat`, which avoids the last 12 tracks played; `taste`, which leans toward tracks and artists you finish and replay. The mode is saved.
- **Loop** (`o`): `ONE` repeats the track, `ALL` walks the list in order. Each run starts at `OFF`.
- **Remix** (`r`): `CHILL` plays 8% slower and darker, `CLUB` adds soft saturation, `NCORE` plays 12% faster. Speed and pitch change together. Each run starts at `RAW`.
- **Taste**: a track played to 80% counts as a listen, the same track finished twice in a row as a replay, and one skipped before 40% as a skip. A track scores listens + 1.6 × replays − 0.4 × skips, and an artist the sum of their tracks. The scores drive `taste` shuffle and add to AutoMix's choice; the 我 tab shows the top ones. They are kept in `~/.local/share/zradio/taste.json`.
- **Resume** (续播): on by default; the last track starts from its saved position.
- **Media keys**: ZRadio registers with MPRIS, so desktop media keys and `playerctl` can play, pause, stop, seek, and skip.

## Visualizer

- `bars`: packed spectrum columns
- `scope`: oscilloscope of the PCM being played (rising-edge trigger, braille envelope)
- `cnm`: gapped density bars with a baseline and vertical gradient (look inspired by CNMPlayer, independently implemented)

When `cava` is on `PATH`, it supplies the spectrum; otherwise ZRadio runs its own FFT.

## AutoMix

Press `m` until the status shows `AUTO`. Each track is analysed once for tempo and beat phase, key (Camelot), BS.1770 loudness, peak, and where the music starts and ends. The results are cached in `~/.cache/zradio/analysis.json`.

- **Next track**: with shuffle and loop off, AutoMix picks the track in the play range that scores best against the one playing on key, tempo and energy, plus its taste score. Tempos lock within 8%, or an octave apart (80 BPM beats with 160).
- **Level**: every track plays at -14 LUFS, or lower if its peak through the crossover would pass -1 dBFS.
- **Transition**: when the tempos lock, the outgoing tail glides to the incoming tempo. The incoming track starts on the outgoing beat phase and is never resampled. The style follows the pair: `bass-swap` (a 160 Hz Linkwitz-Riley crossover hands the low end over mid-overlap), `filter` when the keys clash or the energy jumps, a 4 s `quick` when both do, a 4 s `echo-out` when the tempos do not lock.

Two optional models improve the mix. Both are off by default and run on the CPU. Turning one on fetches its model with `curl` if it is not on disk yet, checked against a pinned sha256. In a build without them, their settings rows read 未编译.

### Beat This!

Off by default. Turning it on in settings fetches the [Beat This!](https://github.com/CPJKU/beat_this) beat and downbeat model (Foscarin, Schlüter and Widmer, ISMIR 2024; MIT): 83 MB, the ONNX export in [beat_this_cpp](https://github.com/mosynthkey/beat_this_cpp) (MIT), pinned to a commit and checked against its sha256, into `~/.local/share/zradio/models/`. Nothing but the request for that file leaves the machine.

While it is on, the track playing and the one predicted next are scored on one background thread, 15 to 25 s of CPU and up to 340 MB of memory a track. The model's grid replaces the onset tracker's tempo and beat phase and adds the bar line, so the incoming track mixes in on the first bar its music is heard on. A rescored track reaches the mixer only between mixes, never under one already planned. A track the model hears no steady grid in keeps its own. Turned off, tracks go back to the onset grid as they play.

On 40 library tracks, the model held a steady grid in 35. On 27 of those it heard the onset tracker's tempo, and put the bar line elsewhere on 9 of the 27; on 7 it heard twice or half the tempo, on 1 three halves.

### Voice handoff

Off by default. Turning it on in settings (人声交接) fetches the htdemucs source separation model (Rouard, Massa and Défossez, ICASSP 2023; [Meta Demucs](https://github.com/facebookresearch/demucs), MIT): 130 MB, the ONNX export by [StemSplitio](https://huggingface.co/StemSplitio/htdemucs-onnx) (MIT) with its STFT rebuilt from ONNX `DFT` by [itamiArika](https://huggingface.co/itamiArika/htdemucs-int8-memory) (MIT), the fp16 build run in fp32 without quantization, pinned to a revision and checked against its sha256, into `~/.local/share/zradio/models/`. Nothing but the request for that file leaves the machine.

While it is on, each `AUTO` mix has the voices of both tracks separated over the overlap on one background thread, about 20 to 35 s of CPU and up to 1 GB of memory a mix. Where both voices would be heard at once for a second or more, within 12 dB of each other, the outgoing voice fades out over 0.3 s up to a beat of the incoming track and the incoming voice fades in from it; the instruments mix as before. Of the beats that take the least voice away, the handoff takes the one nearest the middle of the overlap. A mix whose voices never clash plays as it would without the setting.

To leave time for the separation, the next track is fetched 45 s sooner. A mix the separation is not ready for keeps both voices: one a skip starts at once, or one replanned by a seek just before it. So do the mixes armed and under way when the setting is turned off.

On 6 transitions between 7 library tracks, the voices clashed in 1, a 4 s `echo-out`, and changed hands 2 s in; the other 5 rendered to the same samples as without it.

### Offline render

The `automix_render` example runs files through the same mixer and writes each transition to a WAV file:

```sh
cargo run --release --example automix_render -- -o /tmp/mix a.flac b.flac c.flac
```

Each neighbouring pair becomes `NN.wav` (`01.wav`, `02.wav`, …): 12 s before the overlap, the overlap, then 6 s after. The log line gives the style, bend, fade, score, peak, and clipped samples.

| Option | Effect |
|---|---|
| `--analyze` | Print the analysis of each file and render nothing (one file is enough) |
| `--mode auto\|fade\|cut` | Mix mode (default `auto`) |
| `--skip SECS` | Mix as if you skipped at that point of the outgoing track |
| `--before SECS` / `--after SECS` | Context kept before and after the overlap (default 12 / 6) |
| `--rate HZ` | Output sample rate (default 48000) |
| `-o DIR` | Output folder (default `automix-out`) |
| `--beat-this MODEL` | Mix on the grid of a [Beat This!](#beat-this) model file |
| `--stems MODEL` | Hand the voices over as the [voice handoff](#voice-handoff) does, and log how long each separation took and where the voices change hands |

## Lyrics, covers and tags

ZRadio reads tags with lofty and never rewrites an audio file. What it finds online is kept beside the track or in its own folders.

- **Lyrics** come from the track's embedded lyrics, then `NAME.lrc` beside it, then `lrc/NAME.lrc` or `LRC/NAME.lrc`. With 拉歌词 on, missing lyrics are looked up on lrclib, then Netease, QQ Music and Kugou, and saved to `lrc/NAME.lrc` beside the track.
- **Covers** come from the track's embedded front cover, then `NAME.jpg` / `NAME.png`, `cover.jpg`, `cover.png`, `folder.jpg`, or the same names in `cover/` or `covers/`. An embedded cover, or one fetched with 拉封面 on, is cached in `~/.cache/zradio/` and copied to `cover/NAME.EXT` beside the track. Covers are drawn with half-block characters.
- **Tags**: when a track has no artist or no album, ZRadio looks it up by title (the file name when untagged), artist and album on iTunes, Netease, QQ Music and Kugou (iTunes last when the title or artist has Chinese, Japanese or Korean characters). This happens as the library is scanned at start and when the track plays, whether or not 拉歌词 and 拉封面 are on. Only empty fields are filled, and only in memory, so the lookup runs again on the next start. `u`, `U` and 补全这首 queue lookups by hand.

## Search and import

`?` searches Netease through the public `apis.netstart.cn` mirror. When that returns fewer than 12 results and SPlayer is running, its local API adds Netease and QQ Music results. `Enter` on a result downloads it with `curl` as `ARTIST - TITLE.EXT`, from the mirror's link if it has one, otherwise through SPlayer.

A YouTube or Bilibili link on the command line is imported with `yt-dlp` as an MP3 at its best quality, the single video only. When a Chrome or Firefox profile is found, `yt-dlp` gets its cookies (`--cookies-from-browser`).

Downloads and imports always go to `~/音乐`, whatever folder the library is. ZRadio adds them to the session and plays them at once; if the library is another folder, they are gone from it after the next start.

## Network use

ZRadio has no account and sends no telemetry. These are all the requests it makes, each through `curl` unless noted:

| When | What is sent | To |
|---|---|---|
| A track has no artist or no album (always on) | Title, artist, album | iTunes, the Netease mirror `apis.netstart.cn`, QQ Music, Kugou |
| 拉歌词 is on and a track has no lyrics | Title, artist | lrclib, then the same services |
| 拉封面 is on and a track has no cover | Title, artist, album | The same services, then the image host they link to |
| `?` search and its downloads | The search text, the chosen song's ID | `apis.netstart.cn`; SPlayer on `127.0.0.1:25884`, which queries its own sources |
| A link is imported | The link, and browser cookies when found | The site, through `yt-dlp` |
| Beat This! or 人声交接 is turned on and its model is not on disk | A request for the pinned model file | `raw.githubusercontent.com` or `huggingface.co` |

Kugou's lyrics are fetched over plain HTTP, and so are some of the cover images the services link to.

## Remote control API

HTTP API on port `18765`, listening on `127.0.0.1` only. Control, status, and lyrics are JSON. `GET /cover/:index` returns image bytes on success. `/status` and `/library/full` do not include cover bytes or LRC text.

To control ZRadio from another device, turn on 局域网遥控 in settings (`t`). The API then listens on `0.0.0.0:18765` and other devices must send `Authorization: Bearer <token>`, where the token is in `~/.config/zradio/api-token` (mode `0600`, created the first time 局域网遥控 is turned on). Requests from this machine need no token. Cross-origin browser requests get `403`.

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

Full API (in Chinese): [docs/API.md](docs/API.md).

## Files

| Path | Holds |
|---|---|
| `~/.config/zradio/prefs.json` | Settings, library folder, equalizer, sort and shuffle |
| `~/.config/zradio/api-token` | Token for the LAN remote |
| `~/.local/share/zradio/playlists.json` | User lists |
| `~/.local/share/zradio/session.json` | Last track and position, for resume |
| `~/.local/share/zradio/taste.json` | Listens, replays, skips and listening time per track |
| `~/.local/share/zradio/models/` | Beat This! and htdemucs models |
| `~/.local/share/zradio/alsa.log` | ZRadio's stderr, mostly ALSA messages, kept off the screen; stops growing at 8 MB |
| `~/.cache/zradio/analysis.json` | AutoMix analysis |
| `~/.cache/zradio/NAME-HASH.EXT` | Cached covers |
| `lrc/`, `cover/` beside the tracks | Fetched lyrics; embedded and fetched covers |

`XDG_CONFIG_HOME` moves `~/.config` and `XDG_CACHE_HOME` moves `~/.cache`. `XDG_DATA_HOME` moves `taste.json` and `models/`; `XDG_STATE_HOME` moves `playlists.json`, `session.json` and `alsa.log`.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test --all-targets --no-default-features
```

Four tests are ignored by default. Three run the models and need their files; the fourth fetches the pinned Beat This! model (83 MB):

```sh
ZRADIO_BEAT_THIS_MODEL=$HOME/.local/share/zradio/models/beat_this.onnx \
ZRADIO_STEMS_MODEL=$HOME/.local/share/zradio/models/htdemucs.onnx \
  cargo test --release -- --ignored
```

## License

MIT. See [LICENSE](LICENSE).
