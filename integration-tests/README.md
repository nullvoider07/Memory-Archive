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
| `test_a_lossy_agent_is_refused_before_it_records_anything` | an agent below 1.2.2 is refused; the session is not left `active` |
| `test_an_agent_at_the_fidelity_floor_is_not_refused` | 1.2.2+ is accepted, asserted on a *positive* log line |
| `test_allow_unsupported_overrides_the_agent_gate` | the override records anyway, and still warns |

## Why some cases need a real *agent*, not just a server

The record-fidelity gate reads `agent_version`, and that field does not exist on
the wire until an agent has registered — until then the server stamps an empty
string on its heartbeats, which the gate correctly ignores. So every case that
predates this section ran server-only and could not reach the gate at all. That
is the gap the typed-command truncation shipped through.

`stage-cc-releases.sh` therefore also lays down `control-center-agent`, which
comes out of the archive it already downloads. Nothing actuates: the agent
registers and idles, and the server emits a heartbeat carrying its version every
five seconds, which is the whole input to the gate. That is what makes these
runnable headless — the agent reads `DISPLAY` only when it executes a command,
and a missing `xdotool` never stops it registering.

A version counts as staged only when **both** binaries are present. Deciding on
the server alone would leave a cache built before the agent was needed unable to
ever acquire one, since re-running the script would skip it — which would make
the three cases below quietly disappear from the run.

Three traps worth knowing, each of which produced a passing-for-the-wrong-reason
run while these were being written:

- **`silence_timeout_seconds` must exceed the server's heartbeat interval.** The
  harness pins 5s to keep the connect-only cases quick, and the server heartbeats
  every 5s — so a test that needs to *receive* one races it and usually loses,
  reporting a silence timeout instead. The fidelity cases raise it.
- **Assert on a positive signal, not on the absence of the refusal.** "No refusal
  was logged" is equally consistent with "no agent version ever reached the gate",
  which is exactly what the timeout collision caused. ma-core logs an accepted
  agent version for this reason.
- **`memory-archive start` blocks until the session ends**, which is correct — a
  healthy capture runs until it is stopped. Any case where the session is *not*
  refused therefore has to use `cli_detached`, or it hangs until the harness
  timeout and reports the feature working as a failure.

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
