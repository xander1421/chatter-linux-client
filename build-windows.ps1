# =============================================================================
# build-windows.ps1 - Build the Chatter Windows client (full/default `desktop`
# feature, with LiveKit calls) on a native MSVC machine.
#
# Run from an ELEVATED PowerShell the FIRST time (the full Skia source build
# extracts symlinks, which need the symlink privilege; also enables
# LongPathsEnabled). Later incremental builds can run non-elevated.
#
# Output: C:\t\release\chatter-ui.exe  (short target dir to dodge MAX_PATH).
# =============================================================================
$ErrorActionPreference = 'Stop'

# Rust toolchain on PATH (an elevated token may not inherit the user PATH).
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

# Short target dir + long paths: Skia's deep relative include paths and long
# source filenames overflow Windows MAX_PATH (260) under the normal target dir.
try {
  Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem" -Name LongPathsEnabled -Value 1 -Type DWord
} catch { Write-Warning "Could not set LongPathsEnabled (need admin): $_" }
$env:CARGO_TARGET_DIR = "C:\t"
New-Item -ItemType Directory -Force -Path $env:CARGO_TARGET_DIR | Out-Null

# --- MSVC environment (cl.exe, link.exe, INCLUDE, LIB) via vcvars64 ---
$vcvars = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvars)) { throw "vcvars64.bat not found at $vcvars" }
Write-Host "Importing MSVC environment..."
cmd /c "`"$vcvars`" && set" | ForEach-Object {
  if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "Env:$($matches[1])" -Value $matches[2] }
}

# --- Portable tools (protoc, pkg-config, nasm, sccache) ---
$env:Path = "C:\Users\win11pc\build-tools\bin;$env:Path"
$env:PROTOC = "C:\Users\win11pc\build-tools\bin\protoc.exe"

# --- GStreamer (MSVC x86_64) ---
$gst = "C:\gstreamer\1.0\msvc_x86_64"
$env:GST_ROOT = $gst
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = $gst
$env:Path = "$gst\bin;$env:Path"
$env:PKG_CONFIG_PATH = "$gst\lib\pkgconfig"
$env:LIB = "$gst\lib;$env:LIB"

# --- LLVM/Clang for skia-bindings (clang-cl + libclang) ---
$llvm = "C:\Program Files\LLVM"
$env:Path = "$llvm\bin;$env:Path"
$env:LIBCLANG_PATH = "$llvm\bin"

# --- Python 3 for the full Skia source build (no prebuilt for this config) ---
$pydir = "C:\Users\win11pc\AppData\Local\Programs\Python\Python312"
$env:Path = "$pydir;$pydir\Scripts;$env:Path"

# --- SQLCipher crypto provider = webrtc's bundled BoringSSL ------------------
# Mirrors android/build-android.sh `setup_boringssl_for_sqlcipher`. rusqlite
# `bundled-sqlcipher` (libsqlite3-sys) compiles SQLCipher's C and links one
# `libcrypto`. If that is a SEPARATE OpenSSL (e.g. vcpkg), it collides with the
# BoringSSL baked into LiveKit's webrtc.lib on the same symbol names (EVP_*,
# HMAC, PKCS5_PBKDF2_HMAC, ...) with a different ABI -> heap corruption -> the
# app hangs/crashes on the first PRAGMA key. Pointing OPENSSL_* at webrtc's
# BoringSSL gives the binary exactly ONE libcrypto, so the DB opens.
# Requires webrtc.lib to exist first (webrtc-sys downloads it during a build);
# falls back to vcpkg OpenSSL with a warning if it's not there yet.
$webrtcLib = Get-ChildItem -Path $env:CARGO_TARGET_DIR -Recurse -Filter webrtc.lib -ErrorAction SilentlyContinue |
  Where-Object FullName -match 'win-x64-release' | Select-Object -First 1
$bsslInc = Get-ChildItem -Path $env:CARGO_TARGET_DIR -Recurse -Directory -ErrorAction SilentlyContinue |
  Where-Object FullName -match 'boringssl[\\/]src[\\/]include$' | Select-Object -First 1
if ($webrtcLib -and $bsslInc) {
  $stage = "$env:CARGO_TARGET_DIR\boringssl-sqlcipher"
  New-Item -ItemType Directory -Force -Path "$stage\lib" | Out-Null
  Copy-Item $webrtcLib.FullName "$stage\lib\libcrypto.lib" -Force   # -lcrypto -> BoringSSL
  $env:OPENSSL_DIR         = $stage
  $env:OPENSSL_LIB_DIR     = "$stage\lib"
  $env:OPENSSL_INCLUDE_DIR = $bsslInc.FullName
  $env:OPENSSL_STATIC      = "1"
  # SQLCipher's sqlite3.c includes <windows.h>, which pulls in <wincrypt.h>;
  # its X509_NAME / X509_EXTENSIONS / PKCS7_* etc. macros collide with
  # BoringSSL's typedefs (base.h: "typedef struct X509_name_st X509_NAME;" ->
  # C2059). OpenSSL's own headers #undef these; BoringSSL does not. NOCRYPT
  # keeps wincrypt.h out (SQLCipher uses BoringSSL's API, not Windows CryptoAPI).
  $env:LIBSQLITE3_FLAGS = "-DNOCRYPT"
  Write-Host "SQLCipher -> BoringSSL: $stage\lib\libcrypto.lib  inc=$($bsslInc.FullName)"
} else {
  $opensslDir = "C:\vcpkg\installed\x64-windows-static"
  $env:OPENSSL_DIR=$opensslDir; $env:OPENSSL_LIB_DIR="$opensslDir\lib"
  $env:OPENSSL_INCLUDE_DIR="$opensslDir\include"; $env:OPENSSL_STATIC="1"
  Write-Warning "webrtc.lib not found under $env:CARGO_TARGET_DIR -> using vcpkg OpenSSL. The DB will hang/crash on the LiveKit build. Build once so webrtc-sys downloads webrtc.lib, then re-run (and clean libsqlite3-sys) to relink SQLCipher against BoringSSL."
}

Set-Location $PSScriptRoot
Write-Host "`n=== cargo build --release (sccache wrapper disabled) ===`n"

# PRESENT-but-EMPTY RUSTC_WRAPPER disables the config.toml sccache wrapper
# (sccache crashes rustc here). PowerShell can't hold an empty env var, so set
# it via ProcessStartInfo's environment dictionary.
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = (Get-Command cargo).Source
$psi.Arguments = "build --release"
$psi.UseShellExecute = $false
$psi.WorkingDirectory = $PSScriptRoot
$psi.EnvironmentVariables["RUSTC_WRAPPER"] = ""
$psi.EnvironmentVariables["CARGO_BUILD_RUSTC_WRAPPER"] = ""
$proc = [System.Diagnostics.Process]::Start($psi)
$proc.WaitForExit()
Write-Host "`ncargo exit: $($proc.ExitCode)"
if ($proc.ExitCode -eq 0) { Write-Host "Built: $env:CARGO_TARGET_DIR\release\chatter-ui.exe" }
exit $proc.ExitCode
