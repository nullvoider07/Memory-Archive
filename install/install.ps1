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

    # Use Invoke-RestMethod (the same cmdlet `irm` is aliased to) for the
    # lightweight API query — it parses JSON automatically.
    $headers = @{
        "Accept"               = "application/vnd.github+json"
        "X-GitHub-Api-Version" = "2022-11-28"
    }

    try {
        $release = Invoke-RestMethod -Uri $RELEASES_API -Headers $headers -ErrorAction Stop
    } catch {
        Invoke-Die "Failed to reach GitHub API: $_`nCheck your internet connection."
    }

    $script:RELEASE_TAG   = $release.tag_name
    $script:DOWNLOAD_URL  = "$REPO_URL/releases/download/$($script:RELEASE_TAG)/$ARTIFACT_NAME"

    if (-not $script:RELEASE_TAG) {
        Invoke-Die "Could not parse release tag from GitHub API response."
    }

    Write-Ok "Latest release: $($script:RELEASE_TAG)"
    Write-Info "Download URL : $($script:DOWNLOAD_URL)"
}

# =============================================================================
# 7. DOWNLOAD THE PACKAGE
# =============================================================================

function Invoke-DownloadPackage {
    Write-Step "Downloading $ARTIFACT_NAME"

    Write-Host ""   # blank line before the curl progress meter

    $destFile = Join-Path $TMP_DIR $ARTIFACT_NAME

    # curl.exe renders the same transfer-stats table as on Linux / macOS:
    #
    #   % Total    % Received % Xferd  Average Speed   Time    Time    Time  Current
    #                                  Dload  Upload   Total   Spent   Left  Speed
    #  100 37.2M  100 37.2M    0     0  2483k      0  0:00:15  0:00:15 ...
    #
    # Flags:
    #   -f  fail on HTTP 4xx/5xx
    #   -L  follow redirects (GitHub releases redirect to CDN)
    #   -o  write to named file (suppresses body going to stdout)
    #
    & $script:CURL_BIN -fL -o $destFile $script:DOWNLOAD_URL
    if ($LASTEXITCODE -ne 0) {
        Invoke-Die "Download failed (exit code $LASTEXITCODE).`nURL: $($script:DOWNLOAD_URL)"
    }

    Write-Host ""   # blank line after progress meter
    $sizeKB = [math]::Round((Get-Item $destFile).Length / 1MB, 1)
    Write-Ok "Download complete: ${sizeKB} MB"
}

# =============================================================================
# 8. EXTRACT THE PACKAGE
# =============================================================================

function Invoke-ExtractPackage {
    Write-Step "Extracting $ARTIFACT_NAME"

    $script:EXTRACT_DIR = Join-Path $TMP_DIR "extracted"
    New-Item -ItemType Directory -Force -Path $script:EXTRACT_DIR | Out-Null

    $zipPath = Join-Path $TMP_DIR $ARTIFACT_NAME

    # Expand-Archive extracts flat archives directly into the destination dir.
    # -Force overwrites any files if extract dir already has contents.
    Expand-Archive -LiteralPath $zipPath -DestinationPath $script:EXTRACT_DIR -Force `
        -ErrorAction Stop

    Write-Ok "Extracted to $($script:EXTRACT_DIR)"
    Write-Info "Archive contents:"
    Get-ChildItem $script:EXTRACT_DIR | ForEach-Object {
        $size = if ($_.PSIsContainer) { "<dir>" } else { "$([math]::Round($_.Length/1KB,1)) KB" }
        Write-Info ("  {0,-40} {1}" -f $_.Name, $size)
    }
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
        Write-Info "Installing Python package from $($whl.Name) ..."

        # Install normally so pip handles the entry point and sys.path correctly.
        $userFlag = if ($IsAdmin) { @() } else { @("--user") }
        & $script:PIP_BIN -m pip install @userFlag $whl.FullName

        if ($LASTEXITCODE -ne 0) {
            Write-Warn "pip install returned exit code $LASTEXITCODE."
            Write-Warn "Run manually: $($script:PIP_BIN) -m pip install $userFlag '$($whl.FullName)'"
        } else {
            # Ask Python where it put the Scripts directory for this install mode.
            $sysScheme = if ($IsAdmin) { "nt" } else { "nt_user" }
            $epDir = & $script:PIP_BIN -c "import sysconfig; print(sysconfig.get_path('scripts', '$sysScheme'))"
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
        Invoke-ExtractPackage             # section 8
        Invoke-InstallComponents          # section 9
        Invoke-UpdatePath                 # section 10
        Write-SuccessMessage              # section 12
    } finally {
        Invoke-Cleanup                    # section 11 — runs even on error
    }
}

Main