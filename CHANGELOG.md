# Changelog

All notable changes to Memory Archive are documented in this file. This project
adheres to [Semantic Versioning](https://semver.org/).

## [0.12.0] — 2026-07-09

Session-lifecycle and capture-fidelity release: a first-class session-delete
command, cursor-move frame capture, and an annotation TUI display fix.

### Added

- **`memory-archive session delete`.** Permanently purges a session from
  everywhere in one command: the Redis record, every status index set,
  `sessions:by_os:*` / `sessions:by_mode:*`, and `claim:{id}`; all stored files
  (cloud objects under `sessions/{id}/…` and the local memory directory,
  including any `(incomplete)` sibling from `mark_incomplete`); and the
  client-side temp/scratch directory. Deleting a stale ID whose Redis record has
  already expired still sweeps orphaned index/claim entries and leftover storage,
  so it also serves as an orphan cleaner. Active or annotating sessions are
  refused unless `--force` is passed. Implemented as an admin-only IPC operation
  (`DeleteSession`) — annotator TCP connections reject it via the existing
  authority catch-alls.
- **Cursor-move frames.** `mouse/move` steps now capture before/at/after
  screenshots with the cursor marked at the destination position, giving
  "move cursor to X" steps the visual context useful for CUA training instead of
  being frameless. Scroll and other mouse subtypes remain frameless by design.
  Applies to sessions captured on 0.12.0 and later.

### Fixed

- **Annotation TUI image pane.** Steps that are frameless by design (e.g. cursor
  moves in pre-0.12.0 sessions, scrolls) no longer render the alarming
  `✗ Image not found` / `No image available`; they read `No screenshot for this
  action`. The Open button label is now always reset on image-bearing steps, so
  navigating from a frameless step no longer leaves a stale "No image available"
  label, and the fullscreen external viewer is enabled on Windows
  (`os.startfile`) as well as Linux/macOS.

## [0.11.0] — 2026-07-08

Supply-chain and network-default hardening release, with an installer/updater
interface cleanup.

### Security

- **Verified installs (HIGH).** `install.sh`, `install.ps1`, and
  `memory-archive update` now download a `SHA256SUMS` manifest and verify each
  release archive against it **before** extraction. A missing or mismatched
  checksum aborts the install.
- **Safe archive extraction (HIGH).** Release archives are unpacked with
  per-member path-traversal guards — absolute paths, `..` components, and
  destination-escaping symlinks are rejected in the updater and both installers.
- **Metrics endpoint fails closed (MEDIUM).** The Prometheus endpoint now binds
  `127.0.0.1` by default via the new `observability.metrics_bind_addr`. Binding a
  non-loopback address requires `observability.metrics_token`; without it,
  ma-core falls back to loopback and logs a CRITICAL rather than exposing
  unauthenticated metrics.
- **Config file permissions (MEDIUM).** `config.json` (which may hold an
  annotator key) is written `0600`, its parent directory `0700`, on both the
  Rust and Python paths.
- **Exact annotation-claim verification.** Claim checks now match the claim id
  exactly instead of via a substring, and `HeartbeatClaim` verifies claim
  ownership — an annotator can no longer refresh another annotator's claim.
- **Directory-listing traversal fixed.** `ListSessionFiles` validates the
  requested prefix, so it can no longer enumerate directories outside a session.
- **Annotator write-authority parity.** The remote-annotator `UploadFile` path
  now enforces the same reasoning/metadata whitelist and `source = "human"`
  forcing as the local handler, so annotators can neither write arbitrary files
  nor mislabel the reasoning source.

### Changed

- **Resilient installer.** `install.sh` resolves the latest version via the
  GitHub release redirect rather than the rate-limited API; set `GITHUB_TOKEN` to
  raise the limit on the API-based paths (`install.ps1`, `memory-archive update`).
- The release pipeline publishes a `SHA256SUMS` asset alongside the five platform
  archives.
- **Cleaner installer/updater output.** `install.sh`, `install.ps1`, and
  `memory-archive update` no longer flood the terminal with pip's resolver
  output — the wheel install runs quietly behind a spinner and its log is shown
  only on failure — and the download renders a single progress bar instead of
  the multi-line transfer table.

### Fixed

- **Complete uninstall.** `memory-archive uninstall` now removes the
  `memory-archive` CLI launcher and any update-installed `lib/`, leaving no
  orphaned files. It also uninstalls the Python distribution under its real name
  (`ma-app`), so the package is no longer left behind in site-packages.
- **`memory-archive update` reinstalls the Python package.** The updater matched
  the bundled wheel by an outdated name and silently skipped the Python package;
  it now matches by extension, as the installers do.
- **Windows installer parity.** `install.ps1` now resolves the latest release via
  the GitHub redirect (honouring `GITHUB_TOKEN`) instead of the rate-limited API,
  and invokes pip through the interpreter correctly so the `memory-archive` CLI
  installs on Windows.
- The generated `INSTALL.md` installs the bundled wheel with a name-agnostic
  glob.

### Notes

- 0.11.0 is the first release to ship a `SHA256SUMS` manifest; the hardened
  installers require it, so they can install 0.11.0 and later but not earlier
  tags.
