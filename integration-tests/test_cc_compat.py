"""Control-Center compatibility matrix, exercised against real release binaries.

The regression these exist for: Control-Center 1.1.0 made TLS mandatory and put a
`monitor` scope on WatchCommands. Memory Archive connected in plaintext with no
credentials, so every session against 1.1.0+ registered, reported `active`, and
recorded nothing — the failure surfaced only as an empty trace, which cost a
recorded corpus session before anyone noticed.

Each test therefore asserts on observable outcomes an operator would notice: the
transport actually negotiated, whether the run failed loudly, and the status the
session was left in.
"""
from __future__ import annotations

import re
from pathlib import Path

import pytest

from conftest import require_cc, require_cc_agent, staged_versions
from harness import (
    cc_agent,
    cc_server,
    cli,
    cli_detached,
    ma_core,
    mint_token,
    register_session,
    session_status,
    wait_for_file_line,
    write_tls_material,
)

pytestmark = pytest.mark.integration

# Every staged release that mandates TLS and a monitor scope (1.1.0+).
# Derived at collection time so a new Control-Center release joins the
# matrix by being staged, not by editing this file.
HARDENED = staged_versions(minimum="1.1.0")
# The newest staged release — what an operator would actually be running.
LATEST = HARDENED[-1]


def _as_version(version: str) -> tuple[int, ...]:
    """`"1.2.10"` -> `(1, 2, 10)`. String order would put 1.10 before 1.9."""
    try:
        return tuple(int(part) for part in version.split("."))
    except ValueError:
        # staged_versions() yields a "0.0.0" placeholder when nothing is staged.
        return (0, 0, 0)


def _log(ma) -> str:
    """ma-core's log with ANSI colour stripped, so matching stays readable."""
    return re.sub(r"\x1b\[[0-9;]*m", "", ma.log.read_text(errors="replace"))


def _start(ma, session_id: str):
    return cli(ma.config_path, "start", "--session", session_id, timeout=180)


# ---------------------------------------------------------------------------
# 1.1.0+ — TLS and a monitor-scoped token
# ---------------------------------------------------------------------------

@pytest.mark.parametrize("version", HARDENED)
def test_hardened_server_connects_over_tls(version, workdir: Path, clean_registry):
    """A configured token plus a trusted CA reaches a TLS-only server."""
    binary = require_cc(version)
    tls = write_tls_material(workdir / "tls")
    ca, _cert, _key = tls

    with cc_server(binary, workdir, tls) as cc:
        with ma_core(
            workdir, cc.port,
            token=mint_token("monitor"), tls_ca=str(ca), security="auto",
        ) as ma:
            session = register_session(ma.config_path, f"compat-tls-{version}")
            _start(ma, session)

            log = _log(ma)
            assert 'transport="tls"' in log, log[-1500:]
            assert "downgraded to an unencrypted connection" not in log
            # The credential is expected on an encrypted channel.
            assert "Withholding the Control-Center token" not in log


@pytest.mark.parametrize("version", HARDENED)
def test_missing_token_fails_loudly_and_does_not_strand_the_session(
    version, workdir: Path, clean_registry
):
    """The original regression: no token must not look like a healthy session."""
    binary = require_cc(version)
    tls = write_tls_material(workdir / "tls")
    ca, _cert, _key = tls

    with cc_server(binary, workdir, tls) as cc:
        with ma_core(workdir, cc.port, token="", tls_ca=str(ca), security="auto") as ma:
            session = register_session(ma.config_path, f"compat-notoken-{version}")
            result = _start(ma, session)

            assert result.returncode != 0, "start must fail when capture cannot subscribe"

            combined = result.stdout + result.stderr
            assert "control_center_token" in combined, combined
            assert "monitor" in combined, combined

            # Left `active`, an operator would believe capture was running.
            assert session_status(ma.config_path, session) != "active"


def test_untrusted_ca_is_reported_and_never_downgraded(workdir: Path, clean_registry):
    """A certificate we cannot verify must not become a plaintext connection."""
    binary = require_cc(LATEST)
    server_tls = write_tls_material(workdir / "tls-server")
    # A second, unrelated CA — valid PEM, wrong issuer.
    wrong_ca, _c, _k = write_tls_material(workdir / "tls-other")

    with cc_server(binary, workdir, server_tls) as cc:
        with ma_core(
            workdir, cc.port,
            token=mint_token("monitor"), tls_ca=str(wrong_ca), security="strict",
        ) as ma:
            session = register_session(ma.config_path, "compat-badca")
            result = _start(ma, session)

            assert result.returncode != 0
            assert "downgraded to an unencrypted connection" not in _log(ma)


# ---------------------------------------------------------------------------
# 1.0.0 — plaintext, no scope enforcement
# ---------------------------------------------------------------------------

def test_legacy_server_downgrades_and_withholds_the_token(workdir: Path, clean_registry):
    """1.0.0 has no TLS. The fallback must work, and must not carry the credential.

    Withholding is the security property: an attacker who can disrupt the TLS
    handshake would otherwise force this path and collect a monitor-scoped token
    in the clear. 1.0.0 never reads the header, so nothing is lost.
    """
    binary = require_cc("1.0.0")

    with cc_server(binary, workdir, None) as cc:
        with ma_core(
            workdir, cc.port, token=mint_token("monitor"), security="auto",
        ) as ma:
            session = register_session(ma.config_path, "compat-legacy")
            _start(ma, session)

            log = _log(ma)
            assert "downgraded to an unencrypted connection" in log, log[-1500:]
            assert "Withholding the Control-Center token" in log, log[-1500:]
            assert 'transport="plaintext"' in log, log[-1500:]


def test_strict_refuses_to_reach_a_plaintext_server(workdir: Path, clean_registry):
    """`strict` is the setting that makes a downgrade impossible. Prove it."""
    binary = require_cc("1.0.0")

    with cc_server(binary, workdir, None) as cc:
        with ma_core(
            workdir, cc.port, token=mint_token("monitor"), security="strict",
        ) as ma:
            session = register_session(ma.config_path, "compat-strict")
            result = _start(ma, session)

            assert result.returncode != 0
            log = _log(ma)
            assert "downgraded to an unencrypted connection" not in log
            assert 'transport="plaintext"' not in log


def test_legacy_policy_sends_the_token_over_plaintext(workdir: Path, clean_registry):
    """The explicit opt-in: `legacy` means the operator accepts a cleartext token."""
    binary = require_cc("1.0.0")

    with cc_server(binary, workdir, None) as cc:
        with ma_core(
            workdir, cc.port, token=mint_token("monitor"), security="legacy",
        ) as ma:
            session = register_session(ma.config_path, "compat-legacy-policy")
            _start(ma, session)

            log = _log(ma)
            assert 'transport="plaintext"' in log, log[-1500:]
            assert "Withholding the Control-Center token" not in log


# ---------------------------------------------------------------------------
# Address handling
# ---------------------------------------------------------------------------

def test_http_address_still_reaches_a_tls_server(workdir: Path, clean_registry):
    """An existing `http://` config must not need editing after a CC upgrade.

    This is why the URL scheme does not pin the transport under `auto`: every
    deployment predating Control-Center 1.1.0 has `http://` written down.
    """
    binary = require_cc(LATEST)
    tls = write_tls_material(workdir / "tls")
    ca, _cert, _key = tls

    with cc_server(binary, workdir, tls) as cc:
        with ma_core(
            workdir, cc.port,
            token=mint_token("monitor"), tls_ca=str(ca), security="auto",
            scheme="http",  # deliberately the "wrong" scheme
        ) as ma:
            session = register_session(ma.config_path, "compat-scheme")
            _start(ma, session)

            assert 'transport="tls"' in _log(ma)


# ---------------------------------------------------------------------------
# Version gate
# ---------------------------------------------------------------------------

def test_supported_version_is_recorded_as_provenance(workdir: Path, clean_registry):
    """The server version must reach the session, not just the log.

    `position_captured` changed meaning between Control-Center releases without
    changing on the wire, so a recorded session is only interpretable if it says
    which server produced it.
    """
    binary = require_cc(LATEST)
    tls = write_tls_material(workdir / "tls")
    ca, _cert, _key = tls

    with cc_server(binary, workdir, tls) as cc:
        with ma_core(
            workdir, cc.port,
            token=mint_token("monitor"), tls_ca=str(ca), security="auto",
        ) as ma:
            session = register_session(ma.config_path, "compat-provenance")
            _start(ma, session)

            assert f"server_version={LATEST}" in _log(ma), _log(ma)[-1500:]


# ---------------------------------------------------------------------------
# Record fidelity — the agent gate
#
# These need a real agent, not just a server. `agent_version` only exists on the
# wire once one has registered: with none attached the server stamps an empty
# string on its heartbeats, so every test above reaches the connect gate and
# never the fidelity gate. That gap is why the truncation was not caught here.
#
# The agent registers and idles. Nothing actuates, and nothing needs to — the
# server emits a heartbeat carrying the agent version every five seconds, and
# that is the whole input to the gate.
# ---------------------------------------------------------------------------

# Releases whose agent stores a typed command truncated at the first quote.
# Fixed in 1.2.2; see ma-core/src/capture/compat.rs RECORD_FIDELITY_MIN.
#
# Both lists fall back to a placeholder rather than being allowed to go empty.
# An empty `parametrize` collects zero tests and still reports green, which is
# the same silent-row-drop the strict-mode note warns about — and here it would
# hide the loss of the only coverage either side of the fidelity floor. The
# placeholder collects one row that skips through `require_cc` with an
# actionable message, or fails outright under MA_INTEGRATION_STRICT.
LOSSY_AGENTS = [v for v in staged_versions() if _as_version(v) < (1, 2, 2)] or ["1.0.0"]
FAITHFUL_AGENTS = [v for v in staged_versions() if _as_version(v) >= (1, 2, 2)] or ["1.2.2"]


def _fidelity_env(version: str, workdir: Path):
    """Transport settings for `version`, so each test is about the gate only.

    1.0.0 is plaintext with no scopes; 1.1.0+ mandates TLS and wants a token on
    both halves. Returns `(hardened, tls_material, ca_path_str)`.
    """
    hardened = _as_version(version) >= (1, 1, 0)
    tls = write_tls_material(workdir / "tls") if hardened else None
    ca = str(tls[0]) if tls else ""
    return hardened, tls, ca


@pytest.mark.parametrize("version", LOSSY_AGENTS)
def test_a_lossy_agent_is_refused_before_it_records_anything(
    version, workdir: Path, clean_registry
):
    """An agent that misreports what it typed must not produce a trace at all.

    The failure this prevents is not a crash — it is a session that records
    perfectly except that one command is stored short, which is discoverable
    only by reading frames weeks later. An empty session is recoverable; a
    plausible wrong one is not.
    """
    server = require_cc(version)
    agent = require_cc_agent(version)
    hardened, tls, ca = _fidelity_env(version, workdir)

    with cc_server(server, workdir, tls) as cc:
        with cc_agent(
            agent, workdir, cc.port,
            token=mint_token("agent") if hardened else None,
            tls_ca=Path(ca) if ca else None,
        ):
            with ma_core(
                workdir, cc.port,
                token=mint_token("monitor") if hardened else "",
                security="auto" if hardened else "legacy",
                tls_ca=ca,
                silence_timeout=20,
            ) as ma:
                session = register_session(ma.config_path, "fidelity-lossy")
                _start(ma, session)

                log = _log(ma)
                # The refusal has to name the version found, the version that
                # fixes it, and the override — an operator reading only this
                # line must be able to act on it.
                assert "does not record typed commands faithfully" in log, log[-2500:]
                assert version in log, log[-2500:]
                assert "1.2.2" in log, log[-2500:]
                assert "control_center_allow_unsupported" in log, log[-2500:]

                # And it must not be left looking like a healthy capture.
                assert session_status(ma.config_path, session) != "active"


@pytest.mark.parametrize("version", FAITHFUL_AGENTS)
def test_an_agent_at_the_fidelity_floor_is_not_refused(
    version, workdir: Path, clean_registry
):
    """The other half of the gate: a fixed agent must still record.

    Without this, a gate that refused everything would pass the test above and
    silently end the corpus.
    """
    server = require_cc(version)
    agent = require_cc_agent(version)
    hardened, tls, ca = _fidelity_env(version, workdir)

    with cc_server(server, workdir, tls) as cc:
        with cc_agent(
            agent, workdir, cc.port,
            token=mint_token("agent") if hardened else None,
            tls_ca=Path(ca) if ca else None,
        ):
            with ma_core(
                workdir, cc.port,
                token=mint_token("monitor") if hardened else "",
                security="auto" if hardened else "legacy",
                tls_ca=ca,
                silence_timeout=20,
            ) as ma:
                session = register_session(ma.config_path, "fidelity-ok")
                # Detached: a session that is *not* refused keeps running, so a
                # blocking start would hang until the harness timeout and report
                # the feature working as a failure.
                with cli_detached(ma.config_path, "start", "--session", session):
                    accepted = wait_for_file_line(
                        ma.log, "agent records typed commands faithfully", timeout=45.0
                    )

                log = _log(ma)
                # Positive assertion on purpose. Asserting only that the refusal
                # is absent would also pass when no agent version ever reached
                # the gate — which is exactly what happened while writing this,
                # because the silence timeout matched the heartbeat interval.
                assert accepted, log[-2500:]
                assert version in log, log[-2500:]
                assert "does not record typed commands faithfully" not in log, log[-2500:]


@pytest.mark.parametrize("version", LOSSY_AGENTS)
def test_allow_unsupported_overrides_the_agent_gate(
    version, workdir: Path, clean_registry
):
    """The documented escape hatch has to actually work, and has to warn.

    A refusal with no override would strand anyone who genuinely needs to record
    against an old agent; an override that goes quiet would recreate the original
    problem with an extra step.
    """
    server = require_cc(version)
    agent = require_cc_agent(version)
    hardened, tls, ca = _fidelity_env(version, workdir)

    with cc_server(server, workdir, tls) as cc:
        with cc_agent(
            agent, workdir, cc.port,
            token=mint_token("agent") if hardened else None,
            tls_ca=Path(ca) if ca else None,
        ):
            with ma_core(
                workdir, cc.port,
                token=mint_token("monitor") if hardened else "",
                security="auto" if hardened else "legacy",
                tls_ca=ca,
                allow_unsupported=True,
                silence_timeout=20,
            ) as ma:
                session = register_session(ma.config_path, "fidelity-override")
                # Detached for the same reason as the faithful case: the whole
                # point of the override is that recording continues.
                with cli_detached(ma.config_path, "start", "--session", session):
                    warned = wait_for_file_line(
                        ma.log, "control_center_allow_unsupported is set", timeout=45.0
                    )

                log = _log(ma)
                assert warned, log[-2500:]
                # Still says what it is recording against, so the trace is not
                # silently trusted just because the operator opted in.
                assert "does not record typed commands faithfully" in log, log[-2500:]
