#requires -Version 5.1
<# Build or package prebuilt native Windows x64 artifacts; no install or user-data access. #>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$OutputPath
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'Create Windows packages on Windows.' }
$SourceRoot = Split-Path $PSScriptRoot -Parent
if (-not $OutputPath) { $OutputPath = Join-Path $SourceRoot 'dist\ia2-windows-x64.zip' }
$OutputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
function Invoke-Native([string]$Program, [string[]]$Arguments) {
    Get-Command -Name $Program -ErrorAction Stop | Out-Null
    $savedPreference = $ErrorActionPreference
    try {
        # PowerShell 5.1 turns captured native stderr into error records;
        # retain that output, but use the actual process exit code.
        $ErrorActionPreference = 'Continue'
        & $Program @Arguments
        $nativeExit = $LASTEXITCODE
    } finally { $ErrorActionPreference = $savedPreference }
    if ($nativeExit -ne 0) { throw "$Program failed with exit code $nativeExit" }
}
function Convert-RvaToOffset([uint32]$Rva, [object[]]$Sections) {
    foreach ($section in $Sections) {
        if ($Rva -ge $section.VirtualAddress -and $Rva -lt ($section.VirtualAddress + [Math]::Max($section.VirtualSize, $section.RawSize))) {
            return [int64]($section.RawOffset + $Rva - $section.VirtualAddress)
        }
    }
    throw "PE import address is outside the section table: $Rva"
}
Push-Location $SourceRoot
$stage = Join-Path ([IO.Path]::GetTempPath()) "ia2-package-$([guid]::NewGuid().ToString('N'))"
try {
    if (-not $SkipBuild) {
        Invoke-Native git @('submodule', 'update', '--init', '--recursive')
        Invoke-Native pnpm @('install', '--frozen-lockfile')
        Invoke-Native cargo @('test', '--locked', '-p', 'server')
        & (Join-Path $PSScriptRoot 'build-windows.ps1')
        Invoke-Native pnpm @('--filter', '@cs/web', 'build')
    }
    New-Item -ItemType Directory -Force -Path (Join-Path $stage 'bin'), (Join-Path $stage 'scripts'), (Join-Path $stage '.claude\skills') | Out-Null
    $names = [ordered]@{ 'IA2.exe' = 'IA2.exe'; 'cs.exe' = 'cs.exe'; 'server.exe' = 'ia2-server.exe'; 'lsp-launcher.exe' = 'lsp-launcher.exe'; 'ia2-runtime.exe' = 'ia2-runtime.exe' }
    foreach ($name in $names.Keys) {
        $source = Join-Path $SourceRoot "target\x86_64-pc-windows-msvc\release\$name"
        # Read the PE machine field: packages named x64 must contain x64 binaries.
        $stream = [IO.File]::OpenRead($source)
        $reader = New-Object IO.BinaryReader($stream)
        try {
            if ($reader.ReadUInt16() -ne 0x5a4d) { throw "Not a Windows executable: $source" }
            $stream.Position = 0x3c
            $offset = $reader.ReadUInt32()
            $stream.Position = $offset
            if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) { throw "Not a native x64 executable: $source" }
            $sectionCount = $reader.ReadUInt16()
            $stream.Position = $offset + 20
            $optionalSize = $reader.ReadUInt16()
            $optionalStart = $offset + 24
            $stream.Position = $optionalStart
            if ($reader.ReadUInt16() -ne 0x20b) { throw "Not PE32+: $source" }
            $stream.Position = $optionalStart + 120
            $importRva = $reader.ReadUInt32()
            $sections = @()
            for ($sectionIndex = 0; $sectionIndex -lt $sectionCount; $sectionIndex++) {
                $stream.Position = $optionalStart + $optionalSize + $sectionIndex * 40 + 8
                $sections += @{
                    VirtualSize = $reader.ReadUInt32()
                    VirtualAddress = $reader.ReadUInt32()
                    RawSize = $reader.ReadUInt32()
                    RawOffset = $reader.ReadUInt32()
                }
            }
            if ($importRva -ne 0) {
                $importOffset = Convert-RvaToOffset $importRva $sections
                for ($entryIndex = 0; ; $entryIndex++) {
                    $stream.Position = $importOffset + $entryIndex * 20
                    $lookup = $reader.ReadUInt32()
                    $stamp = $reader.ReadUInt32()
                    $forward = $reader.ReadUInt32()
                    $nameRva = $reader.ReadUInt32()
                    $address = $reader.ReadUInt32()
                    if (($lookup -bor $stamp -bor $forward -bor $nameRva -bor $address) -eq 0) { break }
                    $stream.Position = Convert-RvaToOffset $nameRva $sections
                    $dll = New-Object Text.StringBuilder
                    do {
                        $byte = $reader.ReadByte()
                        if ($byte -ne 0) { [void]$dll.Append([char]$byte) }
                        if ($dll.Length -gt 512) { throw "Malformed DLL import in $source" }
                    } while ($byte -ne 0)
                    if ($dll.ToString() -match '^(?i:vcruntime|msvcp|ucrtbase|api-ms-win-crt-)') {
                        throw "$source imports $dll. Rebuild with scripts/build-windows.ps1 for the static-CRT package."
                    }
                    if ($dll.ToString() -match '^(?i:wpcap|packet)\.dll$') {
                        throw "$source imports $dll at process startup. Npcap must load only when real EtherCAT is requested; this package must also run without Npcap."
                    }
                }
            }
        } finally { $reader.Dispose() }
        Copy-Item -LiteralPath $source -Destination (Join-Path $stage "bin\$($names[$name])")
    }
    foreach ($entry in @('index.html', 'hmi.html')) {
        if (-not (Test-Path -LiteralPath (Join-Path $SourceRoot "apps\web\dist\$entry"))) { throw "Web build missing $entry" }
    }
    Copy-Item -LiteralPath (Join-Path $SourceRoot 'apps\web\dist') -Destination (Join-Path $stage 'web') -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $SourceRoot 'library') -Destination (Join-Path $stage 'library') -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $SourceRoot '.claude\skills\industrial-automation-skill') -Destination (Join-Path $stage '.claude\skills\industrial-automation-skill') -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install-skill.ps1') -Destination (Join-Path $stage 'scripts\install-skill.ps1')
    Copy-Item -LiteralPath (Join-Path $SourceRoot 'docs\windows.md') -Destination (Join-Path $stage 'WINDOWS.md')
    foreach ($document in @('windows-validation.md', 'windows-can-ethercat.md', 'edge-deploy.md')) {
        Copy-Item -LiteralPath (Join-Path $SourceRoot "docs\$document") -Destination (Join-Path $stage $document)
    }
    foreach ($notice in @('LICENSE', 'NOTICE')) {
        if (Test-Path -LiteralPath (Join-Path $SourceRoot $notice)) { Copy-Item -LiteralPath (Join-Path $SourceRoot $notice) -Destination $stage }
    }
    $commit = (& git rev-parse HEAD | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'Cannot determine source revision.' }
    $dirty = (@(& git status --porcelain).Count -gt 0)
    if ($LASTEXITCODE -ne 0) { throw 'Cannot determine source status.' }
    [ordered]@{ format = 1; target = 'x86_64-pc-windows-msvc'; commit = $commit; dirty = $dirty; created_utc = [DateTime]::UtcNow.ToString('o') } |
        ConvertTo-Json | Set-Content -LiteralPath (Join-Path $stage 'windows-package.json') -Encoding UTF8
    $instructions = @'
IA2 Windows x64
Open Windows PowerShell in this extracted folder and run:
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-skill.ps1
No Rust, Node, pnpm, administrator rights, or source checkout are required.
Open IA2 IDE from the desktop or Start menu. IA2 Terminal remains available for CLI work.
The native app requires Microsoft Edge WebView2 Runtime. If the installer reports it missing,
install Evergreen Runtime from https://developer.microsoft.com/microsoft-edge/webview2/ and retry.
Read WINDOWS.md for scope and operation. The app, server and web assets must stay together.
'@
    Set-Content -LiteralPath (Join-Path $stage 'INSTALL.txt') -Value $instructions -Encoding UTF8
    # Compress-Archive skips hidden directories, including the bundled .claude skill.
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    New-Item -ItemType Directory -Force -Path (Split-Path $OutputPath -Parent) | Out-Null
    $temporaryZip = "$OutputPath.tmp-$([guid]::NewGuid().ToString('N'))"
    try {
        [IO.Compression.ZipFile]::CreateFromDirectory($stage, $temporaryZip, [IO.Compression.CompressionLevel]::Optimal, $false)
        Move-Item -LiteralPath $temporaryZip -Destination $OutputPath -Force
    } finally { if (Test-Path -LiteralPath $temporaryZip) { Remove-Item -LiteralPath $temporaryZip -Force } }
    (Get-FileHash -LiteralPath $OutputPath -Algorithm SHA256).Hash + '  ' + [IO.Path]::GetFileName($OutputPath) |
        Set-Content -LiteralPath "$OutputPath.sha256" -Encoding ASCII
    Write-Host "Package: $OutputPath"
    Write-Host "SHA256: $OutputPath.sha256"
} finally {
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    Pop-Location
}
