# Changelog

All notable changes to Memory Archive are documented in this file. This project
adheres to [Semantic Versioning](https://semver.org/).

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
