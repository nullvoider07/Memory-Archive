# Changelog

All notable changes to Memory Archive are documented in this file. This project
adheres to [Semantic Versioning](https://semver.org/).

## [0.3.0] — 2026-07-28

Control-Center 1.2.1 support, and a version gate.

Control-Center 1.2.1 changes no part of the contract Memory Archive consumes: the
`.proto` is untouched, `crates/server/` is not in the 1.2.0..1.2.1 diff, and the
typed-text redaction added in that release is confined to the controller's console
output and its local metrics history — `human_command` on the wire still carries the
full text, so recorded steps are unaffected. No compatibility change was required;
the work below is verification and the parity items 1.2.1 raised.

### Security

- **`cargo audit`: 0 vulnerabilities.** The 13 warnings deferred at 0.2.0 were
  triaged by reachability rather than carried forward. `imageproc` 0.25.0 → 0.25.1
  clears three unsoundness advisories (RUSTSEC-2026-0115/0116/0117); none were
  reachable — they affect `binary_descriptors::brief` and
  `geometric_transformations::warp_into{,_with}`, while `vision/marker.rs` uses
  only `drawing::*` and `rect::Rect` — but the patch release is free. Of the ten
  that remain, `anyhow`'s `Error::downcast_mut` unsoundness (RUSTSEC-2026-0190) is
  unreachable: the workspace never calls `downcast*`. `rand`'s unsoundness
  (RUSTSEC-2026-0097) requires a custom `log` logger that calls `rand::rng()`;
  there is none. The rest are unmaintained or yanked transitive crates
  (`core2`, `paste`, `rustls-pemfile`, `ttf-parser`, `spin`) with no advisory
  against them.

- **A pinned Control-Center CA now excludes the platform trust store.** With
  `control_center_tls_ca` set, `try_connect` built its TLS configuration as
  `ClientTlsConfig::new().with_enabled_roots()` and then *added* the configured CA.
  Both inputs are additive — `with_enabled_roots` sets `with_native_roots` and
  `ca_certificate` appends to the trust set — so every publicly trusted CA remained
  acceptable for the Control-Center name while the operator believed the connection
  was pinned to their own. The platform roots are now enabled only when no CA is
  configured. This is the counterpart to the downgrade Control-Center closed in
  1.2.1; the related failure mode there (an unreadable CA silently falling back to
  the system store) never applied here — that path has always been a hard error.

- **Staged Control-Center archives are verified before extraction.**
  `integration-tests/stage-cc-releases.sh` downloaded a release archive and handed
  it straight to `tar`. Control-Center publishes a `SHA256SUMS` covering every
  archive from 1.2.1 onward; the script now checks the download against it and
  refuses to extract on a mismatch. Releases before 1.2.1 predate checksum
  publishing and are reported as extracted unverified rather than failing. Proven
  in both directions: the pristine archive verifies, a single appended byte is
  rejected, and an empty checksum list fails closed rather than passing vacuously.

### Added

- **A Control-Center version gate.** `WatchStream` now reads `GetServerIdentity`
  before subscribing and refuses to record against a version outside the range
  this build has been verified against. The session is marked `incomplete` and
  `memory-archive start` exits non-zero, rather than the historical failure of a
  session that registers, reports `active` and records nothing.

  The gate refuses rather than adapts, deliberately. Protobuf already absorbs
  additive wire changes — an unknown field is ignored — so a new field needs no
  code. What cannot be absorbed is a change of *meaning* in an existing field:
  `position_captured` was a best-effort readback before 1.2.0 and a verified
  value from 1.2.0, identical on the wire. Nothing observable at runtime
  separates them, so nothing at runtime can adapt; the knowledge lives in a
  changelog a person read. Guessing would trade a loud failure for a silent one,
  and the artefact of guessing wrong is a corpus session that records
  confidently and wrongly, discovered long after the environment is gone.

  Two escapes, both explicit: `control_center_max_version` raises the ceiling
  without a rebuild, for a release whose notes confirm the command stream is
  unchanged — it only ever raises, so it cannot narrow the supported range;
  `control_center_allow_unsupported` bypasses the gate entirely and logs the
  refusal as a warning on every connect. A server that reports no version, or one
  that cannot be parsed, is **not** refused — Control-Center 1.0.0 predates this
  check and turning a working legacy setup into a hard failure would be a
  regression, not a safeguard.

  Verified end-to-end against real binaries by temporarily lowering the ceiling
  to 1.1.0 and pointing ma-core at a 1.2.1 server: unset → exit 1, no
  subscription, session `incomplete`; `control_center_max_version = "1.2.1"` →
  subscribes cleanly; `control_center_allow_unsupported = true` → subscribes and
  still logs the refusal.

- **`actuation_server_version` in session metadata**, alongside the existing
  `actuation_agent_version`. The two halves of Control-Center install
  independently and 1.2.1 warns that a mismatched pair fails actuation closed
  while still reporting success, so a mismatch is now logged once when the first
  command event arrives.

### Changed

- **The compatibility matrix is discovered, not written down.**
  `stage-cc-releases.sh` enumerates published releases from the GitHub API and
  the tests parameterise over whatever is staged, so a new Control-Center release
  joins the matrix by being staged rather than by editing a list. A hard-coded
  array silently stops covering the newest release, which is precisely the case
  these tests exist to catch. Explicit arguments still override, for bisecting.
  12 integration tests (was 9) across real 1.0.0, 1.1.0, 1.2.0 and 1.2.1 servers.

- **`release.yml` refuses a tag that disagrees with the source.** Nothing compared
  them, so a tag pushed before the version bump would publish archives labelled
  with one version containing binaries reporting another. The new `verify-version`
  job gates the whole matrix on the tag matching `Cargo.toml`,
  `ma-app/pyproject.toml`, `ma_app/__init__.py` and the `ma-core` pin in
  `Cargo.lock` — the last because a stale lockfile breaks every `--locked` build,
  and it went stale during this release.

- `control_center_max_version` and `control_center_allow_unsupported` are exposed
  through `memory-archive config` and `config --show`, matching the other
  Control-Center settings.

## [0.2.0] — 2026-07-27

Control-Center 1.1.0+ compatibility, and the silent capture failure it exposed.

### Fixed

- **No steps were recorded against Control-Center 1.1.0 or 1.2.0.** Control-Center
  1.1.0 made TLS mandatory on the gRPC listener and added
  `require_scope(&claims, "monitor")` to `WatchCommands`. `WatchStream::try_connect`
  called `ControlServiceClient::connect` with no TLS configuration and issued
  `watch_commands` with no credentials, so the subscription was refused and the
  capture loop exited. Sessions still registered, still reported `status: active`,
  and recorded nothing. Confirmed by inspection of the live process: with a watch
  running, ma-core held no socket to the Control-Center port at all. The stream now
  presents a bearer token and negotiates the transport. Verified end-to-end against
  real 1.0.0 and 1.2.0 servers from a single unchanged configuration.

- **A watch that could not subscribe reported success.** The IPC handler spawned the
  capture loop and immediately answered `WatchStarted` — the gRPC subscription
  happens inside that task and had not been attempted yet, so the reply could not
  reflect it. On failure the loop logged an error and returned, leaving the session
  at `active` in Redis with a dead loop behind it and nothing surfaced to the
  operator. The loop now signals readiness over a oneshot channel; the handler waits
  for it, answers `WATCH_START_FAILED` with the underlying cause, and marks the
  session `incomplete` (the status the shutdown path already uses for an interrupted
  session) rather than stranding it `active`. `memory-archive start` exits non-zero.

- **Click markers could be drawn where the pointer never was.** The vision pipeline
  marked an at-frame for every mouse event using `mouse_x`/`mouse_y` without reading
  `position_captured`. Those fields are non-optional, so an uncaptured position
  arrives as `(0, 0)` — a valid screen coordinate, the top-left corner — and the
  marker landed there. Control-Center 1.2.0 reports `position_captured=false`
  whenever the cursor readback cannot be verified, which makes an unverified
  position ordinary rather than exceptional. The frame is still captured; only the
  marker is withheld.

- **`memory-archive server logs` presented stale output as current.** The command
  reads `~/.memory-archive/ma-core.log`, which only daemon mode writes; a foreground
  ma-core logs to its own stdout. With a live foreground daemon the command printed
  a previous run's file with no indication of its age — during this investigation it
  displayed five-day-old content while the failure being diagnosed went unlogged. It
  now warns when the file predates the running process.

- **Installer PATH persistence.** `install.sh` and `install.ps1` decided whether to
  write the PATH entry by checking the current process `$PATH` (or `$env:PATH`). A
  shell that had run an earlier install already had the directory exported, so the
  entry was skipped and every fresh terminal lacked it. Persistence is now driven by
  the shell rc file and the Windows registry.

### Added

- `control_center_token` — JWT presented to Control-Center. The `monitor` scope is
  required by 1.1.0+ and ignored by 1.0.0, so it is safe to set unconditionally.
- `control_center_tls_ca` — PEM CA that signed the Control-Center certificate.
  Empty uses the system trust store; a private CA (what `control-center gen-certs`
  produces) must be named explicitly.
- `control_center_security` — transport policy: `auto` (default), `strict`,
  `legacy`. Under `auto` the stream tries TLS first and falls back to plaintext only
  on a transport-level failure, logging a warning that names the downgrade. A
  rejected token never triggers a fallback, because the server answered and a weaker
  transport cannot help. `strict` refuses any downgrade; `legacy` forces plaintext.
- `actuation_agent_version` and `actuation_transport` in `metadata.json`.
  `position_captured` means "best-effort readback" before Control-Center 1.2.0 and
  "verified, or false" from 1.2.0 onward, so recorded coordinates cannot be
  interpreted correctly without knowing which produced them. Empty for sessions
  recorded before this release.
- `--control-center-token`, `--control-center-tls-ca` and
  `--control-center-security` on `memory-archive config`.
- **A Control-Center compatibility test suite** under `integration-tests/`, run
  against real released `control-center-server` binaries (1.0.0, 1.1.0, 1.2.0)
  rather than mocks — a mock would only assert that the mock matches our belief
  about Control-Center, and that belief is exactly what was wrong. It covers the
  negotiated transport per version, the loud-failure path, the credential-
  withholding rule, and that an `http://` address still reaches an upgraded
  server. Tests pin Redis DB 15 and refuse to flush any other, so they cannot
  reach the live session registry in DB 0. `integration-tests/stage-cc-releases.sh`
  fetches the server binaries; everything skips cleanly when they are absent.
- **CI** (`.github/workflows/ci.yml`). `release.yml` builds artefacts but runs no
  tests and checks no advisories, which is why both the Control-Center 1.1.0
  regression and three certificate-validation advisories went unnoticed. Every
  push now builds, clippies, runs the Rust and Python suites against a Redis
  service, and fails on any `cargo audit` advisory. The compatibility matrix is a
  `workflow_dispatch` job, since it downloads three Control-Center releases.

### Security

- **The Control-Center token is never sent over an unencrypted connection that was
  not asked for.** Under `auto`, a downgrade to plaintext withholds the credential
  and logs a warning. An automatic downgrade is not consent: an attacker able to
  disrupt the TLS handshake could otherwise force the fallback and collect a
  `monitor`-scoped token in the clear, and that token subscribes to
  `WatchCommands` — every keystroke a session records, including anything typed
  into a password field while capture is running. Withholding costs nothing
  against a genuine 1.0.0 server, which does not check the token; an operator who
  needs the token over plaintext (1.1.0+ with `CC_ALLOW_INSECURE=true`) opts in
  with `control_center_security = "legacy"`.

- **Certificate-verification advisories on the new TLS path closed.** A
  `cargo audit` of the dependency tree — the first this project has had — reported
  nine advisories, six of them in `rustls-webpki`, the library that validates the
  Control-Center server certificate. Two are certificate-validation bypasses
  (`RUSTSEC-2026-0099`, name constraints accepted for a wildcard certificate;
  `RUSTSEC-2026-0098`, URI name constraints incorrectly accepted) and one is a
  reachable panic in CRL parsing (`RUSTSEC-2026-0104`). This release is what puts
  that code on the hot path: before it, the Control-Center connection never
  verified a certificate at all. `rustls-webpki` moves to `0.103.13`, with
  `crossbeam-epoch` `0.9.20` and `quinn-proto` `0.11.15`.

  The last three were the same `rustls-webpki` defects in `0.101.7`, and they were
  a **feature flag, not an out-of-date version**. `aws-sdk-s3`'s default features
  include `"rustls"`, which selects the legacy hyper-0.14 connector through
  `aws-smithy-runtime/tls-rustls` →
  `aws-smithy-http-client/legacy-rustls-ring`, dragging in `rustls 0.21` and with
  it the vulnerable `rustls-webpki`. The modern connector is a separate feature
  (`default-https-client` → `rustls-aws-lc` → `rustls 0.23`) and is the one
  `S3Backend` actually uses, since it builds its client through
  `aws_config::defaults`. `aws-sdk-s3` is now declared with
  `default-features = false` and its default list minus `"rustls"`.

  **`cargo audit` reports zero vulnerabilities.** No toolchain bump and no
  dependency version bumps were needed. An earlier draft of this entry claimed
  clearing these required a newer `aws-smithy` on Rust 1.94.1; that was wrong — a
  dry-run update of the whole AWS stack leaves `rustls`, `hyper-rustls` and
  `webpki` untouched, so the bump would have cost a great deal and fixed nothing.

  Thirteen non-vulnerability warnings remain untriaged (4 unmaintained, 7
  unsound, 2 yanked).

- **`CcEndpoint` no longer derives `Debug`.** The struct holds the token, and a
  derived impl meant any future `?cc` at a `tracing` call site would publish the
  credential in plaintext. `Debug` is now written by hand and renders the token as
  `(redacted)` or `(unset)`. A test asserts the token cannot appear.

### Notes

- **The URL scheme no longer pins the transport under `auto`.** An address written
  `http://` is still attempted over TLS first, so an existing configuration reaches
  an upgraded server without being edited. Use `strict` or `legacy` to pin.
- **No new dependencies.** Enabling tonic's `tls`/`tls-roots` features added no
  packages to `Cargo.lock` — the rustls stack was already present through the cloud
  SDKs — so the cross-compilation matrix is unaffected.
- **Existing configurations need no migration.** All three keys default when absent.
- **Sessions recorded before this release remain valid.** S001-era captures were
  produced against Control-Center 1.0.0 and are unaffected.

## [0.13.2] — 2026-07-14

Critical updater hotfix: `memory-archive update` was breaking every install it
touched.

### Fixed

- **`memory-archive update` destroyed the working CLI install.** The Python
  package reinstall step used `pip install --prefix ~/.memory-archive/lib
  --upgrade`. Two things combined to break every update: (1) `--prefix` installs
  into a site-packages tree the interpreter never adds to `sys.path`, so the new
  package was unimportable; (2) pip's own `--upgrade` resolver still detected the
  *existing* on-path install (from the original `--user`/system install done by
  `install.sh`) and uninstalled it first. Net effect: the working install was
  deleted and replaced with one `memory-archive` could no longer import —
  `ModuleNotFoundError: No module named 'ma_app'` immediately after the updater
  printed `[OK] ma-app updated`. `install.sh` already deliberately avoids
  `--prefix` for exactly this reason (see its inline comment); `update` now
  mirrors it: a plain `pip install [--user]` into the interpreter's normal
  site-packages, with the entry-point script located via `sysconfig` (as
  `install.sh` does) rather than assumed under the `--prefix` layout. Verified by
  simulating the fixed install step directly (`memory-archive --version` stays
  resolvable immediately afterward — the exact step that failed before) and by
  invoking it as the currently-installed 0.13.2 package, i.e. the code path a
  genuine future `update` runs.
- **`memory-archive uninstall`** now treats a leftover `MA_HOME/lib` directory
  (the artifact of the broken `--prefix` install above) as migration cleanup for
  anyone who ran an update before this fix, rather than the expected install
  layout.

### Upgrade note

If you already ran `memory-archive update` on a version before this fix, your
install is currently broken (`ModuleNotFoundError: No module named 'ma_app'`),
and **running `update` again will not repair it** — the broken, already-installed
copy is what decides how the new version gets installed, so it repeats the same
`--prefix` mistake regardless of the wheel it downloads. Reinstall once with
`install.sh` / `install.ps1`, or manually: `pip install --user --upgrade
ma_app-*.whl` from a downloaded release archive. Every `update` after that one
manual step uses the fixed code and works normally.

## [0.13.1] — 2026-07-14

Registry-recovery patch: a finished recording can no longer be demoted to
incomplete by the startup sweep when Redis state is stale, plus PID-file and
status-routing fixes found in the same audit.

### Security

- **Unix IPC socket locked to the owner (0600).** The Unix-socket transport
  carries no per-message token — reachability *is* authorization — yet the socket
  was created with the process umask (0775 under a group-writable umask), so a
  same-group local user who could reach the socket path could issue
  unauthenticated admin commands (register, delete, done). The daemon now sets the
  socket to 0600 and its parent directory to 0700 before the accept loop starts,
  so ownership is the boundary regardless of umask or where `ipc_socket_path`
  points. TCP IPC is unaffected (already token-gated over TLS 1.3, and refuses to
  start without `MA_IPC_TOKEN`). Added a `validate_session_id` unit test covering
  empty/`.`/`..`/embedded-traversal/separator/absolute vectors.

### Fixed

- **Startup sweep no longer demotes finished recordings.** If the host goes down
  uncleanly after `done` completes, Redis can restart from a snapshot taken
  before the status flip and re-list the session as `active`. The sweep then
  marked the session incomplete and renamed its memory directory — even though
  `metadata.json` was frozen at `complete` with every frame flushed. The sweep
  now treats an on-disk `complete` status as authoritative and restores the
  session to `pending_annotation` instead of touching the directory. (Observed
  live: a completed capture was demoted after an overnight power loss; the
  recording itself was intact.)

- **PID file written where `server stop` looks for it.** ma-core wrote
  `ma-core.pid` next to the storage path (inside the capture tree) while the CLI
  reads `~/.memory-archive/ma-core.pid`, so `server stop` always failed with "No
  ma-core.pid file". The daemon now writes the PID file next to `config.json`,
  and removes it on shutdown.

- **Stale-PID takeover verifies the target process.** On startup, ma-core
  SIGTERM'd whatever PID the stale PID file named. After a reboot that PID can
  belong to an unrelated process. The takeover now confirms the process is
  actually `ma-core` before signalling, and ignores the file otherwise.

- **`memory_path` follows the "(incomplete)" rename.** `mark_incomplete` renames
  the memory directory but the Redis record kept the old path, so any later
  lookup (annotate, compile, delete) resolved a directory that no longer
  existed. All rename sites now update the record's `memory_path`.

- **Manual sessions finished via the direct `Done` path are annotatable.** The
  no-watch-loop `Done` branch routed manual-mode sessions to
  `pending_human_annotation`, which the annotation loader rejects — the session
  became un-annotatable. The branch now mirrors the watch-loop path: status is
  chosen by reasoning degradation, not session mode.

- **`convert` unit tests brought in line with shipped behavior.** Three tests
  asserted pre-normalization output (`Press: ^c`, `Press: Return`, synthesized
  `scroll-down`); the shipped converter — and the recorded corpus — use humanized
  modifiers (`Press: Ctrl+c`), cross-OS key labels (`Press: Enter`), and raw
  passthrough for unknown action types.

- **Redundant comparison in the pricing-registry age alert.** The stale-manifest
  check read `age > 0.0 && age > 604_800.0`; the first clause is implied by the
  second (a `deny`-level clippy lint). Simplified to `age > 604_800.0`; behavior
  unchanged.

### Operational note

The registry's annotation sub-lifecycle lives only in Redis; with default RDB
snapshotting, an unclean host shutdown can roll it back by up to an hour. Enable
AOF (`appendonly yes`) on the local Redis so registry updates survive power
loss.

## [0.13.0] — 2026-07-11

Capture-fidelity and crash-recovery release: drag-interaction frames, an explicit
compile-stage finalize key, and a fix for interrupted annotations being locked out
after a restart.

### Added

- **Explicit `Ctrl+D` to finalize a memory.** The compile-stage editor now has a
  dedicated finalize action: `Ctrl+D` saves, asks for confirmation, and marks the
  session `complete`. Previously the only way out of the editor was `Ctrl+Q`, which
  silently finalized — the same keystroke that means "quit without saving progress"
  during annotation. See the matching change to `Ctrl+Q` below.

- **Frames for every mouse interaction.** All mouse subtypes now capture
  before/at/after screenshots with the cursor marked at the acted-on position:
  the click point for `left`/`right`/`double`/`middle`/`triple`, the destination
  for `move` and `drag` (which reports its endpoint as the captured position), the
  press/release point for `hold`/`release`, and the pointer position for
  `scroll_up`/`scroll_down`. Previously only `left`/`right`/`double`/`move`
  captured frames, so `hold`, `release`, `drag`, `middle`, `triple`, and scroll
  steps were frameless and landed in the corpus without visual context — leaving
  drag-and-drop, press-and-hold, and scroll interactions incompletely recorded.
  `vision::decide` now fetches for the whole `mouse` action type via a catch-all,
  so any future mouse subtype is captured rather than silently dropped. (The
  `position` action type is a cursor-position query, not a state-changing action,
  and stays frameless.) Applies to sessions captured on 0.13.0 and later.

### Changed

- **`Ctrl+Q` at the compile stage no longer finalizes.** It now saves, confirms,
  and exits leaving the session at `pending_compilation` — resumable with
  `memory-archive compile` — mirroring what `Ctrl+Q` already means during
  annotation (exit without completing). Finalizing is now solely `Ctrl+D`. To
  support lossless resume, `run_compile` no longer overwrites an existing
  `memory.md`: a resumed compile reopens the saved draft (with its notes and
  edge-cases) instead of regenerating a blank scaffold. Delete `memory.md` to force
  a fresh scaffold.

### Fixed

- **Interrupted annotations are resumable after a restart.** The startup
  reconcile sweep mirrored `metadata.json`'s `status` into Redis, but that field
  is frozen at `complete` once capture finishes and never tracks the
  annotation/compilation lifecycle. An `annotating` session that survived an
  unclean ma-core exit (power cut, reboot) was therefore demoted to `complete`,
  after which `memory-archive annotate` refused to load it
  (`INVALID_STATUS`). The sweep now trusts the live Redis status when metadata
  reads `complete`: an interrupted annotation is restored to `pending_annotation`
  so the TUI resumes from `reasoning.jsonl`, while `pending_compilation` and
  `reasoning_degraded` sessions are left untouched. Metadata is mirrored only when
  it carries a genuine resumable status (cloud-primary Kafka-replay crash
  recovery).

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
