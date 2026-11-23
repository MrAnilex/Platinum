# Prepares this checkout to build with Rust's x86_64-pc-windows-gnu toolchain (see README,
# "Building from source").
#
# Why it is needed: Slint's Windows dependencies build import libraries at compile time
# (raw-dylib). The dlltool shipped with the GNU toolchain calls an assembler most machines do not
# have, and GNU ld cannot use the import libraries LLVM's dlltool makes. So the whole build links
# with LLVM's linker, rust-lld (shipped with Rust), and builds import libraries with LLVM's
# dlltool, which is llvm-ar from rustup's llvm-tools component under the name llvm-dlltool.exe.
#
# What it does:
#   1. installs the stable x86_64-pc-windows-gnu toolchain and its llvm-tools component;
#   2. links (or copies) llvm-ar.exe to .tools\llvm-dlltool.exe;
#   3. writes .cargo\config.toml with this machine's absolute paths.
# Both .tools\ and .cargo\config.toml are machine-specific and ignored by git.
#
# Usage, from the repository root:
#   powershell -ExecutionPolicy Bypass -File scripts\setup-toolchain.ps1
# Options:
#   -NoInstall      skip the rustup steps (toolchain and component already there)
#   -Output <file>  write the configuration elsewhere instead of .cargo\config.toml

param(
    [switch]$NoInstall,
    [string]$Output
)

$ErrorActionPreference = 'Stop'
$toolchain = 'stable-x86_64-pc-windows-gnu'
$target = 'x86_64-pc-windows-gnu'
$root = Split-Path -Parent $PSScriptRoot

if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    throw 'rustup was not found: install Rust from https://rustup.rs first.'
}

if (-not $NoInstall) {
    & rustup toolchain install $toolchain --profile minimal
    if ($LASTEXITCODE -ne 0) { throw "rustup could not install $toolchain" }
    & rustup component add llvm-tools --toolchain $toolchain
    if ($LASTEXITCODE -ne 0) { throw 'rustup could not add the llvm-tools component' }
}

$sysroot = (& rustup run $toolchain rustc --print sysroot).Trim()
if ($LASTEXITCODE -ne 0 -or -not $sysroot) { throw "cannot find the sysroot of $toolchain" }
$bin = Join-Path $sysroot "lib\rustlib\$target\bin"
$lib = Join-Path $sysroot "lib\rustlib\$target\lib\self-contained"
$lld = Join-Path $bin 'rust-lld.exe'
$ar = Join-Path $bin 'llvm-ar.exe'
foreach ($needed in @($lld, $ar, $lib)) {
    if (-not (Test-Path $needed)) { throw "missing: $needed (is the llvm-tools component installed?)" }
}

$tools = Join-Path $root '.tools'
$dlltool = Join-Path $tools 'llvm-dlltool.exe'
if (-not (Test-Path $dlltool)) {
    New-Item -ItemType Directory -Force $tools | Out-Null
    try {
        # A hard link takes no room; it needs the repository and the toolchain on the same drive.
        New-Item -ItemType HardLink -Path $dlltool -Target $ar | Out-Null
    } catch {
        Copy-Item $ar $dlltool
    }
    Write-Host "dlltool: $dlltool"
}

$slash = { param($path) $path.Replace('\', '/') }
$config = @"
# Written by scripts/setup-toolchain.ps1 for this machine; not tracked by git.
[target.$target]
linker = "$(& $slash $lld)"
rustflags = [
    "-C", "linker-flavor=ld.lld",
    "-C", "dlltool=$(& $slash $dlltool)",
    "-L", "$(& $slash $lib)",
]
"@

if (-not $Output) {
    $folder = Join-Path $root '.cargo'
    New-Item -ItemType Directory -Force $folder | Out-Null
    $Output = Join-Path $folder 'config.toml'
}
# UTF-8 without a byte order mark, which Cargo's TOML reader does not expect.
[System.IO.File]::WriteAllText($Output, $config, (New-Object System.Text.UTF8Encoding $false))
Write-Host "configuration: $Output"
Write-Host "now: cargo +$toolchain build --release --workspace"
