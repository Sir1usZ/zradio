# ZRadio Bug Hardening Design

## Goal

Fix reproducible data-integrity and control-path defects without changing ZRadio's user-facing playback, playlist, metadata, or remote-control feature set.

## Constraints

- Keep the existing CLI, TUI keys, JSON API routes, and on-disk JSON schemas compatible.
- Add no new runtime dependency.
- Do not broadly rewrite `ui.rs`; extract only small invariants that make the fixes testable.
- Every behavior change starts with a failing regression test and lands in an independently revertible Git commit.
- The final branch must pass formatting, all-target tests, Clippy with warnings denied, and a debug build.

## Confirmed defects

### Stale asynchronous track results

Decode, local-tag, and remote-metadata workers identify results primarily by a vector index. Opening or rescanning a library replaces that vector while old workers continue running. A late result can therefore mutate or play a different track that happens to reuse the same index.

Each library replacement will advance a generation counter. Track-bound jobs will carry the generation, index, and source path captured when they were started. The UI will accept a result only when all three still identify the current track. Error results use the same identity so an obsolete failure cannot clear a newer in-flight job.

### Destructive metadata peeks

`apply_peek` currently replaces an existing metadata record unless it already contains lyrics or a decoded cover. A delayed local tag peek can erase remotely filled title, artist, or album fields. The merge will instead fill only empty text fields and missing rich fields, preserving every existing non-empty value.

### Cover cache collisions and wrong media types

Cover cache names currently use only the source file stem. Equal stems in different folders overwrite one another. Remote PNG bytes also default to a `.jpg` cache path, causing the API to emit the wrong content type.

Cache names will include a deterministic hash of the full source path. The extension will prefer a supplied MIME type and otherwise use byte signatures for PNG, GIF, and WebP before falling back to JPEG. Per-album sidecar behavior remains unchanged.

### API false success

Mutating API handlers discard `Sender::send` failures and return `ok: true` even after the UI control receiver has gone away. A shared enqueue helper will turn a disconnected receiver into `{"ok":false,"error":"control unavailable"}`. Successful request payloads and routes remain unchanged.

### Unsafe and non-retryable JSON persistence

Several stores write JSON directly over the destination. A partial write can destroy the last valid file. `AnalysisStore` and `TasteStore` also clear their dirty flags even when the write fails, suppressing retries.

A small storage module will serialize to a unique temporary file in the destination directory, flush it, and atomically rename it over the destination. Playlist, preferences, session, analysis, and taste persistence will share it. Dirty flags clear only after success. Existing public method signatures and JSON schemas remain compatible.

## Verification

Each defect gets a focused regression test that fails against the pre-fix code. After every task, run its focused tests and the complete suite before committing. At the end run:

```sh
cargo fmt --all -- --check
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo build
git diff --check
git status --short --branch
```
