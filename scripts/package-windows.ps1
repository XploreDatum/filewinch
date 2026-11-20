<#
.SYNOPSIS
  Builds a release Filewinch for Windows and packages it into dist\windows\.

.DESCRIPTION
  Produces:
    Filewinch-<version>-windows-<arch>.zip   Filewinch\ folder with Filewinch.exe, filewinch.ico,
                                             Install-StartMenuShortcut.ps1 (+ LICENSE/README if
                                             the repo has them)
    Filewinch-<version>-windows-<arch>.msi   per-user installer, when cargo-wix and WiX v3 exist
    symbols\<version>\<arch>\filewinch.pdb   crash-report symbols; never in the zip or the MSI
  <arch> is x64 or arm64.

  The release profile strips symbols, which on MSVC also means no PDB, so this script builds with
  CARGO_PROFILE_RELEASE_STRIP=none: the linker then writes filewinch.pdb beside the exe, and the
  exe itself still holds no debug info (only the PDB's name and id). With -NoBuild, build the
  same way (the release workflow does) or there is no PDB to keep.

  Usage (on Windows, PowerShell 5.1 or later, from anywhere in the repo):
    powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1
    powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1 -Target aarch64-pc-windows-msvc
    powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1 -Features "sftp ftp"
    powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1 -SkipMsi
    powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1 -NoBuild
      (packages the exe already built for -Target, e.g. after signing it; the release workflow
      builds, signs Filewinch.exe, packages with -NoBuild, then signs the MSI)

  Prerequisites:
    - Rust via rustup, plus the target: rustup target add x86_64-pc-windows-msvc (or aarch64-...)
    - Visual Studio 2019+ Build Tools with "Desktop development with C++" (MSVC linker and the
      Windows SDK, whose rc.exe build.rs uses to embed the icon, version info and manifest).
    - For the MSI: cargo install cargo-wix, and the WiX Toolset v3.14
      (https://github.com/wixtoolset/wix3/releases; its installer sets %WIX%).
  The MSI's template is packaging\windows\main.wxs.

  A local (gitignored) .cargo\config.toml may set target-dir to another machine's drive; when its
  parent folder doesn't exist here and CARGO_TARGET_DIR is unset, this script builds into <repo>\target.

.PARAMETER Target
  The Rust target triple: x86_64-pc-windows-msvc (default) or aarch64-pc-windows-msvc.

.PARAMETER Features
  Backends to include, space- or comma-separated (default: all of them). Release builds leave
  out the app's dev-tools feature (headless snapshots, scenes, demo data) either way.

.PARAMETER SkipMsi
  Only build the zip.

.PARAMETER NoBuild
  Don't run cargo build; package the existing target\<Target>\release\filewinch.exe.
#>
[CmdletBinding()]
param(
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string]$Target = 'x86_64-pc-windows-msvc',
    [string]$Features = '',
    [switch]$SkipMsi,
    [switch]$NoBuild
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

# Runs a native command with the arguments given, and throws when it exits non-zero
# (PowerShell 5.1 doesn't). stdout is returned; stderr goes straight to the console.
function Invoke-Native {
    param([Parameter(Mandatory)][string]$Exe, [string[]]$Arguments = @())
    # 'Continue' here so a native program's stderr lines never become terminating errors
    # (PowerShell 5.1 wraps them when output is redirected); the exit code decides.
    $ErrorActionPreference = 'Continue'
    $output = & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "'$Exe $($Arguments -join ' ')' failed with exit code $LASTEXITCODE."
    }
    $output
}

if ($env:OS -ne 'Windows_NT') {
    throw 'Run this on Windows (scripts/cross-check.sh windows type-checks from a Mac or Linux).'
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo not found. Install Rust from https://rustup.rs and open a new terminal.'
}

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).ProviderPath
Push-Location -LiteralPath $Root
$SavedTargetDir = $env:CARGO_TARGET_DIR
$SavedStrip = $env:CARGO_PROFILE_RELEASE_STRIP
try {
    $Metadata = (Invoke-Native cargo @('metadata', '--format-version', '1', '--no-deps')) -join "`n" | ConvertFrom-Json
    $Package = @($Metadata.packages | Where-Object { $_.name -eq 'filewinch-app' })
    if ($Package.Count -ne 1) { throw 'cargo metadata lists no filewinch-app package.' }
    $Version = $Package[0].version
    $TargetDir = $Metadata.target_directory
    if (-not $env:CARGO_TARGET_DIR -and -not (Test-Path -LiteralPath (Split-Path -Parent $TargetDir))) {
        Write-Host "Configured target dir $TargetDir isn't usable here; building into $Root\target"
        $env:CARGO_TARGET_DIR = Join-Path $Root 'target'
        $TargetDir = $env:CARGO_TARGET_DIR
    }

    $Arch = @{ 'x86_64-pc-windows-msvc' = 'x64'; 'aarch64-pc-windows-msvc' = 'arm64' }[$Target]
    if (Get-Command rustup -ErrorAction SilentlyContinue) {
        $Installed = Invoke-Native rustup @('target', 'list', '--installed')
        if ($Installed -notcontains $Target) {
            throw "The Rust target $Target isn't installed. Run: rustup target add $Target"
        }
    }

    $Exe = Join-Path $TargetDir "$Target\release\filewinch.exe"
    if ($NoBuild) {
        if ($Features) { throw '-Features has no effect with -NoBuild; pass the features to the cargo build instead.' }
        if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
            throw "-NoBuild: $Exe doesn't exist. Run: cargo build --release --locked -p filewinch-app --target $Target --no-default-features --features backends (with CARGO_PROFILE_RELEASE_STRIP=none)"
        }
        Write-Host "== packaging the existing $Exe (-NoBuild)"
    } else {
        $BuildFeatures = if ($Features) { $Features } else { 'backends' }
        $BuildArgs = @('build', '--release', '--locked', '-p', 'filewinch-app', '--target', $Target,
            '--no-default-features', '--features', $BuildFeatures)
        $env:CARGO_PROFILE_RELEASE_STRIP = 'none'
        Write-Host "== CARGO_PROFILE_RELEASE_STRIP=none cargo $($BuildArgs -join ' ')"
        Invoke-Native cargo $BuildArgs | Out-Host
        if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) { throw "The build finished but $Exe is missing." }
    }

    $Dist = Join-Path $Root 'dist\windows'
    New-Item -ItemType Directory -Force -Path $Dist | Out-Null
    $Name = "Filewinch-$Version-windows-$Arch"

    # --- symbols: kept beside dist, never packaged -----------------------------------------------
    $Pdb = Join-Path $TargetDir "$Target\release\filewinch.pdb"
    if (-not (Test-Path -LiteralPath $Pdb -PathType Leaf)) {
        throw "$Pdb is missing, so crash reports couldn't be symbolicated. Build with CARGO_PROFILE_RELEASE_STRIP=none (this script does unless -NoBuild)."
    }
    $Symbols = Join-Path $Dist "symbols\$Version\$Arch"
    New-Item -ItemType Directory -Force -Path $Symbols | Out-Null
    Copy-Item -LiteralPath $Pdb -Destination $Symbols -Force
    Write-Host "symbols (keep, don't ship): $(Join-Path $Symbols 'filewinch.pdb')"

    # --- zip -------------------------------------------------------------------------------------
    $Stage = Join-Path $Dist ".$Name.building"
    if (Test-Path -LiteralPath $Stage) { Remove-Item -LiteralPath $Stage -Recurse -Force }
    $AppDir = Join-Path $Stage 'Filewinch'
    New-Item -ItemType Directory -Force -Path $AppDir | Out-Null
    try {
        Copy-Item -LiteralPath $Exe -Destination (Join-Path $AppDir 'Filewinch.exe')
        Copy-Item -LiteralPath (Join-Path $Root 'assets\icon\filewinch.ico') -Destination $AppDir
        Copy-Item -LiteralPath (Join-Path $Root 'packaging\windows\Install-StartMenuShortcut.ps1') -Destination $AppDir
        foreach ($Doc in @('LICENSE', 'LICENSE.md', 'LICENSE.txt', 'README.md')) {
            $Path = Join-Path $Root $Doc
            if (Test-Path -LiteralPath $Path -PathType Leaf) { Copy-Item -LiteralPath $Path -Destination $AppDir }
        }

        $Zip = Join-Path $Dist "$Name.zip"
        $ZipBuilding = "$Zip.building"
        if (Test-Path -LiteralPath $ZipBuilding) { Remove-Item -LiteralPath $ZipBuilding -Force }
        # ZipFile rather than Compress-Archive: PowerShell 5.1's Compress-Archive writes backslashes
        # into entry names, which other unzip tools read as part of the file name.
        Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
        [IO.Compression.ZipFile]::CreateFromDirectory($AppDir, $ZipBuilding,
            [IO.Compression.CompressionLevel]::Optimal, $true)
        Move-Item -LiteralPath $ZipBuilding -Destination $Zip -Force
        Write-Host "wrote $Zip"
    } finally {
        if (Test-Path -LiteralPath $Stage) { Remove-Item -LiteralPath $Stage -Recurse -Force }
    }

    # --- MSI -------------------------------------------------------------------------------------
    if ($SkipMsi) { return }
    $Missing = @()
    if (-not (Get-Command cargo-wix -ErrorAction SilentlyContinue)) {
        $Missing += 'cargo-wix (cargo install cargo-wix)'
    }
    $HasWix = (Get-Command candle.exe -ErrorAction SilentlyContinue) -or
        ($env:WIX -and (Test-Path -LiteralPath (Join-Path $env:WIX 'bin\candle.exe')))
    if (-not $HasWix) {
        $Missing += 'the WiX Toolset v3.14 (https://github.com/wixtoolset/wix3/releases; sets %WIX%)'
    }
    if ($Missing.Count -gt 0) {
        Write-Host "Skipping the MSI: missing $($Missing -join ' and '). The zip is complete without it."
        return
    }

    $Msi = Join-Path $Dist "$Name.msi"
    $WixArgs = @(
        'wix', '--package', 'filewinch-app', '--no-build', '--nocapture',
        '--target', $Target,
        '--include', (Join-Path $Root 'packaging\windows\main.wxs'),
        '-C', "-dRepoRoot=$Root",
        '--output', $Msi
    )
    Write-Host "== cargo $($WixArgs -join ' ')"
    Invoke-Native cargo $WixArgs | Out-Host
    if (-not (Test-Path -LiteralPath $Msi -PathType Leaf)) { throw "cargo wix finished but $Msi is missing." }
    Write-Host "wrote $Msi"
} finally {
    $env:CARGO_TARGET_DIR = $SavedTargetDir
    $env:CARGO_PROFILE_RELEASE_STRIP = $SavedStrip
    Pop-Location
}
