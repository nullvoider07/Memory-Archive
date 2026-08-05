# Changelog

All notable changes to Memory Archive are documented in this file. This project
adheres to [Semantic Versioning](https://semver.org/).

## [0.3.5] — 2026-08-05

### Fixed

- **Finalizing the compiled memory works again, on `Ctrl+F`.** `Ctrl+D` never
  reached the finalize action. Textual resolves a key against the focused
  widget's bindings before the screen's, and the compile editor is a `TextArea`,
  which binds `delete,ctrl+d` to `delete_right` — so every press **deleted the
  character to the right of the cursor** and the confirm dialog never opened. A
  draft was being edited by the key meant to lock it, silently, with no error
  and no visible failure; reproduced against the real `CompilerApp`, where
  `Ctrl+D` turned `# Overview` into ` Overview`.

  The action is now `Ctrl+F`, which nothing in `TextArea`, `Input`, `Button`,
  `App` or `Screen` claims, and it is bound with `priority=True` so a focused
  editor cannot shadow it even if a future Textual does claim it. The priority
  binding lives on `CompilerScreen` rather than `CompilerApp` on purpose:
  priority bindings resolve against the *top* screen's binding chain, so an App
  copy would still fire while the confirm overlay was open and stack a second
  dialog.

  A regression test asserts the invariant rather than the instance — no
  non-priority binding on `CompilerScreen` or `AnnotationScreen` may share a key
  with `TextArea` or `Input`, because such a binding can never fire and nothing
  reports that it did not.

### Changed

- **The compile status bar and the README report the finalize key correctly.**
  The README additionally described finalizing as a side effect of `Ctrl+Q` and
  saving, in four places, which stopped being true when the explicit finalize
  was introduced. `Ctrl+Q` saves the draft and leaves the session at
  `pending_compilation`; only `Ctrl+F` finalizes.

## [0.3.4] — 2026-08-05

Five defects found during an annotation and compile pass, plus copy-on-select.

### Fixed

- **`memory-archive update` can now replace a running `ma-core`.** The POSIX
  branch copied the new binary over the destination in place, which Linux
  refuses with `ETXTBSY` when the target is a running executable — and `ma-core`
  normally is, so the update aborted unless the daemon was stopped first. The
  new binary is written to a sibling temp file, made executable (and on macOS
  de-quarantined and signed) there, then `os.replace`d over the target:
  `rename(2)` over a running binary is permitted and the running process keeps
  its old inode. This mirrors what the Windows path already did.
- **`ma-core` removes its Unix socket on shutdown.** The SIGTERM handler removed
  the PID file but left `ma.sock` behind. Startup masked it by unlinking any
  existing socket before binding, so nothing broke — but a leftover socket reads
  as a running daemon and has repeatedly misdirected debugging. Both files are
  now removed only if the PID file still names the exiting process: startup
  SIGTERMs an existing `ma-core` and waits just 500 ms before writing its own PID
  file and binding, while the handler's per-session Redis and storage I/O can
  outlast that window, so an unconditional removal could delete the successor's
  socket. That race already existed for the PID file.
- **Ctrl+N returns to a step it has passed.** `_advance_to_next_pending` scanned
  forward only, so a step left pending behind the cursor — what happens whenever
  annotation starts part-way through a session — was unreachable by keyboard for
  the rest of the session. Worse, at the last step the forward scan found
  nothing, `all_done` was `False` because of that pending step, and the fallback
  branch performed no navigation, showed no completion prompt and printed no
  message: Ctrl+N looked broken and the session could not be finished from the
  keyboard. The scan now wraps to the start, announces the backwards jump, and
  the remaining fallback says why it did not move.
- **Confirm dialogs respond to Left/Right.** `CompilerQuitOverlay`,
  `CompilerFinalizeOverlay`, `QuitConfirmOverlay` and `CrashRecoveryOverlay`
  bound only Escape and a letter, and Textual moves focus with Tab/Shift+Tab, so
  the arrow keys did nothing. Arrow navigation with wrap-around — previously
  implemented once in `AnnotationCompleteOverlay` — is now a shared
  `ButtonNavModal` base used by all five dialogs.
- **The focused button is now the one that looks selected.** Confirm dialogs
  focused Cancel while rendering Quit with a `variant` colour fill, so the
  loudest button was not the one Enter would activate — the dangerous direction
  on a destructive choice. The fills are gone; focus is the only selection
  signal, and it stays on the non-destructive button.
- **Dialog key hints are visible again.** Button labels are parsed as content
  markup, so `Button("Quit  [q]")` had `[q]` consumed as an unknown tag and
  rendered as `Quit` — hiding the only key that closed the dialog. Eleven buttons
  across both screens were affected. Labels are now built as `Content`, which
  bypasses the markup parser.

### Added

- **Selecting text with the mouse copies it, in every region of the TUI.** No
  region could be copied before. A terminal application normally leaves the
  mouse to the terminal emulator, whose own drag-to-select does the copying;
  Textual instead asks the terminal for mouse reporting on startup
  (`SET_ANY_EVENT_MOUSE` and friends, in its driver), which takes selection away
  from the terminal and makes it the application's problem.

  Textual then implements selection twice, and both models have to be covered:

  - *Screen-level*, for static content — `Screen.selections` is populated on
    drag and `Screen.get_selected_text()` reads it, but nothing is bound to
    `action_copy_text`, so a selection was made and then dropped.
  - *Widget-owned*, for editable content — `TextArea` and `Input` call
    `capture_mouse()` on mouse-down, and `Screen._forward_event` only opens a
    screen-level selection while nothing has captured the mouse. Inside the
    compile editor, the reasoning editor and the jump box the screen therefore
    records nothing; the text is selected, in that widget's own `selected_text`.

  Both apps now read the screen's selection first and the drag's origin widget
  second (recorded on mouse-down, because the capture is released before the
  event arrives and a drag may end over a different widget than it began on).

  Delivery goes to two destinations: OSC 52 via `App.copy_to_clipboard` for
  terminals that support it, and an external helper (`wl-copy`, `xclip`, `xsel`,
  `pbcopy`, `clip`) for the system clipboard proper, which is what makes the
  text pastable into other applications. `App.copy_to_clipboard` is overridden
  rather than called, so every copy Textual performs internally — Ctrl+C in a
  `TextArea` or `Input`, and both cut actions — reaches the OS clipboard too.
  An empty selection — every plain click posts the same event — copies nothing,
  so clicking never clears the clipboard.

  The helper runs on a single coalescing background thread. Spawning it takes
  ~64 ms (114 ms worst case) and the handler runs on the event loop, so an
  inline call stalls the UI for the length of every drag; a thread per selection
  is worse still — 300 back-to-back selections left 137 helpers running at once
  and the clipboard holding selection **150** of 299, because concurrent helpers
  finish out of order. One helper at a time, newest text wins.

### Changed

- **The reasoning editor's Ctrl+C and Ctrl+V use the shared clipboard path.**
  It carried its own copy of the helper lookup, which ran the helper inline on
  the event loop — the stall that the coalescing worker exists to avoid — and
  drifted from the version everything else used. Ctrl+V now reads the OS
  clipboard first and falls back to the in-app clipboard, so text copied in
  another application pastes into the reasoning field.

### Security

- **The updater's temp file can no longer be redirected by a planted symlink.**
  The binary replacement wrote to a sibling path derived from the process id,
  and `shutil.copy2` follows symlinks — so anyone able to create a file in the
  install directory could pre-plant `ma-core.new-<pid>` pointing elsewhere and
  have the update overwrite that target instead. The temp file is now created
  with `tempfile.mkstemp` (O_EXCL, unguessable name, mode 0600), and the
  source's mode is applied afterwards. Demonstrated both ways: the previous
  shape overwrites the symlink target, the current one leaves it untouched.

### Changed

- The finalize dialog no longer claims a "90-day retention". No session status
  has carried a TTL since 0.3.2; the text was left over from the removed policy.
- **The IPC handler chain takes one `IpcServices` struct instead of seven
  positional services.** `registry`, `config`, `done_handles`, `push_handles`,
  `kafka_session_map`, `storage_router` and `reasoning_maps` were threaded
  individually through `serve` → `handle_connection` →
  `handle_connection_inner` → `handle_message`, and again through `serve_tcp` →
  `handle_tcp_connection` → `handle_annotator_connection`, putting every
  signature past ten parameters. Cloning is as cheap as before — each field is a
  handle, so a per-connection clone shares the same state. Function bodies are
  unchanged: each destructures into the names it already used.
- `run_watch_loop` takes a `WatchLoopArgs`, and `build_automated_entry` takes an
  `AutomatedReasoning` struct. The latter had eleven positional parameters,
  five of them consecutive `Option<String>`/`Option<u32>`, where a transposed
  pair would type-check and write the wrong provenance into `reasoning.jsonl`.
- `ma-core` is clippy-clean: 39 warnings to 0. Most were mechanical
  (`needless_borrow`, `useless_conversion`, `or_default`); the rest are the
  refactors above. Three are silenced with a documented reason rather than
  changed: the `collapsible_if` family, because every site is an
  `if cond { if let … }` whose collapsed form needs let-chains (Rust 1.88) and
  the documented build requirement is Rust 1.85; a test fixture builder in
  `convert`; and `large_enum_variant` on `InboundMessage`, where boxing the
  largest variant would alter the shape serde derives at the protocol boundary
  with ma-app for a value built once per request.

### Tests

- `test_button_nav.py`, `test_advance_to_next_pending.py`, `test_clipboard.py`
  and `test_updater_binary_replace.py` — 49 new cases. The dialogs are driven
  headless through Textual's own pilot (`asyncio.run` drives the async pilot, so
  no `pytest-asyncio` dependency is added), and the updater test reproduces
  `ETXTBSY` against a genuinely running ELF before asserting the fix clears it.
- A stress pass covered 30 daemon start/stop cycles, 15 takeover races, 400
  randomised annotation sessions driven by Ctrl+N alone, 300 back-to-back
  selections, and 400 binary replacements (200 sequential against a running
  process, 200 across 8 concurrent threads).
- `test_it_replaces_a_binary_that_is_currently_running` was flaky as first
  written: it copied `/usr/bin/sleep` to a file named `bin`, and coreutils is a
  multi-call binary that dispatches on `argv[0]`, so the child exited
  immediately with "unknown program". A dead child holds no text image, so the
  `ETXTBSY` assertion was passing only by racing the exit. The copy keeps its
  name now, and the test asserts the process is alive before relying on it.

## [0.3.3] — 2026-08-04

The annotation TUI stacked a new image viewer on every open.

### Fixed

- **Opening a step's image now replaces the previous viewer instead of stacking
  another one.** `ImageReview._open_image` spawned the viewer with
  `subprocess.Popen` and never kept the handle, so nothing tracked or closed it.
  Every Enter or click launched *another* fullscreen `feh`. The visible symptoms
  were the image appearing to open "in a new tab", and Escape appearing not to
  close it — feh binds Escape to quit but quits only the **focused** instance, so
  closing the top of a stack of identical fullscreen windows leaves an identical
  image on screen. That is deterministic in how many times the step was opened,
  not intermittent. The second window was never useful: the command already
  passes all three frames (`before`, `at`, `after`) with `--start-at`, so one
  instance holds the whole step.
- **The viewer no longer outlives the TUI.** `ImageReview` had no `on_unmount`,
  so a viewer left open when the annotator quit kept running unattended.
- **Exited viewers are reaped.** Without a retained handle the child stayed a
  zombie until the next spawn happened to clear it. `poll()`/`wait()` on the
  tracked process now reap it directly.

The macOS and Windows launchers are deliberately left untracked: `open` and
`os.startfile` hand the file to a separate application and exit immediately, so
the handle refers to the launcher rather than the window, and both Preview and
the Windows shell handler reuse their own window on a repeated open.

### Notes

- Running `pytest ma-app/tests` without installing the package first tests the
  *installed* `ma_app`, not the working tree. CI installs `./ma-app` before
  running them, so it is unaffected; locally, prefix with `PYTHONPATH=ma-app` or
  reinstall before trusting a pass.

## [0.3.2] — 2026-08-04

Session registry records no longer expire, and deletion can no longer remove a
recording it does not own.

A 101-session corpus was found to be missing 21 registry records — every macOS
session from its first fortnight. The capture data was intact on disk in all 21
cases; only the Redis Hash was gone, and since `annotate`, `compile` and `status`
all resolve a session through that record, the recordings were unreachable. There
is no import path to rebuild one, so they had to be reconstructed by hand from
each `metadata.json`.

The cause was a TTL, not a fault. Session records carried expiries of 7 days while
pending or annotating, 30 days when incomplete, and 90 days once complete, so the
boundary fell exactly where the recording dates crossed the 7-day line. At the
point of diagnosis the next record was 24 minutes from expiring and 22 more were
inside a day.

### Fixed

- **Session registry records no longer expire.** `SessionStatus::ttl_seconds()`
  returns `None` for every status. Expiry could never reclaim a session's bytes —
  those are the frames on disk, not the 1.8 KB record — so it only dropped the
  pointer and stranded the payload. Records are removed explicitly, through
  `delete_session`, which drops the Hash, every index set, the stored objects and
  the directory together.
- **Finalize no longer sets its own TTL.** `FinalizeMemory` applied a hardcoded 90
  days independently of `ttl_seconds()`, so changing the status table alone would
  have left every finalized memory expiring on the old schedule.
- **`update_status` now clears a stale TTL.** It previously only ever *set* one, so
  returning `None` could not remove an expiry a key already carried. Added
  `SessionRegistry::clear_ttl` (Redis `PERSIST`) on the `None` branch — this is
  also what makes the fix retroactive, since existing records shed their old
  expiry the next time their status is written rather than needing a manual sweep.
- **Deletion no longer purges a directory it does not own.** `memory_path` does not
  uniquely identify a session: when a scrapped take is re-recorded under the same
  `memory_name`, the replacement occupies the same path while the abandoned record
  keeps pointing at it. `purge_memory_dir` therefore took the *successful*
  recording. It now requires the directory's `metadata.json` to name the session
  being deleted, and leaves anything it cannot prove it owns in place. Two live
  examples were found in the affected corpus, each aimed at a complete recording.
  Ownership is read as untyped JSON rather than through `metadata::read`, so a
  session written before a field was added to `SessionMetadata` stays deletable.

### Added

- **Retention sweep for interrupted captures.** `Incomplete` is the one status with
  a retention bound, `INCOMPLETE_RETENTION_SECONDS`, at one year. It is enforced by
  a startup sweep rather than a TTL: key expiry runs no application code, so it
  could only ever drop the record and orphan the frames. The sweep removes the
  record, the stored objects and the directory in one operation, and retries on the
  next start if any step fails. `SessionRegistry::list_incomplete_before` scans
  `session:*` rather than reading an index set, since `Incomplete` has none and
  sessions marked by earlier versions were never indexed.

### Tests

- **Registry unit tests moved to Redis DB 15.** They ran against DB 0 — the live
  session registry — relying on random UUIDs and a `cleanup()` at the end of each
  test. The first run of this release proved that insufficient: the stale
  `Annotating` TTL assertion panicked before its cleanup and left an orphan record
  and three index-set memberships behind in real corpus data. The DB index now
  matches `REDIS_TEST_DB` in `integration-tests/harness.py`.
- `test_annotating_status_uses_index_and_ttl` asserted the 7-day expiry this
  release removes. Renamed and inverted: an annotating session must now carry
  `TTL -1`.
- Added coverage for the two behaviours this release introduces —
  `update_status` clearing a pre-existing TTL, and `list_incomplete_before`
  selecting only `incomplete` records that are both past the cutoff and datable.

### Notes

- The sweep runs at startup only. A process running longer than the retention
  period will not clean up until it restarts.
- Records that cannot be dated — no readable `updated_at` — are skipped rather than
  deleted. Retention never removes a session it cannot age.
- `cargo audit`: 0 vulnerabilities, and the same 10 allowed warnings triaged at
  0.3.1 — unchanged, since no dependency moved in this release.

## [0.3.1] — 2026-07-29

Control-Center 1.2.2 support, and a converter panic that voided a capture session.

1.2.2 is the first Control-Center release since the gate landed where
`crates/server/` **is** in the diff, so the review was not a formality. The findings:
the `.proto` is untouched, so the wire shape is identical; the `CommandEvent`
construction in `crates/server/src/main.rs` is not in the diff, so recorded events
are built exactly as before; and the server changes are command-lifecycle
correctness — a queue reaper, and failing outstanding commands when an agent
disconnects — which alter `CommandResponse` for commands that were *never
delivered*, not the content of events for commands that ran. `position_captured`
keeps the meaning it took in 1.2.0: the settle before the first readback drops from
50 ms to 10 ms on macOS and Linux, but behind the verify-and-retry loop that is the
actual correctness mechanism, with 60/60 captures measured at both settings, and
Windows is unchanged. Token revocation (`CC_REVOKED_SUBJECTS`) is opt-in and empty
by default.

One thing 1.2.2 does change is recorded content, on one platform: a Windows chord
now reaches the record as the command that was issued rather than as the
AutoHotkey wire form it is actuated through.

### Security

- **Raising the ceiling does not bless a weaker server.** The gate is a safety
  control, so the review that raises it is part of the security surface, not
  separate from it. `HARDENED` in the compatibility matrix is derived as
  `staged_versions(minimum="1.1.0")`, so 1.2.2 was exercised for the TLS-only
  connection and for failing loudly rather than stranding the session when the
  token is missing; `LATEST` resolves to the newest staged release, which puts
  1.2.2 under the CA-pinning, no-downgrade, strict-policy and provenance tests as
  well. Control-Center's own changes in 1.2.2 add a revocation list
  (`CC_REVOKED_SUBJECTS`, opt-in and empty by default) and remove no control:
  TLS and the `monitor` scope on `WatchCommands` are still mandatory.

- **No remaining slice hazards in the key converter.** The panic fixed below was
  a backwards byte range, so the rest of `humanize_key` was audited for the same
  class. The three surviving slices are sound: `end` is now derived from a search
  that starts at `start`, and `{`, `}` and the modifier sigils are all ASCII, so
  every index is a valid UTF-8 boundary even for a key string carrying multi-byte
  text.

- `cargo audit`: 0 vulnerabilities, and the same 10 allowed warnings triaged at
  0.3.0 — no new advisories. The hermetic suite (`tests/run_all.sh`) passes 5/5,
  including the installer traversal and checksum guards on both `install.sh` and
  `install.ps1`.

### Fixed

- **A malformed key string panicked the converter and took the whole session with
  it.** `humanize_key` located the opening brace with `find('{')` but the closing
  one with `find('}')` searched from index 0, so a string whose `}` precedes its
  `{` produced `end < start` and a slice that panicked — *byte index range starts
  at 11 but ends at 9*. The panic killed the watch loop mid-session; Control-Center
  reported every step as successful, and the session finalised with **zero steps
  recorded**. One corpus session (S054) was lost this way before the cause was
  found. The scan now starts at the opening brace, which restores the behaviour the
  function's own doc comment promises: *never fails — unknown or malformed events
  fall back to the raw command*. A malformed key must cost a poor label, not a
  capture.

  The input that triggered it came from Control-Center, and both halves of that
  path are closed in 1.2.2 — the Windows controller no longer reports the AHK
  expansion, and the agent no longer strips the outer braces off an explicit
  down/up sequence. The fix here is still worth having on its own terms: this
  converter is the last thing standing between a strange key string and a
  destroyed session, and it does not get to choose its inputs.

- Quote and backslash fidelity through the key converter is now pinned by tests
  rather than assumed.

### Changed

- **The Control-Center ceiling is raised to 1.2.2** (`SUPPORTED_MAX`), verified by
  the compatibility matrix against real 1.0.0, 1.1.0, 1.2.0, 1.2.1 and 1.2.2 server
  binaries. The 1.2.2 archive is checksum-verified against the published
  `SHA256SUMS` before extraction; 1.0.0–1.2.0 predate checksum publishing and are
  staged unverified, as before. The matrix picked up 1.2.2 by discovery, with no
  edit to a version list — which is what that change in 0.3.0 was for. 14
  integration tests (was 12) and 103 unit tests (was 91), all passing;
  `cargo audit` reports 0 vulnerabilities and the same 10 allowed warnings
  triaged at 0.3.0.

- **Windows chord steps are recorded as the chord.** Control-Center's Windows
  controller was reporting the AutoHotkey transport form, so `press ^s` reached the
  record as `{Ctrl down}s{Ctrl up}`; 1.2.2 reports the command as issued, and the
  agent humanises it to `Ctrl+Shift+N` the same way every other platform does.
  Memory Archive needs no change for this — `humanize_key` handles the prefix form,
  the explicit down/up form, and the already-humanised string — but sessions
  recorded before and after the Control-Center upgrade will label Windows chords
  differently, and `actuation_agent_version` in `metadata.json` is what
  distinguishes them.

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
