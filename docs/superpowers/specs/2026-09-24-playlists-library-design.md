# Playlists + library browser

Local playlists, a large `y` library window, and an `x` context menu.

## Decisions

- Music tab: `1` = all library, `2–9` = user lists, `←`/`→` cycle occupied slots (including `1`).
- Seek `←`/`→` only on the Player tab.
- Artists/albums live in the `y` window as filters, not on `2`.
- Current list **is** the play range for `n` / shuffle / loop.
- At most 8 user lists. Full → refuse, delete first.
- `x` opens a context menu; “add to playlist” opens a submenu of slots `2–9`.
- Create with `:playlist 名字`. Delete current user list with `:playlist-rm`.
- Old `y` 5-row menu moves into the large window’s top bar. Global `s` still cycles shuffle.
- Store: `~/.local/share/zradio/playlists.json` (XDG state), identity = absolute path.
- Emoji only as slot marks (`♪`) and a short status (`♪ 已加入 通勤`).

## Data

```json
{
  "active": 0,
  "lists": [
    { "name": "通勤", "paths": ["/abs/path.flac"] }
  ]
}
```

- `active = 0` → slot `1` (all). `active = 1..8` → slots `2..9`.
- Missing files stay in JSON, render gray, excluded from mixer `playable`.
- Duplicate path in the same list is refused.
- Out-of-range `active` clamps to `0`.
- Write failure rolls memory back to the last successful disk read.
- Mixer keeps scan indices; `playable: Option<Vec<usize>>` filters `next_index` / shuffle / prev. `None` = whole library.

If the current track is outside the new range, it finishes; the next `n` enters the new range. Empty / all-missing list: `n` does nothing.

## `y` window

Centered ~80%×80%, `Clear` panel. Browses the **whole library** (top-bar filters), independent of slots `1–9`.

- Top: 全部 / 有标签 / 缺标签 / 歌手 / 专辑, sort, full scan.
- Left: cover + tags for the highlighted row. No fake cover.
- Right: file names. `j`/`k` move. Groups: Enter drills in, Backspace returns.
- `u` fetch meta for the highlighted track (queue head). `U` enqueue missing.
- `1–9` do **not** change play range while `y` is open.

## Keys (v1)

Music tab: `1–9` jump slot (empty slot refuses), `←`/`→` cycle, `x` menu, Enter play.

Player tab: `←`/`→` seek 5s. `1`/`2` do not switch library/artists.

`y`: `j`/`k`, Enter, Backspace, Tab / Shift+Tab filter, `.` sort, `u`/`U`, `x`, Esc/`y` close.

`x` menu: 加到播放列表 → submenu `2–9`; 补全这首; 播放; 从当前列表移除 (only on a user list).

## Out of scope (v1)

Remote playlist API, rename, `y`-open `1–9` changing play range.
