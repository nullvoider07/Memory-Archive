#!/usr/bin/env bash
# Stage Control-Center server binaries for the compatibility tests.
#
# Downloads the linux-x64 archive for each release under test and lays them out
# as <dir>/<version>/control-center-server. Versions that fail to download are
# skipped rather than fatal — the tests skip the matching parameterisation.
set -uo pipefail

DEST="${MA_TEST_CC_BIN_DIR:-/tmp/ma-test-cc}"
REPO="${MA_TEST_CC_REPO:-nullvoider07/control-center}"

# Versions come from the release list, not a hard-coded array: a matrix that has
# to be edited for every Control-Center release is a matrix that silently stops
# covering the newest one. Explicit arguments still override, for bisecting.
VERSIONS=("$@")
if [[ ${#VERSIONS[@]} -eq 0 ]]; then
    mapfile -t VERSIONS < <(
        gh release list --repo "${REPO}" --limit 100 \
            --json tagName,isDraft,isPrerelease \
            --jq '.[] | select(.isDraft == false and .isPrerelease == false) | .tagName' \
        | sed 's/^v//' \
        | grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' \
        | sort -t. -k1,1n -k2,2n -k3,3n
    )
    if [[ ${#VERSIONS[@]} -eq 0 ]]; then
        echo "[FAIL] could not enumerate releases from ${REPO}" >&2
        exit 1
    fi
    echo "Discovered ${#VERSIONS[@]} release(s): ${VERSIONS[*]}"
fi

mkdir -p "${DEST}"

# Record what this run intended to stage, before any download is attempted.
# Download failure is deliberately non-fatal here so a developer can stage a
# subset, but the test matrix derives its parametrisation from what is on disk:
# without this manifest a failed download silently drops a row (or, when the
# newest release is the one that fails, silently retargets the provenance test
# at the previous version) and the suite still reports green. MA_INTEGRATION_STRICT
# compares the two and fails the run instead.
printf '%s\n' "${VERSIONS[@]}" > "${DEST}/DISCOVERED"

for version in "${VERSIONS[@]}"; do
    target="${DEST}/${version}/control-center-server"
    agent_target="${DEST}/${version}/control-center-agent"
    # Both binaries, not just the server. A cache staged before the agent was
    # needed holds a server and no agent, and skipping on the server alone would
    # make re-running this script a no-op that can never repair it — the agent
    # tests would keep skipping (or, under MA_INTEGRATION_STRICT, keep failing
    # with an instruction that does nothing). NO_AGENT marks the versions whose
    # archive genuinely carries no agent, so those are not re-downloaded forever.
    if [[ -f "${target}" ]] && { [[ -f "${agent_target}" ]] || [[ -f "${DEST}/${version}/NO_AGENT" ]]; }; then
        echo "[skip] ${version} already staged"
        continue
    fi

    tmp="$(mktemp -d)"
    archive="control-center-${version}-linux-x64.tar.gz"

    if ! gh release download "v${version}" --repo "${REPO}" \
            --pattern "${archive}" --dir "${tmp}" 2>/dev/null; then
        echo "[warn] could not download ${archive} — ${version} will be skipped"
        rm -rf "${tmp}"
        continue
    fi

    # Verify before extracting. Control-Center publishes a SHA256SUMS covering
    # every archive from 1.2.1 onward; earlier releases predate it. Where the
    # file exists a mismatch is fatal, so a tampered archive never reaches tar.
    if gh release download "v${version}" --repo "${REPO}" \
            --pattern "SHA256SUMS" --dir "${tmp}" 2>/dev/null; then
        if ! (cd "${tmp}" && grep -F " ${archive}" SHA256SUMS | sha256sum -c --status -); then
            echo "[FAIL] checksum mismatch for ${archive} — refusing to extract"
            rm -rf "${tmp}"
            exit 1
        fi
        echo "[ok] checksum verified for ${version}"
    else
        echo "[warn] ${version} publishes no SHA256SUMS (pre-1.2.1) — extracting unverified"
    fi

    mkdir -p "${DEST}/${version}"
    tar -xzf "${tmp}/${archive}" -C "${tmp}"
    cp "${tmp}/bin/control-center-server" "${target}"
    chmod +x "${target}"

    # The agent comes out of the same archive, so staging it costs no extra
    # download. It is required to exercise the record-fidelity gate: the server
    # stamps an empty agent_version on its heartbeats until an agent registers,
    # so a server-only run cannot reach that code path at all. Absence is a warn
    # rather than a failure — the tests that need it skip, and the archive did
    # not always carry one.
    if [[ -f "${tmp}/bin/control-center-agent" ]]; then
        cp "${tmp}/bin/control-center-agent" "${agent_target}"
        chmod +x "${agent_target}"
    else
        touch "${DEST}/${version}/NO_AGENT"
        echo "[warn] ${version} archive carries no control-center-agent — agent-gate tests will skip"
    fi

    rm -rf "${tmp}"
    echo "[ok] staged ${version}"
done

echo
echo "Staged under ${DEST}:"
ls -1 "${DEST}" 2>/dev/null || true
