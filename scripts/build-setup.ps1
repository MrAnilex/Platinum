# Builds Platinum-Setup.exe: Platinum's release programs first, then the setup, which carries
# them (setup/build.rs takes them from target\release), into dist\Platinum-Setup.exe.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-setup.ps1
#
# The toolchain is the one scripts\setup-toolchain.ps1 prepares (Rust's x86_64-pc-windows-gnu).
# Nothing of SteelSeries goes into the setup: it downloads GG's official installer on the user's
# PC (see README.md, "Install").
#
# Crash reports open issues on github.com/MrAnilex/Platinum; a fork names its own repository with
# PLATINUM_REPO ("owner/name") when the programs are built.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    if ($env:PLATINUM_REPO) {
        Write-Host "Crash reports will open issues on github.com/$($env:PLATINUM_REPO)"
    } else {
        Write-Host 'Crash reports will open issues on github.com/MrAnilex/Platinum (set PLATINUM_REPO for a fork)'
    }
    Write-Host 'Building platinum.exe, platinum-start.exe and platinum-ui.exe...'
    cargo build --release -p platinum -p platinum-ui
    if ($LASTEXITCODE -ne 0) { throw 'building Platinum failed' }

    # The setup's build script reruns when one of the three programs changed, so the setup always
    # carries the programs just built.
    Write-Host 'Building the setup...'
    cargo build --release -p platinum-setup
    if ($LASTEXITCODE -ne 0) { throw 'building the setup failed' }

    New-Item -ItemType Directory -Force -Path 'dist' | Out-Null
    $setup = Join-Path 'dist' 'Platinum-Setup.exe'
    Copy-Item -Force 'target\release\platinum-setup.exe' $setup
    $size = (Get-Item $setup).Length
    $hash = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Host ("{0}: {1:N0} bytes, SHA-256 {2}" -f $setup, $size, $hash)

    # A setup built without the programs refuses to install: say so here rather than later.
    $plan = & $setup --dry-run 2>&1 | Out-String
    if ($plan -match 'carries no Platinum programs') { throw 'the setup was built without the programs' }
    Write-Host 'Check it without installing anything: dist\Platinum-Setup.exe --dry-run'
}
finally {
    Pop-Location
}
