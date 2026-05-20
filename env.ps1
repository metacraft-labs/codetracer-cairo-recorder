# codetracer-cairo-recorder Windows dev environment (PowerShell)
# Usage: . .\env.ps1
#
# The recorder builds with a plain `cargo build` / `cargo test`; its only
# non-standard requirements on Windows are:
#
#   1. The shared CodeTracer toolchain (Rust, Nim + nimble, just, Cap'n Proto,
#      MSVC).  These are provisioned by the main `codetracer` repo's env.ps1,
#      which this script dot-sources.  The Nim toolchain is needed because the
#      `codetracer_trace_writer_nim` crate's build script compiles a Nim static
#      library.
#
#   2. The Cairo corelib.  `cairo-lang-compiler` cannot compile a `.cairo`
#      source without the standard library; the Nix `cairo` dev shell normally
#      supplies it.  On Windows we download the corelib that ships inside the
#      matching `starkware-libs/cairo` compiler release (it is plain Cairo
#      source, platform-independent) and point `CAIRO_CORELIB_DIR` at it.  The
#      recorder's `find_corelib_path()` honours that variable first.
#
#   3. An explicit MSVC linker for the `x86_64-pc-windows-msvc` target.  The
#      `just test` recipe runs `tests/verify-cli-convention-no-silent-skip.sh`
#      via bash, and that script invokes `cargo build`.  Git Bash ships a
#      coreutils `link.exe` (the hard-link tool) in its `usr/bin`, and a bash
#      login shell re-orders PATH so `usr/bin` precedes the MSVC toolchain.
#      Cargo would then resolve `link.exe` to coreutils `link` and the link
#      fails with `link: missing operand`.  Pointing
#      `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER` at MSVC's absolute
#      `link.exe` bypasses PATH resolution entirely, so every build -- whether
#      launched from PowerShell or from a bash `just` recipe -- links with the
#      correct linker.

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition

# --- 1. Shared CodeTracer toolchain -----------------------------------------
# The blockchain recorders do not need FPC, LLVM, nargo or dotnet; skip those
# bootstrap steps so activation is fast.
$env:WINDOWS_DIY_SKIP_FPC = "1"
$env:WINDOWS_DIY_SKIP_LLVM = "1"
$env:WINDOWS_DIY_SKIP_NARGO = "1"
$env:WINDOWS_DIY_SKIP_DOTNET = "1"

$codetracerEnv = Join-Path (Split-Path -Parent $scriptDir) "codetracer\env.ps1"
if (-not (Test-Path $codetracerEnv)) {
    throw "Could not find the shared CodeTracer env.ps1 at $codetracerEnv -- the `codetracer` repo must be checked out as a sibling of this repo."
}
. $codetracerEnv

# --- 2. Explicit MSVC linker (immune to Git Bash PATH reordering) -----------
if ($env:WINDOWS_DIY_CL_EXE -and (Test-Path $env:WINDOWS_DIY_CL_EXE)) {
    $msvcBin = Split-Path -Parent $env:WINDOWS_DIY_CL_EXE
    $msvcLink = Join-Path $msvcBin "link.exe"
    if (Test-Path $msvcLink) {
        $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $msvcLink
    }
    # Also prepend the MSVC bin dir so the linker can find its own DLLs and
    # any auxiliary tools it spawns.
    if ($env:Path -notlike "$msvcBin;*") {
        $env:Path = "$msvcBin;$($env:Path)"
    }
}

# --- 3. Cairo corelib -------------------------------------------------------
# The corelib version must match the `cairo-lang-*` crate versions pinned in
# Cargo.toml.  Keep this in sync with the `cairo-lang-compiler` version there.
$cairoVersion = "2.17.0-rc.4"
$devDepsRoot = if ($env:WINDOWS_DIY_INSTALL_ROOT) { $env:WINDOWS_DIY_INSTALL_ROOT }
               elseif (Test-Path "D:\") { "D:\metacraft-dev-deps" }
               else { Join-Path $env:LOCALAPPDATA "codetracer\windows-diy" }
$corelibRoot = Join-Path $devDepsRoot "cairo-corelib\$cairoVersion"
$corelibSrc = Join-Path $corelibRoot "corelib\src"

if (Test-Path $corelibSrc) {
    Write-Host "Cairo corelib $cairoVersion already installed"
} else {
    Write-Host "Installing Cairo corelib $cairoVersion..."
    # The corelib ships inside every platform's compiler release tarball; the
    # Linux musl archive is the smallest reliable source and the corelib is
    # platform-independent Cairo source.
    $relUrl = "https://github.com/starkware-libs/cairo/releases/download/v$cairoVersion/release-x86_64-unknown-linux-musl.tar.gz"
    $relTar = Join-Path $env:TEMP "cairo-$cairoVersion.tar.gz"
    Invoke-WebRequest -Uri $relUrl -OutFile $relTar
    New-Item -ItemType Directory -Force -Path $corelibRoot | Out-Null
    # `tar` (bsdtar) ships with Windows 10+; extract only the corelib subtree.
    & tar -xzf $relTar -C $corelibRoot --strip-components=1 cairo/corelib
    if ($LASTEXITCODE -ne 0) { throw "Failed to extract Cairo corelib from release archive" }
    Remove-Item $relTar -Force -ErrorAction SilentlyContinue
    if (-not (Test-Path $corelibSrc)) { throw "Cairo corelib src not found after extraction" }
    Write-Host "Installed Cairo corelib to $corelibRoot"
}
$env:CAIRO_CORELIB_DIR = $corelibSrc

Write-Host "CAIRO_CORELIB_DIR=$env:CAIRO_CORELIB_DIR"
Write-Host "codetracer-cairo-recorder dev environment ready."
