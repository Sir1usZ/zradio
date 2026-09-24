# ZRadio Remote Control API

HTTP JSON API，用于远程控制 ZRadio 播放器。监听 `0.0.0.0:18765`，支持 CORS，浏览器/手机/脚本可直接调用。

## 快速开始

```bash
# 查看当前状态
curl -s http://localhost:18765/status | jq

# 播放
curl -X POST http://localhost:18765/control -d '{"action":"play"}'

# 暂停
curl -X POST http://localhost:18765/control -d '{"action":"pause"}'

# 下一曲
curl -X POST http://localhost:18765/control -d '{"action":"next"}'
```

## 基础信息

- **地址:** `http://<host>:18765`
- **协议:** HTTP/1.1 JSON
- **CORS:** 完全开放（`Access-Control-Allow-Origin: *`）
- **认证:** 无（仅限本地/可信网络使用）

## 响应格式

所有响应遵循统一格式：

```json
{
  "ok": true,
  "data": { ... }
}
```

错误时：

```json
{
  "ok": false,
  "error": "错误描述"
}
```

---

## GET 端点

### `/status` — 完整状态

返回播放状态、当前曲目、EQ、资料库、口味统计、偏好设置。

```bash
curl -s http://localhost:18765/status | jq
```

响应示例：

```json
{
  "ok": true,
  "data": {
    "playback": {
      "paused": false,
      "position_secs": 45.2,
      "duration_secs": 240.0,
      "volume": 85,
      "mix_mode": "automix",
      "remix_mode": "RAW",
      "shuffle": true,
      "loop_mode": "OFF",
      "status": "playing Levels · 128 8A",
      "fading": false,
      "bpm": 128.0,
      "key": "8A",
      "next_hint": "next Fade Into Darkness 132 9A",
      "sample_rate": 48000
    },
    "current_track": {
      "index": 3,
      "title": "Levels",
      "artist": "Avicii",
      "album": "True",
      "path": "/home/xender/music/Avicii/True/Levels.mp3",
      "has_cover": true,
      "has_lyrics": false
    },
    "eq": {
      "bands": ["60", "250", "1k", "4k", "12k"],
      "db": [0.0, 2.0, -1.0, 0.0, 3.0]
    },
    "library": {
      "total": 342
    },
    "taste": {
      "total_listen_secs": 128400,
      "top_tracks": [
        {"title": "Levels", "artist": "Avicii", "score": 4.6}
      ]
    },
    "prefs": {
      "theme": "frappe",
      "visualize": "bars",
      "sort_mode": "artist",
      "shuffle_mode": "taste"
    }
  }
}
```

### `/health` — 健康检查

```bash
curl -s http://localhost:18765/health | jq
```

```json
{"ok": true, "data": {"version": "0.1.0", "name": "zradio"}}
```

### `/library` — 资料库概览

```json
{"ok": true, "data": {"total": 342, "current_track": {...}}}
```

### `/library/full` — 完整曲目列表

返回所有曲目的标题、歌手、专辑、路径、是否有封面/歌词。

```bash
curl -s http://localhost:18765/library/full | jq '.data.tracks[:5]'
```

```json
{
  "ok": true,
  "data": {
    "total": 342,
    "tracks": [
      {
        "index": 0,
        "title": "Fade Into Darkness",
        "artist": "Avicii",
        "album": "True",
        "path": "/home/xender/music/Avicii/True/Fade Into Darkness.mp3",
        "has_cover": true,
        "has_lyrics": true
      }
    ]
  }
}
```

### `/track/:index` — 曲目详情

```bash
curl -s http://localhost:18765/track/3 | jq
```

```json
{
  "ok": true,
  "data": {
    "info": { "index": 3, "title": "Levels", "artist": "Avicii", ... },
    "bpm": 128.0,
    "key": "8A"
  }
}
```

### `/cover/:index` — 封面信息

```json
{"ok": true, "data": {"index": 3, "has_cover": true}}
```

### `/lyrics/:index` — 完整 LRC

任意曲目下标都返回完整 LRC 文本。没有歌词时 `lrc` 为空字符串。省略 index 时用正在播的那首。

```bash
curl -s http://localhost:18765/lyrics/3 | jq -r '.data.lrc'
```

```json
{
  "ok": true,
  "data": {
    "index": 3,
    "has_lyrics": true,
    "lrc": "[00:00.00]Somehow I wake up\n[00:04.50]I found my way back home\n"
  }
}
```

### `/shuffle` — 随机模式

```json
{
  "ok": true,
  "data": {
    "shuffle": true,
    "mode": "taste",
    "modes": ["off", "random", "norepeat", "taste"]
  }
}
```

### `/loop` — 循环模式

```json
{
  "ok": true,
  "data": {
    "mode": "off",
    "modes": ["off", "one", "all"]
  }
}
```

### `/mix` — 混音模式

```json
{
  "ok": true,
  "data": {
    "mode": "automix",
    "modes": ["cut", "crossfade", "automix"]
  }
}
```

### `/remix` — Remix 模式

```json
{
  "ok": true,
  "data": {
    "mode": "RAW",
    "modes": ["off", "chill", "club", "ncore"]
  }
}
```

### `/sort` — 排序模式

```json
{
  "ok": true,
  "data": {
    "mode": "artist",
    "modes": ["path", "title", "artist", "album"]
  }
}
```

### `/eq` — EQ 设置

```json
{
  "ok": true,
  "data": {
    "bands": ["60", "250", "1k", "4k", "12k"],
    "db": [0.0, 2.0, -1.0, 0.0, 3.0]
  }
}
```

### `/prefs` — 偏好设置

```bash
curl -s http://localhost:18765/prefs | jq
```

```json
{
  "ok": true,
  "data": {
    "theme": "frappe",
    "transparent": false,
    "visualize": "bars",
    "lyrics_fetch": true,
    "cover_fetch": true,
    "resume": true,
    "library": "/home/xender/music",
    "sort_mode": "artist",
    "shuffle_mode": "taste",
    "themes": ["system", "latte", "frappe", "macchiato", "mocha"],
    "visualizes": ["off", "bars", "scope", "cnm"],
    "sorts": ["path", "title", "artist", "album"],
    "shuffles": ["off", "random", "norepeat", "taste"]
  }
}
```

### `/taste` — 口味统计

```json
{
  "ok": true,
  "data": {
    "total_listen_secs": 128400,
    "total_listen_hours": 35.7,
    "top_tracks": [
      {"title": "Levels", "artist": "Avicii", "score": 4.6},
      {"title": "Clarity", "artist": "Zedd", "score": 3.2}
    ]
  }
}
```

---

## POST 端点

### `/control` — 播放控制

所有播放控制通过此端点，用 `action` 字段区分操作。

#### 基础播放

| action | 参数 | 说明 |
|---|---|---|
| `play` | — | 播放或恢复 |
| `play` | `index` | 播放指定序号的曲目 |
| `play_path` | `path` | 按文件路径播放 |
| `pause` | — | 暂停 |
| `toggle` | — | 切换暂停/播放 |
| `next` | — | 下一曲 |
| `prev` | — | 上一曲 |

```bash
# 播放第 5 首
curl -X POST http://localhost:18765/control \
  -d '{"action":"play","index":5}'

# 按路径播放
curl -X POST http://localhost:18765/control \
  -d '{"action":"play_path","path":"/home/xender/music/Levels.mp3"}'
```

#### 跳转

| action | 参数 | 说明 |
|---|---|---|
| `seek` | `value` | 相对跳转（秒），正数前进，负数后退 |
| `seek_to` | `value` | 绝对跳转（秒） |

```bash
# 快进 30 秒
curl -X POST http://localhost:18765/control \
  -d '{"action":"seek","value":30.0}'

# 跳到 1 分 30 秒
curl -X POST http://localhost:18765/control \
  -d '{"action":"seek_to","value":90.0}'
```

#### 音量

| action | 参数 | 说明 |
|---|---|---|
| `volume` | `value` | 相对增减（-1.0 ~ 1.0） |
| `set_volume` | `value` | 绝对值（0 ~ 100） |

```bash
# 音量设为 80%
curl -X POST http://localhost:18765/control \
  -d '{"action":"set_volume","value":80}'

# 增加 5%（相对值 0.05）
curl -X POST http://localhost:18765/control \
  -d '{"action":"volume","value":0.05}'
```

#### 播放模式

| action | 参数 | 可选值 | 说明 |
|---|---|---|---|
| `mix` | — | — | 循环切换混音模式 |
| `set_mix` | `query` | `cut` `crossfade` `automix` | 设置混音模式 |
| `remix` | — | — | 循环切换 Remix |
| `set_remix` | `query` | `off` `chill` `club` `ncore` | 设置 Remix |
| `shuffle` | — | — | 循环切换随机 |
| `set_shuffle` | `query` | `off` `random` `norepeat` `taste` | 设置随机模式 |
| `loop` | — | — | 循环切换循环 |
| `set_loop` | `query` | `off` `one` `all` | 设置循环模式 |

```bash
# 随机模式设为口味加权
curl -X POST http://localhost:18765/control \
  -d '{"action":"set_shuffle","query":"taste"}'

# 混音设为 AutoMix
curl -X POST http://localhost:18765/control \
  -d '{"action":"set_mix","query":"automix"}'

# 循环设为单曲
curl -X POST http://localhost:18765/control \
  -d '{"action":"set_loop","query":"one"}'

# Remix 设为 Nightcore
curl -X POST http://localhost:18765/control \
  -d '{"action":"set_remix","query":"ncore"}'
```

#### EQ

| action | 参数 | 说明 |
|---|---|---|
| `eq` | `index`(0-4), `value`(-12~12) | 设置指定频段增益(dB) |
| `eq_reset` | — | 重置 EQ |

频段对照：`0`=60Hz, `1`=250Hz, `2`=1kHz, `3`=4kHz, `4`=12kHz

```bash
# 250Hz 提升 3dB
curl -X POST http://localhost:18765/control \
  -d '{"action":"eq","index":1,"value":3.0}'

# 重置 EQ
curl -X POST http://localhost:18765/control \
  -d '{"action":"eq_reset"}'
```

#### 排序/主题/可视化

| action | 参数 | 可选值 |
|---|---|---|
| `set_sort` | `query` | `path` `title` `artist` `album` |
| `set_theme` | `query` | `system` `latte` `frappe` `macchiato` `mocha` |
| `set_visualize` | `query` | `off` `bars` `scope` `cnm` |
| `toggle_transparent` | — | — |
| `toggle_lyrics_fetch` | — | — |
| `toggle_cover_fetch` | — | — |
| `toggle_resume` | — | — |

```bash
# 排序按歌手
curl -X POST http://localhost:18765/control \
  -d '{"action":"set_sort","query":"artist"}'

# 主题切 Mocha
curl -X POST http://localhost:18765/control \
  -d '{"action":"set_theme","query":"mocha"}'
```

#### 资料库

| action | 说明 |
|---|---|
| `meta_scan` | 扫描缺失的元数据（标题/歌手/专辑/封面/歌词） |
| `select` | 选中列表中某行（`index` 参数） |

### `/import` — 导入 URL

```bash
curl -X POST http://localhost:18765/import \
  -d '{"url":"https://music.163.com/song?id=123456"}'
```

### `/library/scan` — 重新扫描资料库

```bash
curl -X POST http://localhost:18765/library/scan
```

### `/meta/scan` — 元数据扫描

扫描所有缺少标题/歌手/专辑/封面/歌词的曲目，后台逐个补全。

```bash
curl -X POST http://localhost:18765/meta/scan
```

### `/eq` — 直接设置 EQ

```bash
curl -X POST http://localhost:18765/eq \
  -d '{"band":2,"db":4.5}'
```

### `/eq/reset` — 重置 EQ

```bash
curl -X POST http://localhost:18765/eq/reset
```

### `/search` — 搜索资料库

按标题、歌手、专辑、路径模糊搜索。

```bash
curl -X POST http://localhost:18765/search \
  -d '{"query":"avicii"}'
```

```json
{
  "ok": true,
  "data": {
    "query": "avicii",
    "total": 28,
    "matches": [
      {
        "index": 3,
        "title": "Levels",
        "artist": "Avicii",
        "album": "True",
        "path": "/home/xender/music/Avicii/True/Levels.mp3",
        "has_cover": true,
        "has_lyrics": false
      }
    ]
  }
}
```

---

## 偏好设置可选值速查

| 设置 | 可选值 |
|---|---|
| `theme` | `system`, `latte`, `frappe`, `macchiato`, `mocha` |
| `visualize` | `off`, `bars`, `scope`, `cnm` |
| `sort` | `path`, `title`, `artist`, `album` |
| `shuffle` | `off`, `random`, `norepeat`, `taste` |
| `loop` | `off`, `one`, `all` |
| `mix` | `cut`, `crossfade`, `automix` |
| `remix` | `off`, `chill`, `club`, `ncore` |

---

## 架构

```
手机/浏览器/脚本
       │
       ▼ HTTP JSON
┌─────────────────┐
│  API Server     │  0.0.0.0:18765
│  (api.rs)       │
└────────┬────────┘
         │
    SharedState (Arc<Mutex<ApiSnapshot>>)
         │
         ▼ 每 tick 更新
┌─────────────────┐
│  App (TUI)      │  主线程
│  Mixer + Engine │  音频回调
└─────────────────┘
```

- API 线程只读共享状态，不阻塞音频
- App 每 33ms 更新一次快照（播放位置、曲目、EQ、资料库）
- 控制命令通过 `mpsc::channel` 发送到 App 主线程执行

## 安全提示

API 监听 `0.0.0.0`，局域网内所有设备可访问。仅在可信网络使用，或通过防火墙限制访问。
