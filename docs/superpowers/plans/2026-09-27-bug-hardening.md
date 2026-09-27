# ZRadio Bug Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove five confirmed integrity defects while preserving ZRadio's existing interfaces and behavior.

**Architecture:** Keep the current module layout, but introduce explicit asynchronous job identities and one internal atomic-storage boundary. Each fix is isolated behind existing APIs and committed separately.

**Tech Stack:** Rust 2021, standard library channels/filesystem, Serde, Cargo test/Clippy/rustfmt.

**Spec:** `docs/superpowers/specs/2026-09-27-bug-hardening-design.md`

## Global Constraints

- Preserve all TUI keys, JSON API routes, public response shapes, and persisted JSON schemas.
- Add no runtime dependency.
- Use test-first red-green cycles and one independently revertible Git commit per task.
- Do not rewrite unrelated UI, audio, or network behavior.

---

### Task 1: Preserve enriched metadata during local peeks

**Files:**
- Modify: `src/meta.rs`
- Test: `src/meta.rs`

**Interfaces:**
- Consumes: `apply_peek(&mut Option<TrackMeta>, TrackMeta) -> bool`
- Produces: the same signature, with fill-only merge semantics and a changed flag.

- [ ] **Step 1: Write the failing regression test**

Add a test where the existing slot contains remote title and artist, the peek contains a fallback title and album, and assert that existing title/artist survive while the missing album is filled.

- [ ] **Step 2: Run the focused test and verify RED**

Run: `cargo test meta::tests::apply_peek_fills_only_missing_fields -- --exact`

Expected: FAIL because the current whole-record replacement erases the enriched values.

- [ ] **Step 3: Implement the minimal merge**

For an empty slot, insert the peeked record. For an existing slot, copy only non-empty peeked text into empty text fields and copy lyrics/cover data only when absent. Return whether any field changed.

- [ ] **Step 4: Verify GREEN and regression safety**

Run: `cargo test meta::tests::apply_peek -- --nocapture`

Run: `cargo test --all-targets --all-features`

- [ ] **Step 5: Commit**

```sh
git add src/meta.rs
git commit -m "fix: preserve enriched metadata during tag peeks"
```

### Task 2: Reject stale asynchronous track results

**Files:**
- Modify: `src/ui.rs`
- Test: `src/ui.rs`

**Interfaces:**
- Consumes: decode, peek, and remote metadata worker channel payloads.
- Produces: private `TrackJobId { generation: u64, index: usize, path: PathBuf }` identity and `matches_tracks(&[Track]) -> bool` validation.

- [ ] **Step 1: Write the failing identity tests**

Add literal fixtures proving that a job is rejected after a generation change, after a path change at the same index, and accepted only when generation, index, and path all match.

- [ ] **Step 2: Run the focused tests and verify RED**

Run: `cargo test ui::tests::track_job -- --nocapture`

Expected: FAIL because no generation/path-backed identity exists.

- [ ] **Step 3: Thread identity through worker results**

Add `library_generation` to `App`; increment it when `open_folder` or `rescan_library` replaces the library. Carry `TrackJobId` in decode success/failure, peek, and remote metadata jobs. Validate before changing metadata, mixer state, in-flight state, or status.

- [ ] **Step 4: Verify GREEN and regression safety**

Run: `cargo test ui::tests::track_job -- --nocapture`

Run: `cargo test --all-targets --all-features`

- [ ] **Step 5: Commit**

```sh
git add src/ui.rs
git commit -m "fix: discard stale asynchronous track jobs"
```

### Task 3: Make cover cache identity and type correct

**Files:**
- Modify: `src/meta.rs`
- Test: `src/meta.rs`

**Interfaces:**
- Consumes: `store_cover(src: &Path, data: &[u8], mime: Option<&str>)`
- Produces: collision-resistant path-derived cache names and content-derived extensions.

- [ ] **Step 1: Write failing cache-name and media-type tests**

Use two literal source paths ending in `song.mp3` and assert distinct cache names. Assert PNG-signature bytes select `png` when MIME is absent and an explicit MIME takes precedence.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test meta::tests::cover_cache -- --nocapture`

Expected: FAIL because current names use only the stem and absent MIME always becomes JPEG.

- [ ] **Step 3: Implement deterministic naming and sniffing**

Add a deterministic FNV-1a hash over the full path representation, retain a readable sanitized stem, and choose `png`, `gif`, or `webp` from MIME/signature before the JPEG fallback.

- [ ] **Step 4: Verify GREEN and regression safety**

Run: `cargo test meta::tests::cover_cache -- --nocapture`

Run: `cargo test --all-targets --all-features`

- [ ] **Step 5: Commit**

```sh
git add src/meta.rs
git commit -m "fix: isolate cover cache files by source path"
```

### Task 4: Report unavailable API control dispatch

**Files:**
- Modify: `src/api.rs`
- Test: `src/api.rs`

**Interfaces:**
- Consumes: existing `Sender<ControlCmd>` in mutating handlers.
- Produces: `enqueue_control(&Sender<ControlCmd>, ControlCmd) -> Result<(), serde_json::Value>` used by every mutating handler.

- [ ] **Step 1: Write the failing disconnected-channel tests**

Drop a channel receiver, call representative `/control`, import, scan, and EQ handlers, and assert `ok == false` with `control unavailable`.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test api::tests::disconnected_control -- --nocapture`

Expected: FAIL because handlers currently discard send errors and report success.

- [ ] **Step 3: Implement one dispatch boundary**

Add the helper and return its error response from `handle_control`, `handle_import`, `handle_library_scan`, `handle_meta_scan`, `handle_eq`, and `handle_eq_reset`. Keep successful payloads unchanged.

- [ ] **Step 4: Verify GREEN and regression safety**

Run: `cargo test api::tests::disconnected_control -- --nocapture`

Run: `cargo test --all-targets --all-features`

- [ ] **Step 5: Commit**

```sh
git add src/api.rs
git commit -m "fix: report failed API control dispatch"
```

### Task 5: Centralize atomic JSON persistence

**Files:**
- Create: `src/storage.rs`
- Modify: `src/lib.rs`
- Modify: `src/analysis.rs`
- Modify: `src/playlist.rs`
- Modify: `src/prefs.rs`
- Modify: `src/session_store.rs`
- Modify: `src/taste.rs`
- Test: `src/storage.rs`, `src/analysis.rs`, `src/taste.rs`

**Interfaces:**
- Produces: `pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()>`.
- Preserves: existing public save/flush method signatures and JSON structures.

- [ ] **Step 1: Write failing persistence tests**

Test that replacing a valid JSON file leaves no temporary files and round-trips the new value. In analysis and taste tests, point a dirty store at a directory path, call `flush`, and assert it remains dirty after the failed write.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test storage::tests analysis::tests::failed_flush taste::tests::failed_flush -- --nocapture`

Expected: FAIL because no atomic helper exists and both stores currently clear dirty after write failure.

- [ ] **Step 3: Implement the atomic storage boundary**

Serialize pretty JSON, create the parent directory, write and `sync_all` a unique same-directory temporary file, rename it over the destination, and remove the temporary on failure. Route all five JSON stores through the helper; clear dirty only on `Ok(())`.

- [ ] **Step 4: Verify GREEN and all quality gates**

Run: `cargo test storage::tests -- --nocapture`

Run: `cargo test analysis::tests::failed_flush taste::tests::failed_flush -- --nocapture`

Run: `cargo fmt --all -- --check`

Run: `cargo test --all-targets --all-features`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Run: `cargo build`

- [ ] **Step 5: Commit**

```sh
git add src/storage.rs src/lib.rs src/analysis.rs src/playlist.rs src/prefs.rs src/session_store.rs src/taste.rs
git commit -m "fix: make JSON persistence atomic and retryable"
```

### Task 6: Final branch audit

**Files:**
- Inspect only.

**Interfaces:**
- Consumes: all commits from Tasks 1-5.
- Produces: verified clean `bug-fixer` branch with an auditable commit chain.

- [ ] **Step 1: Inspect every commit and aggregate diff**

Run: `git log --oneline --decorate main..HEAD`

Run: `git diff --stat main...HEAD`

Run: `git diff --check main...HEAD`

- [ ] **Step 2: Re-run the complete fresh verification suite**

Run: `cargo fmt --all -- --check`

Run: `cargo test --all-targets --all-features`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Run: `cargo build`

- [ ] **Step 3: Confirm repository state**

Run: `git status --short --branch`

Expected: clean `bug-fixer` branch with only the planned commits ahead of `main`.
