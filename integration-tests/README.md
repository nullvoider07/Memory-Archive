# Control-Center compatibility tests

These drive **real binaries** — a released `control-center-server` and the built
`ma-core` — because the behaviour under test is the wire contract between two
independently versioned programs. A mock would only assert that the mock matches
our belief about Control-Center, which is precisely the belief that was wrong.

## What they cover

The regression they exist for: Control-Center 1.1.0 made TLS mandatory and added a
`monitor` scope on `WatchCommands`. Memory Archive connected in plaintext with no
credentials, so sessions against 1.1.0+ registered, reported `active`, and recorded
nothing. It surfaced only as an empty trace.

| Test | Asserts |
|---|---|
| `test_hardened_server_connects_over_tls` | 1.1.0 / 1.2.0 reached over TLS with a token |
| `test_missing_token_fails_loudly_and_does_not_strand_the_session` | non-zero exit, the message names `control_center_token`, session is not left `active` |
| `test_untrusted_ca_is_reported_and_never_downgraded` | a bad CA fails; no plaintext fallback |
| `test_legacy_server_downgrades_and_withholds_the_token` | 1.0.0 connects; the credential is **not** sent |
| `test_strict_refuses_to_reach_a_plaintext_server` | `strict` never downgrades |
| `test_legacy_policy_sends_the_token_over_plaintext` | `legacy` is the explicit cleartext opt-in |
| `test_http_address_still_reaches_a_tls_server` | an `http://` config still reaches an upgraded server |

## Safety

**Redis DB 15, never DB 0.** DB 0 holds the live session registry; a test that wrote
there could destroy recorded corpus sessions. The DB index is pinned in the harness
and `reset_registry()` refuses to flush anything else. Storage, config and the IPC
socket live under a per-test temporary directory, and every port is ephemeral
loopback.

## Running them

Stage the Control-Center releases under test, then run pytest:

```bash
# Stage the servers (any subset — missing versions skip rather than fail)
bash integration-tests/stage-cc-releases.sh

# Build ma-core and make sure the CLI is on PATH
cargo build -p ma-core

python3 -m pytest integration-tests -v
```

Override the staging directory with `MA_TEST_CC_BIN_DIR`. The layout is
`<dir>/<version>/control-center-server`.

Everything skips cleanly when Redis is unreachable, `ma-core` is unbuilt, or the
`memory-archive` CLI is not on `PATH`, so a plain `cargo test` workflow is unaffected.

## Strict mode — use this for the release gate

That leniency is wrong wherever the matrix is the gate, because every way it can
degrade still exits 0:

```bash
MA_INTEGRATION_STRICT=1 python3 -m pytest integration-tests -v
```

Strict mode turns an unmet prerequisite into a hard failure, and additionally
requires that every release `stage-cc-releases.sh` *discovered* actually staged.
The matrix parametrises over what is on disk, so a failed download otherwise
drops a row silently — and because `LATEST` is the newest staged version, a
failure to download the newest release retargets the provenance test at the
previous one and passes. Confirm the run's test count, not just its exit code.
