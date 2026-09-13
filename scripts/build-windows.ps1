#requires -Version 5.1
<# Native x64 release with a static CRT, so a installed package needs no VC++ Redistributable. #>
[CmdletBinding()]
param()
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'Build Windows releases on Windows.' }
$SourceRoot = Split-Path $PSScriptRoot -Parent
$savedEncoded = $env:CARGO_ENCODED_RUSTFLAGS
$savedFlags = $env:RUSTFLAGS
Push-Location $SourceRoot
try {
    # Cargo gives CARGO_ENCODED_RUSTFLAGS precedence over RUSTFLAGS. Preserve
    # callers' flags and restore them even after a failed build.
    if (-not [string]::IsNullOrEmpty($savedEncoded)) {
        $separator = [char]31
        $env:CARGO_ENCODED_RUSTFLAGS = $savedEncoded + $separator + '-C' + $separator + 'target-feature=+crt-static'
    } else {
        $env:RUSTFLAGS = ($savedFlags + ' -C target-feature=+crt-static').Trim()
    }
    Get-Command cargo -ErrorAction Stop | Out-Null
    $savedPreference = $ErrorActionPreference
    try {
        # Native stderr must remain log output when this script is captured
        # with *> in PowerShell 5.1; Cargo's exit code determines success.
        $ErrorActionPreference = 'Continue'
        & cargo build --locked --release --target x86_64-pc-windows-msvc --target-dir (Join-Path $SourceRoot 'target') -p ia2-desktop -p server -p ia2-cli -p ia2-runtime -p lsp-launcher
        $nativeExit = $LASTEXITCODE
    } finally { $ErrorActionPreference = $savedPreference }
    if ($nativeExit -ne 0) { throw "Windows release build failed with exit code $nativeExit" }
} finally {
    $env:CARGO_ENCODED_RUSTFLAGS = $savedEncoded
    $env:RUSTFLAGS = $savedFlags
    Pop-Location
}
