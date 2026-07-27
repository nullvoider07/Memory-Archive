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

from conftest import require_cc
from harness import (
    cc_server,
    cli,
    ma_core,
    mint_token,
    register_session,
    session_status,
    write_tls_material,
)

pytestmark = pytest.mark.integration


def _log(ma) -> str:
    """ma-core's log with ANSI colour stripped, so matching stays readable."""
    return re.sub(r"\x1b\[[0-9;]*m", "", ma.log.read_text(errors="replace"))


def _start(ma, session_id: str):
    return cli(ma.config_path, "start", "--session", session_id, timeout=180)


# ---------------------------------------------------------------------------
# 1.1.0+ — TLS and a monitor-scoped token
# ---------------------------------------------------------------------------

@pytest.mark.parametrize("version", ["1.1.0", "1.2.0"])
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


@pytest.mark.parametrize("version", ["1.1.0", "1.2.0"])
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
    binary = require_cc("1.2.0")
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
    binary = require_cc("1.2.0")
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
