#!/usr/bin/env bash
# ============================================================================
#  Memory Archive — Linux / macOS Installer
#  Repository : https://github.com/nullvoider07/memory-archive
#  Usage      : curl -fsSL https://raw.githubusercontent.com/nullvoider07/
#                 memory-archive/master/install/install.sh | bash
# ============================================================================
set -euo pipefail

# ============================================================================
# 1. CONFIGURATION
# ============================================================================

# Repository
readonly REPO_OWNER="nullvoider07"
readonly REPO_NAME="memory-archive"
readonly REPO_URL="https://github.com/${REPO_OWNER}/${REPO_NAME}"
readonly RELEASES_API="https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}/releases/latest"

# Package components
# Every release archive contains exactly these components:
#
#   ma-core            — Main daemon binary (Rust).  Manages sessions, IPC,
#                        TLS, storage backends, annotator lifecycle.
#   ma-kafka-producer  — Dev-tool binary (Rust).  Bridges gRPC stream → Kafka.
#   ma-app             — Python TUI + CLI package.  Bundled as a self-contained
#                        .whl and installed via pip so the `memory-archive`
#                        command is available on PATH.
#   ma-proto           — Protobuf library (Rust).  Compiled into ma-core and
#                        ma-kafka-producer at build time; no standalone binary
#                        is shipped, but it is listed here for completeness.
#
readonly RUST_BINARIES=("ma-core" "ma-kafka-producer")
readonly PYTHON_PACKAGE="ma-app"           # installed from the bundled .whl
readonly PROTO_LIBRARY="ma-proto"          # compile-time only; no binary

# Installation directories
# Everything lives under a single dedicated memory-archive directory so all
# components are in one predictable place regardless of Python environment.
if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    readonly MA_HOME="/opt/memory-archive"
else
    readonly MA_HOME="${HOME}/.memory-archive"
fi
readonly INSTALL_DIR="${MA_HOME}/bin"
readonly DATA_DIR="${MA_HOME}/share"

# Temp workspace — cleaned up in the EXIT trap (section 11).
readonly TMP_DIR="$(mktemp -d -t memory-archive-install.XXXXXX)"

# Terminal colours──
# Only emit ANSI codes when stdout is a real terminal.
if [[ -t 1 ]]; then
    CLR_RESET="\033[0m"
    CLR_BOLD="\033[1m"
    CLR_GREEN="\033[0;32m"
    CLR_CYAN="\033[0;36m"
    CLR_YELLOW="\033[0;33m"
    CLR_RED="\033[0;31m"
    CLR_DIM="\033[2m"
else
    CLR_RESET="" CLR_BOLD="" CLR_GREEN="" CLR_CYAN=""
    CLR_YELLOW="" CLR_RED="" CLR_DIM=""
fi

# ============================================================================
# 2. HELPER FUNCTIONS
# ============================================================================

# Print functions — all messages go to stderr so they don't pollute piped output.
# Print a box that auto-sizes to the widest line of content.
# Usage: _print_box COLOR "line1" "line2" ...
_print_box() {
    local color="$1"; shift
    local lines=("$@")
    local max_len=0
    local len
    for line in "${lines[@]}"; do
        len="${#line}"
        (( len > max_len )) && max_len=$len
    done
    local pad=$(( max_len + 2 ))
    local bar
    bar="$(printf '═%.0s' $(seq 1 $pad))"
    echo -e "${color}  ╔${bar}╗${CLR_RESET}" >&2
    for line in "${lines[@]}"; do
        local spaces=$(( pad - ${#line} - 1 ))
        echo -e "${color}  ║ ${line}$(printf ' %.0s' $(seq 1 $spaces))║${CLR_RESET}" >&2
    done
    echo -e "${color}  ╚${bar}╝${CLR_RESET}" >&2
}

print_banner() {
    echo -e "${CLR_BOLD}" >&2
    _print_box "${CLR_BOLD}${CLR_CYAN}" \
        "Memory Archive — Installer" \
        "https://github.com/${REPO_OWNER}/${REPO_NAME}"
    echo -e "${CLR_RESET}" >&2
}

print_step() {
    echo -e "\n${CLR_BOLD}${CLR_CYAN}  ▶  $*${CLR_RESET}" >&2
}

print_info() {
    echo -e "  ${CLR_DIM}$*${CLR_RESET}" >&2
}

print_ok() {
    echo -e "  ${CLR_GREEN}✔  $*${CLR_RESET}" >&2
}

print_warn() {
    echo -e "  ${CLR_YELLOW}⚠  $*${CLR_RESET}" >&2
}

print_error() {
    echo -e "  ${CLR_RED}✖  ERROR: $*${CLR_RESET}" >&2
}

die() {
    print_error "$*"
    exit 1
}

# Returns 0 if the named command exists on PATH, 1 otherwise.
command_exists() {
    command -v "$1" &>/dev/null
}

# Returns 0 if the given version string ($1) is ≥ the required version ($2).
# Both arguments must be dot-separated integers (e.g. "3.13.2").
version_ge() {
    # Sort both versions and check whether $2 is not greater than $1.
    printf '%s\n%s\n' "$2" "$1" | sort -V -C
}

# ============================================================================
# 3. CHECK EXISTING INSTALLATION
# ============================================================================

check_existing_installation() {
    print_step "Checking for existing installation"

    local found_any=false

    for bin in "${RUST_BINARIES[@]}"; do
        if command_exists "${bin}"; then
            local existing_path
            existing_path="$(command -v "${bin}")"
            print_warn "Found existing ${bin} at ${existing_path}"
            found_any=true
        fi
    done

    if command_exists "memory-archive"; then
        local ma_ver
        ma_ver="$(memory-archive --version 2>/dev/null | head -1 || echo "unknown")"
        print_warn "Found existing memory-archive (${ma_ver}) at $(command -v memory-archive)"
        found_any=true
    fi

    if [[ "${found_any}" == true ]]; then
        print_info "Existing components will be overwritten by this installation."
    else
        print_ok "No existing installation found."
    fi
}

# ============================================================================
# 4. DETECT OS AND ARCHITECTURE
# ============================================================================

detect_platform() {
    print_step "Detecting platform"

    local os arch

    # Operating system
    case "$(uname -s)" in
        Linux*)   os="linux"  ;;
        Darwin*)  os="macos"  ;;
        *)        die "Unsupported operating system: $(uname -s).  Only Linux and macOS are supported by this script.  For Windows use install.ps1." ;;
    esac

    # CPU architecture
    case "$(uname -m)" in
        x86_64 | amd64)       arch="x64"   ;;
        aarch64 | arm64)      arch="arm64" ;;
        *)                    die "Unsupported architecture: $(uname -m).  Only x86_64 and arm64 are supported." ;;
    esac

    # Export as read-only globals for later sections.
    readonly PLATFORM_OS="${os}"
    readonly PLATFORM_ARCH="${arch}"
    readonly ARTIFACT_STEM="ma-${PLATFORM_OS}-${PLATFORM_ARCH}"   # e.g. ma-linux-x64
    readonly ARTIFACT_NAME="${ARTIFACT_STEM}.tar.gz"

    print_ok "Platform: ${PLATFORM_OS} / ${PLATFORM_ARCH}"
    print_info "Target artifact: ${ARTIFACT_NAME}"
}

# ============================================================================
# 5. CHECK DEPENDENCIES
# ============================================================================

check_dependencies() {
    print_step "Checking dependencies"

    local missing=()

    # curl
    if command_exists curl; then
        print_ok "curl $(curl --version | head -1 | awk '{print $2}')"
    else
        missing+=("curl")
    fi

    # tar
    if command_exists tar; then
        print_ok "tar"
    else
        missing+=("tar")
    fi

    # Python 3.13+ (required for ma-app wheel install)
    # Probe python3 first, then python as a fallback.
    PYTHON_BIN=""
    for candidate in python3.13 python3.14 python3 python; do
        if command_exists "${candidate}"; then
            local ver
            ver="$("${candidate}" -c 'import sys; print(".".join(map(str, sys.version_info[:3])))'  2>/dev/null || echo "0.0.0")"
            if version_ge "${ver}" "3.13"; then
                PYTHON_BIN="${candidate}"
                print_ok "Python ${ver} (${candidate})"
                break
            fi
        fi
    done
    readonly PYTHON_BIN

    if [[ -z "${PYTHON_BIN}" ]]; then
        print_warn "Python 3.13+ not found.  ma-app (the 'memory-archive' CLI) will NOT be installed."
        print_warn "Install Python 3.13+ and re-run this script to add the CLI."
    fi

    # pip — always derived from the selected Python binary so they are
    # guaranteed to belong to the same environment.
    PIP_BIN=""
    if [[ -n "${PYTHON_BIN}" ]]; then
        if "${PYTHON_BIN}" -m pip --version &>/dev/null; then
            PIP_BIN="${PYTHON_BIN} -m pip"
            print_ok "pip (via ${PYTHON_BIN} -m pip)"
        else
            print_warn "pip not found.  ma-app (the 'memory-archive' CLI) will NOT be installed."
        fi
    fi
    readonly PIP_BIN

    # macOS: codesign verification (informational only)
    if [[ "${PLATFORM_OS}" == "macos" ]]; then
        if command_exists codesign; then
            print_ok "codesign (ad-hoc signatures will be verified)"
        fi
    fi

    # Fail hard only if curl or tar are missing
    if [[ ${#missing[@]} -gt 0 ]]; then
        die "Required tools not found: ${missing[*]}.  Install them and re-run."
    fi
}

# ============================================================================
# 6. GET LATEST RELEASE
# ============================================================================

get_latest_release() {
    print_step "Fetching latest release information"

    # Query the GitHub Releases API.
    local api_response
    api_response="$(
        curl -fsSL \
             -H "Accept: application/vnd.github+json" \
             -H "X-GitHub-Api-Version: 2022-11-28" \
             "${RELEASES_API}"
    )" || die "Failed to reach GitHub API.  Check your internet connection."

    # Parse the tag name using grep + sed — no jq dependency required.
    RELEASE_TAG="$(echo "${api_response}" | grep '"tag_name"' | head -1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    [[ -n "${RELEASE_TAG}" ]] || die "Could not parse release tag from GitHub API response."

    # Build the direct download URL for this platform's archive.
    DOWNLOAD_URL="${REPO_URL}/releases/download/${RELEASE_TAG}/${ARTIFACT_NAME}"

    readonly RELEASE_TAG DOWNLOAD_URL

    print_ok "Latest release: ${RELEASE_TAG}"
    print_info "Download URL : ${DOWNLOAD_URL}"
}

# ============================================================================
# 7. DOWNLOAD THE PACKAGE
# ============================================================================

download_package() {
    print_step "Downloading ${ARTIFACT_NAME}"

    # Emit a blank line so the curl progress meter starts on its own line.
    echo "" >&2

    # Download with curl's default transfer stats (the clean tabular display
    # the user sees when piping to bash).  We deliberately do NOT pass -s
    # (silent) here — the progress meter prints to stderr, which is always
    # visible even in a pipe.
    #
    #   -f  : fail on HTTP 4xx/5xx (avoids saving an HTML error page)
    #   -L  : follow redirects (GitHub releases redirect to cdn)
    #   -o  : write to a named file instead of stdout
    #
    curl -fL \
         -o "${TMP_DIR}/${ARTIFACT_NAME}" \
         "${DOWNLOAD_URL}" \
    || die "Download failed.  URL: ${DOWNLOAD_URL}"

    echo "" >&2
    print_ok "Download complete: $(du -sh "${TMP_DIR}/${ARTIFACT_NAME}" | cut -f1)"
}

# ============================================================================
# 8. EXTRACT THE PACKAGE
# ============================================================================

extract_package() {
    print_step "Extracting ${ARTIFACT_NAME}"

    readonly EXTRACT_DIR="${TMP_DIR}/extracted"
    mkdir -p "${EXTRACT_DIR}"

    # -x  extract
    # -z  decompress gzip
    # -f  read from file
    # -C  change to target directory before extracting
    tar -xzf "${TMP_DIR}/${ARTIFACT_NAME}" -C "${EXTRACT_DIR}" \
    || die "Failed to extract ${ARTIFACT_NAME}.  The archive may be corrupt."

    print_ok "Extracted to ${EXTRACT_DIR}"
    print_info "Archive contents:"
    ls -lh "${EXTRACT_DIR}" | tail -n +2 | while IFS= read -r line; do
        print_info "  ${line}"
    done
}

# ============================================================================
# 9. INSTALL THE COMPONENTS
# ============================================================================

install_components() {
    print_step "Installing components"

    # Create install directories
    mkdir -p "${INSTALL_DIR}"
    mkdir -p "${DATA_DIR}"

    # Rust binaries
    for bin in "${RUST_BINARIES[@]}"; do
        local src="${EXTRACT_DIR}/${bin}"
        if [[ ! -f "${src}" ]]; then
            die "Binary not found in archive: ${bin}.  The release may be malformed."
        fi
        cp "${src}" "${INSTALL_DIR}/${bin}"
        chmod 755 "${INSTALL_DIR}/${bin}"

        # macOS: clear the quarantine extended attribute that Gatekeeper adds
        # when files are downloaded from the internet. The binaries are
        # ad-hoc signed by the CI pipeline so they will run after xattr removal.
        if [[ "${PLATFORM_OS}" == "macos" ]]; then
            xattr -d com.apple.quarantine "${INSTALL_DIR}/${bin}" 2>/dev/null || true
        fi

        print_ok "Installed ${bin} → ${INSTALL_DIR}/${bin}"
    done

    # Python wheel (ma-app)
    # Install into a dedicated prefix inside MA_HOME so the entry point script
    # lands in INSTALL_DIR alongside the Rust binaries — one directory on PATH.
    local whl
    whl="$(ls "${EXTRACT_DIR}"/*.whl 2>/dev/null | head -1 || true)"

    if [[ -n "${whl}" && -n "${PIP_BIN}" ]]; then
        print_info "Installing Python package from ${whl##*/} …"

        # Install normally so pip handles the shebang and sys.path correctly.
        # --user ensures it goes into the user's site-packages, not a prefix
        # that breaks imports at runtime.
        local user_flag=""
        [[ "${EUID:-$(id -u)}" -ne 0 ]] && user_flag="--user"
        ${PIP_BIN} install ${user_flag} "${whl}" \
        || die "pip install failed.  Run manually: ${PIP_BIN} install ${user_flag} '${whl}'"

        # Ask pip where it put the entry point, then copy it into INSTALL_DIR.
        # This works regardless of which Python or site-packages layout is used.
        local ep_src
        ep_src="$(${PYTHON_BIN} -c "import sysconfig; print(sysconfig.get_path('scripts', 'posix_user' if '${user_flag}' else 'posix_prefix'))")/memory-archive"

        if [[ -f "${ep_src}" ]]; then
            cp "${ep_src}" "${INSTALL_DIR}/memory-archive"
            chmod 755 "${INSTALL_DIR}/memory-archive"
            print_ok "Installed memory-archive → ${INSTALL_DIR}/memory-archive"
        else
            print_warn "memory-archive entry point not found at ${ep_src} — CLI will not be available."
        fi
    elif [[ -z "${whl}" ]]; then
        print_warn "No .whl found in archive — skipping ${PYTHON_PACKAGE} install."
    else
        print_warn "pip unavailable — skipping ${PYTHON_PACKAGE} install."
    fi

    # Support files
    # Copy LICENSE and README into the data directory for reference.
    for doc in LICENSE README.md INSTALL.md; do
        if [[ -f "${EXTRACT_DIR}/${doc}" ]]; then
            cp "${EXTRACT_DIR}/${doc}" "${DATA_DIR}/${doc}"
        fi
    done
    print_ok "Documentation installed → ${DATA_DIR}/"
}

# ============================================================================
# 10. UPDATE PATH
# ============================================================================

update_path() {
    print_step "Checking PATH"

    # System-wide /usr/local/bin is already on PATH in virtually all distros.
    # The user-local ~/.local/bin may not be — check and advise accordingly.
    if [[ ":${PATH}:" == *":${INSTALL_DIR}:"* ]]; then
        print_ok "${INSTALL_DIR} is already on your PATH."
        return
    fi

    # Only reached for the user-local case.
    print_warn "${INSTALL_DIR} is not on your PATH."

    # Detect the user's login shell and append to the correct rc file.
    local shell_rc=""
    case "${SHELL:-}" in
        */zsh)  shell_rc="${ZDOTDIR:-${HOME}}/.zshrc" ;;
        */fish) shell_rc="${HOME}/.config/fish/config.fish" ;;
        *)      shell_rc="${HOME}/.bashrc" ;;
    esac

    local path_export="export PATH=\"${INSTALL_DIR}:\${PATH}\""
    local fish_export="fish_add_path ${INSTALL_DIR}"

    if [[ "${SHELL:-}" == */fish ]]; then
        if ! grep -qF "${INSTALL_DIR}" "${shell_rc}" 2>/dev/null; then
            echo "${fish_export}" >> "${shell_rc}"
            print_ok "Added fish_add_path to ${shell_rc}"
        fi
    else
        if ! grep -qF "${INSTALL_DIR}" "${shell_rc}" 2>/dev/null; then
            {
                echo ""
                echo "# Added by Memory Archive installer"
                echo "${path_export}"
            } >> "${shell_rc}"
            print_ok "Added PATH export to ${shell_rc}"
        fi
    fi

    # Export for the current shell session so the user can run immediately.
    export PATH="${INSTALL_DIR}:${PATH}"
    print_info "Run 'source ${shell_rc}' or open a new terminal for the PATH change to persist."
}

# ============================================================================
# 11. CLEANUP
# ============================================================================

cleanup() {
    # Registered as an EXIT trap so it runs even on error.
    if [[ -d "${TMP_DIR:-}" ]]; then
        rm -rf "${TMP_DIR}"
    fi
}
trap cleanup EXIT

# ============================================================================
# 12. PRINT SUCCESS MESSAGE
# ============================================================================

print_success() {
    echo "" >&2
    _print_box "${CLR_BOLD}${CLR_GREEN}" "Memory Archive installed successfully!"
    echo "" >&2
    echo -e "${CLR_BOLD}  Version  :${CLR_RESET} ${RELEASE_TAG}" >&2
    echo -e "${CLR_BOLD}  Binaries :${CLR_RESET} ${INSTALL_DIR}/ma-core" >&2
    echo -e "            ${INSTALL_DIR}/ma-kafka-producer" >&2
    if command_exists memory-archive; then
        echo -e "${CLR_BOLD}  CLI      :${CLR_RESET} $(command -v memory-archive)" >&2
    fi
    echo -e "${CLR_BOLD}  Docs     :${CLR_RESET} ${DATA_DIR}/" >&2
    echo "" >&2
    echo -e "${CLR_CYAN}  Quick start:${CLR_RESET}" >&2
    echo -e "    ${CLR_DIM}# Start the core daemon${CLR_RESET}" >&2
    echo -e "    ma-core" >&2
    echo "" >&2
    echo -e "    ${CLR_DIM}# Annotate a session${CLR_RESET}" >&2
    echo -e "    memory-archive annotate <session-id>" >&2
    echo "" >&2
    echo -e "    ${CLR_DIM}# Documentation${CLR_RESET}" >&2
    echo -e "    ${REPO_URL}#readme" >&2
    echo "" >&2
}

# ============================================================================
# 13. MAIN INSTALLATION FLOW
# ============================================================================

main() {
    print_banner

    check_existing_installation  # section 3
    detect_platform              # section 4
    check_dependencies           # section 5
    get_latest_release           # section 6
    download_package             # section 7
    extract_package              # section 8
    install_components           # section 9
    update_path                  # section 10
    # cleanup runs via EXIT trap  (section 11)
    print_success                # section 12
}

main "$@"