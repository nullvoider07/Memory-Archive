# =============================================================================
#  Memory Archive — Windows Installer (PowerShell)
#  Repository : https://github.com/nullvoider07/memory-archive
#  Usage      : irm https://raw.githubusercontent.com/nullvoider07/
#                  memory-archive/master/install/install.ps1 | iex
#
#  Requirements: PowerShell 5.1+ (Windows 10+) or PowerShell 7+
#                curl.exe ships with Windows 10 1803+ and is used for the
#                download step to render the same progress interface as the
#                Bash installer.
# =============================================================================
#Requires -Version 5.1
[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

# =============================================================================
# 1. CONFIGURATION
# =============================================================================

# Repository
$REPO_OWNER    = "nullvoider07"
$REPO_NAME     = "memory-archive"
$REPO_URL      = "https://github.com/$REPO_OWNER/$REPO_NAME"
$RELEASES_API  = "https://api.github.com/repos/$REPO_OWNER/$REPO_NAME/releases/latest"

# Package components
# Every release archive contains exactly these components:
#
#   ma-core.exe            — Main daemon binary (Rust).  Manages sessions, IPC,
#                            TLS, storage backends, annotator lifecycle.
#   ma-kafka-producer.exe  — Dev-tool binary (Rust).  Bridges gRPC stream → Kafka.
#   ma-app                 — Python TUI + CLI.  Bundled as a self-contained .whl
#                            and installed via pip; provides the `memory-archive`
#                            command.
#   ma-proto               — Protobuf library (Rust). Compiled into the binaries
#                            at build time; no standalone binary is shipped.
#
$RUST_BINARIES  = @("ma-core.exe", "ma-kafka-producer.exe")
$PYTHON_PACKAGE = "ma-app"   # installed from the bundled .whl
$PROTO_LIBRARY  = "ma-proto" # compile-time only; no binary

# Installation directories
# Default to a per-user location so no elevation is required.
# If the session is elevated (admin), install system-wide.
$IsAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator
)

if ($IsAdmin) {
    $MA_HOME     = Join-Path $env:ProgramFiles "MemoryArchive"
} else {
    $MA_HOME     = Join-Path $env:LOCALAPPDATA "MemoryArchive"
}
$INSTALL_DIR = Join-Path $MA_HOME "bin"
$DATA_DIR    = Join-Path $MA_HOME "share"
$LIB_DIR     = Join-Path $MA_HOME "lib"

# Only x86_64 Windows is currently supported.
$ARTIFACT_STEM = "ma-windows-x64"
$ARTIFACT_NAME = "$ARTIFACT_STEM.zip"

# Temp workspace — cleaned up in section 11.
$TMP_DIR = Join-Path ([System.IO.Path]::GetTempPath()) "memory-archive-install-$([System.IO.Path]::GetRandomFileName())"

# =============================================================================
# 2. HELPER FUNCTIONS
# =============================================================================

function Write-Box([string[]]$Lines, [string]$Color = "Cyan") {
    $maxLen = ($Lines | ForEach-Object { $_.Length } | Measure-Object -Maximum).Maximum
    $pad    = $maxLen + 2
    $bar    = "=" * $pad
    Write-Host "  +$bar+" -ForegroundColor $Color
    foreach ($line in $Lines) {
        $spaces = " " * ($pad - $line.Length - 1)
        Write-Host "  | $line$spaces|" -ForegroundColor $Color
    }
    Write-Host "  +$bar+" -ForegroundColor $Color
}

function Write-Banner {
    Write-Host ""
    Write-Box @(
        "Memory Archive - Installer",
        "https://github.com/$REPO_OWNER/$REPO_NAME"
    ) -Color Cyan
    Write-Host ""
}

function Write-Step([string]$Message) {
    Write-Host ""
    Write-Host "  > $Message" -ForegroundColor Cyan
}

function Write-Info([string]$Message) {
    Write-Host "    $Message" -ForegroundColor DarkGray
}

function Write-Ok([string]$Message) {
    Write-Host "  [OK] $Message" -ForegroundColor Green
}

function Write-Warn([string]$Message) {
    Write-Host "  [!!] $Message" -ForegroundColor Yellow
}

function Write-Err([string]$Message) {
    Write-Host "  [XX] ERROR: $Message" -ForegroundColor Red
}

function Invoke-Die([string]$Message) {
    Write-Err $Message
    exit 1
}

# Run a native command while showing an in-place spinner, capturing its stdout
# and stderr so they do not flood the styled installer display. The captured
# output is surfaced only if the command fails.
#   Usage: Invoke-WithSpinner "running message" "done message" <exe> @(args...)
# Returns the command's exit code. On an interactive console an animated spinner
# is drawn in place; otherwise the command runs silently and its log is printed
# only on failure.
$script:SPINNER_FRAMES = @('|', '/', '-', '\')
function Invoke-WithSpinner {
    param(
        [string]  $RunMessage,
        [string]  $DoneMessage,
        [string]  $FilePath,
        [string[]]$ArgumentList
    )

    $log = Join-Path $TMP_DIR "spinner.log"

    # Run the command in a background job via the call operator (&) so argument
    # quoting is preserved on both Windows PowerShell 5.1 and PowerShell 7
    # (Start-Process naively space-joins its ArgumentList, which corrupts paths
    # containing spaces). All command streams are redirected to a log; the job's
    # only pipeline output is $LASTEXITCODE.
    $job = Start-Job -ScriptBlock {
        param($exe, $a, $logPath)
        & $exe @a *> $logPath
        $LASTEXITCODE
    } -ArgumentList $FilePath, $ArgumentList, $log

    $interactive = $false
    try { $interactive = -not [System.Console]::IsOutputRedirected } catch { $interactive = $false }
    if ($interactive) { try { [System.Console]::CursorVisible = $false } catch {} }

    $i = 0
    while ($job.State -eq "Running") {
        if ($interactive) {
            $frame = $script:SPINNER_FRAMES[$i++ % $script:SPINNER_FRAMES.Length]
            Write-Host ("`r  {0}  {1}" -f $frame, $RunMessage) -NoNewline -ForegroundColor Cyan
        }
        Start-Sleep -Milliseconds 100
    }

    $rc = Receive-Job $job
    Remove-Job $job -Force
    if ($null -eq $rc) { $rc = 1 }   # job failed before reporting an exit code
    $rc = [int]$rc

    if ($interactive) {
        Write-Host ("`r" + (" " * ($RunMessage.Length + 6)) + "`r") -NoNewline
        try { [System.Console]::CursorVisible = $true } catch {}
    }

    if ($rc -eq 0) {
        Write-Ok $DoneMessage
    } else {
        Write-Err "$RunMessage — failed"
        if (Test-Path $log) {
            Get-Content -LiteralPath $log | ForEach-Object { Write-Host "    $_" }
        }
    }
    return $rc
}

# Returns $true if a command is found on PATH, $false otherwise.
function Test-Command([string]$Name) {
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

# Compares two "major.minor.patch" version strings.
# Returns $true if $Actual >= $Required.
function Test-VersionGe([string]$Actual, [string]$Required) {
    try {
        return ([Version]$Actual) -ge ([Version]$Required)
    } catch {
        return $false
    }
}

# =============================================================================
# 3. CHECK EXISTING INSTALLATION
# =============================================================================

function Invoke-CheckExistingInstallation {
    Write-Step "Checking for existing installation"

    $foundAny = $false

    foreach ($bin in @("ma-core", "ma-kafka-producer")) {
        $existing = Get-Command $bin -ErrorAction SilentlyContinue
        if ($existing) {
            Write-Warn "Found existing $bin at $($existing.Source)"
            $foundAny = $true
        }
    }

    $maCmd = Get-Command "memory-archive" -ErrorAction SilentlyContinue
    if ($maCmd) {
        $ver = & memory-archive --version 2>$null | Select-Object -First 1
        Write-Warn "Found existing memory-archive ($ver) at $($maCmd.Source)"
        $foundAny = $true
    }

    if ($foundAny) {
        Write-Info "Existing components will be overwritten by this installation."
    } else {
        Write-Ok "No existing installation found."
    }
}

# =============================================================================
# 4. DETECT OS AND ARCHITECTURE
# =============================================================================

function Invoke-DetectPlatform {
    Write-Step "Detecting platform"

    # Verify we are on Windows (guard against running under WSL by mistake).
    if ($env:OS -ne "Windows_NT" -and $IsWindows -eq $false) {
        Invoke-Die "This script is for Windows only.  Use install.sh on Linux / macOS."
    }

    # Architecture — only x86_64 (AMD64) is currently shipped.
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -notin @("AMD64", "x86_64")) {
        Invoke-Die "Unsupported CPU architecture: $arch.  Only x86_64 (AMD64) is supported."
    }

    # Windows version check — Windows 10 build 17763 (1809) minimum.
    $build = [System.Environment]::OSVersion.Version.Build
    if ($build -lt 17763) {
        Invoke-Die "Windows 10 version 1809 or later is required (build $build detected)."
    }

    Write-Ok "Platform: Windows / x86_64 (build $build)"
    Write-Info "Target artifact: $ARTIFACT_NAME"
}

# =============================================================================
# 5. CHECK DEPENDENCIES
# =============================================================================

function Invoke-CheckDependencies {
    Write-Step "Checking dependencies"

    # curl.exe
    # Windows 10 1803+ ships curl.exe in System32.  We use curl.exe (not the
    # PowerShell Invoke-WebRequest alias) so the download progress display
    # (section 7) matches the Bash installer's interface exactly.
    $script:CURL_BIN = $null
    foreach ($candidate in @("curl.exe", "curl")) {
        if (Test-Command $candidate) {
            $curlVer = & $candidate --version 2>$null | Select-Object -First 1
            $script:CURL_BIN = $candidate
            Write-Ok "$candidate  ($($curlVer -replace '^curl\s*',''))"
            break
        }
    }
    if (-not $script:CURL_BIN) {
        Invoke-Die "curl.exe not found.  It ships with Windows 10 1803+.  Please update Windows."
    }

    # Python 3.13+
    $script:PYTHON_BIN = $null
    foreach ($candidate in @("python3.13", "python3.14", "python3", "python")) {
        if (Test-Command $candidate) {
            $verStr = & $candidate -c "import sys; print('.'.join(map(str, sys.version_info[:3])))" 2>$null
            if ($verStr -and (Test-VersionGe $verStr "3.13")) {
                $script:PYTHON_BIN = $candidate
                Write-Ok "Python $verStr  ($candidate)"
                break
            }
        }
    }
    if (-not $script:PYTHON_BIN) {
        Write-Warn "Python 3.13+ not found.  ma-app (the 'memory-archive' CLI) will NOT be installed."
        Write-Warn "Download Python from https://www.python.org/downloads/ and re-run to add the CLI."
    }

    # pip
    $script:PIP_BIN = $null
    if ($script:PYTHON_BIN) {
        $null = & $script:PYTHON_BIN -m pip --version 2>$null
        if ($LASTEXITCODE -eq 0) {
            $script:PIP_BIN = "$($script:PYTHON_BIN) -m pip"
            Write-Ok "pip  (via $($script:PYTHON_BIN) -m pip)"
        } else {
            Write-Warn "pip not found.  ma-app wheel will NOT be installed."
        }
    }

    # Expand-Archive / System.IO.Compression (built-in)
    # Available in all PS 5.1+ environments — no check needed, but confirm.
    Write-Ok "Expand-Archive  (built-in)"
}

# =============================================================================
# 6. GET LATEST RELEASE
# =============================================================================

function Invoke-GetLatestRelease {
    Write-Step "Fetching latest release information"

    $script:RELEASE_TAG = $null

    # Primary: resolve the latest tag from github.com's redirect rather than the
    # Releases API. /releases/latest 302-redirects to /releases/tag/<TAG>; with
    # -L curl.exe follows it and -w reports the final URL, which carries the tag.
    # This path is NOT subject to the api.github.com 60-req/hr unauthenticated
    # rate limit, so it succeeds even when that quota is exhausted.
    $resolvedUrl = & $script:CURL_BIN -fsSLI -o NUL -w "%{url_effective}" "$REPO_URL/releases/latest" 2>$null
    if ($LASTEXITCODE -eq 0 -and $resolvedUrl) {
        $resolvedUrl = ($resolvedUrl | Out-String).Trim()
        if ($resolvedUrl -match "/releases/tag/([^/\s]+)") {
            $script:RELEASE_TAG = $Matches[1]
        }
    }

    # Fallback: the Releases API. Reached only if the redirect yielded no tag.
    # Honours GITHUB_TOKEN to lift the limit from 60 to 5000 req/hr, and
    # distinguishes rate-limiting from real network faults.
    if (-not $script:RELEASE_TAG) {
        $headers = @{
            "Accept"               = "application/vnd.github+json"
            "X-GitHub-Api-Version" = "2022-11-28"
        }
        if ($env:GITHUB_TOKEN) { $headers["Authorization"] = "Bearer $($env:GITHUB_TOKEN)" }

        try {
            $release = Invoke-RestMethod -Uri $RELEASES_API -Headers $headers -ErrorAction Stop
            $script:RELEASE_TAG = $release.tag_name
        } catch {
            $status = $null
            try { $status = [int]$_.Exception.Response.StatusCode } catch {}
            if ($status -eq 403 -or $status -eq 429) {
                Write-Err "GitHub API rate limit exceeded (HTTP $status)."
                Write-Info "This is a rate limit, not a network fault. Options:"
                Write-Info "  - wait for the hourly reset, or retry from a different network/IP"
                Write-Info "  - re-run with a token:  `$env:GITHUB_TOKEN='<token>'; irm <url> | iex"
                exit 1
            } elseif ($status -eq 404) {
                Invoke-Die "No published release found for $REPO_OWNER/$REPO_NAME (HTTP 404). The repository may have only drafts or pre-releases."
            } else {
                Invoke-Die "Failed to reach GitHub API: $_`nCheck your internet connection."
            }
        }
    }

    if (-not $script:RELEASE_TAG) {
        Invoke-Die "Could not determine the latest release tag."
    }

    $script:DOWNLOAD_URL = "$REPO_URL/releases/download/$($script:RELEASE_TAG)/$ARTIFACT_NAME"

    Write-Ok "Latest release: $($script:RELEASE_TAG)"
    Write-Info "Download URL : $($script:DOWNLOAD_URL)"
}

# =============================================================================
# 7. DOWNLOAD THE PACKAGE
# =============================================================================

function Invoke-DownloadPackage {
    Write-Step "Downloading $ARTIFACT_NAME"

    Write-Host ""   # blank line before the progress bar

    $destFile = Join-Path $TMP_DIR $ARTIFACT_NAME

    # --progress-bar renders a single self-updating bar instead of the default
    # multi-line transfer table, keeping the display clean.
    #   -f  fail on HTTP 4xx/5xx
    #   -L  follow redirects (GitHub releases redirect to CDN)
    #   -o  write to named file (suppresses body going to stdout)
    #
    & $script:CURL_BIN -fL --progress-bar -o $destFile $script:DOWNLOAD_URL
    if ($LASTEXITCODE -ne 0) {
        Invoke-Die "Download failed (exit code $LASTEXITCODE).`nURL: $($script:DOWNLOAD_URL)"
    }

    $sizeMB = [math]::Round((Get-Item $destFile).Length / 1MB, 1)
    Write-Ok "Downloaded (${sizeMB} MB)"
}

# =============================================================================
# 7b. VERIFY ARTIFACT INTEGRITY (SHA-256)
# =============================================================================

function Invoke-VerifyChecksum {
    Write-Step "Verifying artifact integrity"

    $sumsUrl  = "$REPO_URL/releases/download/$($script:RELEASE_TAG)/SHA256SUMS"
    $sumsFile = Join-Path $TMP_DIR "SHA256SUMS"
    $destFile = Join-Path $TMP_DIR $ARTIFACT_NAME

    # A missing checksums file is a hard failure — refusing to install an
    # unverified artifact prevents a strip/downgrade attack.
    & $script:CURL_BIN -fsSL -o $sumsFile $sumsUrl
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path $sumsFile)) {
        Invoke-Die "SHA256SUMS not found for $($script:RELEASE_TAG). Refusing to install unverified artifacts.`n  Expected: $sumsUrl"
    }

    $expected = $null
    foreach ($line in Get-Content $sumsFile) {
        $parts = $line -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].Trim() -eq $ARTIFACT_NAME) {
            $expected = $parts[0].Trim().ToLower()
            break
        }
    }
    if (-not $expected) {
        Invoke-Die "No checksum entry for $ARTIFACT_NAME in SHA256SUMS. Refusing to install."
    }

    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $destFile).Hash.ToLower()
    if ($actual -ne $expected) {
        Invoke-Die "Checksum mismatch for $ARTIFACT_NAME — possible tampering.`n  expected: $expected`n  actual  : $actual"
    }

    Write-Ok "Checksum verified (SHA-256)"
}

# =============================================================================
# 8. EXTRACT THE PACKAGE
# =============================================================================

function Invoke-ExtractPackage {
    Write-Step "Extracting $ARTIFACT_NAME"

    $script:EXTRACT_DIR = Join-Path $TMP_DIR "extracted"
    New-Item -ItemType Directory -Force -Path $script:EXTRACT_DIR | Out-Null

    $zipPath = Join-Path $TMP_DIR $ARTIFACT_NAME

    # Traversal guard: reject any entry with a rooted path or a '..' component
    # before extracting, so a crafted archive cannot write outside EXTRACT_DIR
    # (CVE-2007-4559 class).
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName
            if ([System.IO.Path]::IsPathRooted($name) -or
                $name -match '(^|[\\/])\.\.([\\/]|$)') {
                Invoke-Die "Unsafe path in archive, refusing to extract: $name"
            }
        }
    } finally {
        $zip.Dispose()
    }

    # Expand-Archive extracts flat archives directly into the destination dir.
    # -Force overwrites any files if extract dir already has contents.
    Expand-Archive -LiteralPath $zipPath -DestinationPath $script:EXTRACT_DIR -Force `
        -ErrorAction Stop

    $fileCount = @(Get-ChildItem -LiteralPath $script:EXTRACT_DIR -File).Count
    Write-Ok "Extracted ($fileCount components)"
}

# =============================================================================
# 9. INSTALL THE COMPONENTS
# =============================================================================

function Invoke-InstallComponents {
    Write-Step "Installing components"

    # Create directories
    New-Item -ItemType Directory -Force -Path $INSTALL_DIR | Out-Null
    New-Item -ItemType Directory -Force -Path $DATA_DIR    | Out-Null

    # Rust binaries (.exe)
    foreach ($bin in $RUST_BINARIES) {
        $src = Join-Path $script:EXTRACT_DIR $bin
        if (-not (Test-Path $src)) {
            Invoke-Die "Binary not found in archive: $bin.  The release may be malformed."
        }
        $dest = Join-Path $INSTALL_DIR $bin
        Copy-Item -LiteralPath $src -Destination $dest -Force
        Write-Ok "Installed $bin  ->  $dest"
    }

    # Python wheel (ma-app)
    $whl = Get-ChildItem (Join-Path $script:EXTRACT_DIR "*.whl") `
               -ErrorAction SilentlyContinue | Select-Object -First 1

    if ($whl -and $script:PIP_BIN) {
        # Resolve the interpreter path so pip is invoked as "<python> -m pip …".
        # ($script:PIP_BIN is the "<python> -m pip" display string used only for
        # messages; it cannot be passed to the call operator directly.)
        $pythonPath = (Get-Command $script:PYTHON_BIN -ErrorAction SilentlyContinue).Source
        if (-not $pythonPath) { $pythonPath = $script:PYTHON_BIN }

        # Install normally so pip handles the entry point and sys.path correctly.
        $userFlag = if ($IsAdmin) { @() } else { @("--user") }

        # pip's own resolver output ("Requirement already satisfied" lines) is
        # captured by Invoke-WithSpinner and surfaced only on failure, so it does
        # not flood the display. --quiet trims it further; on a fresh machine that
        # still downloads wheels, the spinner shows liveness.
        $pipArgs = @('-m', 'pip', 'install') + $userFlag +
                   @('--quiet', '--disable-pip-version-check', '--no-input', $whl.FullName)
        $pipRc = Invoke-WithSpinner "Installing ma-app (Python CLI)" "Installed ma-app (Python CLI)" `
                     $pythonPath $pipArgs

        if ($pipRc -ne 0) {
            Write-Warn "Run manually: $($script:PIP_BIN) install $(if (-not $IsAdmin) { '--user ' })'$($whl.FullName)'"
        } else {
            # Ask Python where it put the Scripts directory for this install mode.
            $sysScheme = if ($IsAdmin) { "nt" } else { "nt_user" }
            $epDir = & $script:PYTHON_BIN -c "import sysconfig; print(sysconfig.get_path('scripts', '$sysScheme'))"
            $epSrc = Join-Path $epDir "memory-archive.exe"
            if (Test-Path $epSrc) {
                Copy-Item -LiteralPath $epSrc -Destination (Join-Path $INSTALL_DIR "memory-archive.exe") -Force
                Write-Ok "Installed memory-archive  ->  $(Join-Path $INSTALL_DIR 'memory-archive.exe')"
            } else {
                Write-Warn "memory-archive entry point not found at $epSrc — CLI will not be available."
            }
        }
    } elseif (-not $whl) {
        Write-Warn "No .whl found in archive — skipping $PYTHON_PACKAGE install."
    } else {
        Write-Warn "pip unavailable — skipping $PYTHON_PACKAGE install."
    }

    # Documentation
    foreach ($doc in @("LICENSE", "README.md", "INSTALL.md")) {
        $src = Join-Path $script:EXTRACT_DIR $doc
        if (Test-Path $src) {
            Copy-Item -LiteralPath $src -Destination (Join-Path $DATA_DIR $doc) -Force
        }
    }
    Write-Ok "Documentation installed  ->  $DATA_DIR\"
}

# =============================================================================
# 10. UPDATE PATH
# =============================================================================

function Invoke-UpdatePath {
    Write-Step "Checking PATH"

    # Check whether INSTALL_DIR is already in the user's persistent PATH
    # (Machine or User scope in the registry).
    $currentUserPath = [System.Environment]::GetEnvironmentVariable("PATH", "User") ?? ""
    $currentMachinePath = [System.Environment]::GetEnvironmentVariable("PATH", "Machine") ?? ""

    if ($currentUserPath -split ";" -contains $INSTALL_DIR -or
        $currentMachinePath -split ";" -contains $INSTALL_DIR) {
        Write-Ok "$INSTALL_DIR is already on your PATH."
        return
    }

    # Also check the current process PATH in case it was set earlier.
    if ($env:PATH -split ";" -contains $INSTALL_DIR) {
        Write-Ok "$INSTALL_DIR is on the current session PATH."
        return
    }

    # Append to the User PATH in the registry.  This persists across sessions
    # without requiring admin rights.
    $scope = if ($IsAdmin) { "Machine" } else { "User" }
    $existingPath = [System.Environment]::GetEnvironmentVariable("PATH", $scope) ?? ""

    # Guard against a trailing semicolon creating an empty entry.
    $newPath = ($existingPath.TrimEnd(";") + ";" + $INSTALL_DIR).TrimStart(";")
    [System.Environment]::SetEnvironmentVariable("PATH", $newPath, $scope)

    # Also update the current process's PATH so binaries are immediately usable.
    $env:PATH = "$INSTALL_DIR;$env:PATH"

    Write-Ok "Added $INSTALL_DIR to the $scope PATH."
    Write-Info "Open a new PowerShell window for the PATH change to take effect in all sessions."
}

# =============================================================================
# 11. CLEANUP
# =============================================================================

function Invoke-Cleanup {
    # Restore the cursor in case a spinner was interrupted mid-frame.
    try { [System.Console]::CursorVisible = $true } catch {}
    if (Test-Path $TMP_DIR) {
        Remove-Item -LiteralPath $TMP_DIR -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Register cleanup to run even on terminating errors.
# In PowerShell the canonical way is try/finally in main (see section 13).

# =============================================================================
# 12. PRINT SUCCESS MESSAGE
# =============================================================================

function Write-SuccessMessage {
    Write-Host ""
    Write-Box @("Memory Archive installed successfully!") -Color Green
    Write-Host ""
    Write-Host ("  Version  : " + $script:RELEASE_TAG)
    Write-Host ("  Binaries : " + (Join-Path $INSTALL_DIR "ma-core.exe"))
    Write-Host ("             " + (Join-Path $INSTALL_DIR "ma-kafka-producer.exe"))
    $maPath = (Get-Command "memory-archive" -ErrorAction SilentlyContinue)?.Source
    if ($maPath) {
        Write-Host ("  CLI      : " + $maPath)
    }
    Write-Host ("  Docs     : " + $DATA_DIR)
    Write-Host ""
    Write-Host "  Quick start:" -ForegroundColor Cyan
    Write-Host "    # Start the core daemon"                          -ForegroundColor DarkGray
    Write-Host "    ma-core.exe"
    Write-Host ""
    Write-Host "    # Annotate a session"                             -ForegroundColor DarkGray
    Write-Host "    memory-archive annotate <session-id>"
    Write-Host ""
    Write-Host "    # Documentation"                                  -ForegroundColor DarkGray
    Write-Host "    $REPO_URL#readme"
    Write-Host ""
}

# =============================================================================
# 13. MAIN INSTALLATION FLOW
# =============================================================================

function Main {
    Write-Banner

    try {
        Invoke-CheckExistingInstallation  # section 3
        Invoke-DetectPlatform             # section 4
        Invoke-CheckDependencies          # section 5
        Invoke-GetLatestRelease           # section 6

        # Create TMP_DIR only after we know we have something to download.
        New-Item -ItemType Directory -Force -Path $TMP_DIR | Out-Null

        Invoke-DownloadPackage            # section 7
        Invoke-VerifyChecksum             # section 7b
        Invoke-ExtractPackage             # section 8
        Invoke-InstallComponents          # section 9
        Invoke-UpdatePath                 # section 10
        Write-SuccessMessage              # section 12
    } finally {
        Invoke-Cleanup                    # section 11 — runs even on error
    }
}

Main