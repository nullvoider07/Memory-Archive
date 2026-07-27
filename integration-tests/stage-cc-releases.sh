#!/usr/bin/env bash
# Stage Control-Center server binaries for the compatibility tests.
#
# Downloads the linux-x64 archive for each release under test and lays them out
# as <dir>/<version>/control-center-server. Versions that fail to download are
# skipped rather than fatal — the tests skip the matching parameterisation.
set -uo pipefail

VERSIONS=("${@:-}")
if [[ -z "${VERSIONS[0]:-}" ]]; then
    VERSIONS=(1.0.0 1.1.0 1.2.0)
fi

DEST="${MA_TEST_CC_BIN_DIR:-/tmp/ma-test-cc}"
REPO="${MA_TEST_CC_REPO:-nullvoider07/control-center}"

mkdir -p "${DEST}"

for version in "${VERSIONS[@]}"; do
    target="${DEST}/${version}/control-center-server"
    if [[ -f "${target}" ]]; then
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

    mkdir -p "${DEST}/${version}"
    tar -xzf "${tmp}/${archive}" -C "${tmp}"
    cp "${tmp}/bin/control-center-server" "${target}"
    chmod +x "${target}"
    rm -rf "${tmp}"
    echo "[ok] staged ${version}"
done

echo
echo "Staged under ${DEST}:"
ls -1 "${DEST}" 2>/dev/null || true
