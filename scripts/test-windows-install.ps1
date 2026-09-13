#requires -Version 5.1
<# Verify a real release ZIP in isolated directories, with no development tools on PATH.
No hardware, default installation or user project mutation. Retains artifacts for review.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$PackagePath,
    [string]$ArtifactsDirectory
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'Installed-artifact acceptance requires native Windows.' }
$PackagePath = (Resolve-Path -LiteralPath $PackagePath).Path
if (-not $ArtifactsDirectory) {
    $ArtifactsDirectory = Join-Path (Split-Path $PSScriptRoot -Parent) ('target\windows-install-smoke\' + [guid]::NewGuid().ToString('N'))
}
$ArtifactsDirectory = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ArtifactsDirectory)
if (Test-Path -LiteralPath $ArtifactsDirectory) { throw "Use a new artifact directory: $ArtifactsDirectory" }
New-Item -ItemType Directory -Path $ArtifactsDirectory | Out-Null
# ASCII source works in Windows PowerShell 5.1 while these paths contain Chinese.
$unicode = [string][char]0x5b89 + [string][char]0x88c5
$extracted = Join-Path $ArtifactsDirectory ($unicode + ' extracted package')
$install = Join-Path $ArtifactsDirectory ($unicode + ' installed app')
$claude = Join-Path $ArtifactsDirectory ($unicode + ' claude agent')
$agents = Join-Path $ArtifactsDirectory ($unicode + ' standard agent')
$working = Join-Path $ArtifactsDirectory 'unrelated working directory'
$data = Join-Path $ArtifactsDirectory 'user data'
New-Item -ItemType Directory -Path $working, $data | Out-Null
$sentinel = Join-Path $data 'preserve.txt'
Set-Content -LiteralPath $sentinel -Value 'User data must survive installation and upgrade.' -Encoding UTF8
$sentinelHash = (Get-FileHash -LiteralPath $sentinel).Hash
$savedPath = $env:PATH
$savedLibrary = $env:IA2_LIBRARY_DIR
$savedLsp = $env:LSP_LAUNCHER
$userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
$machinePath = [Environment]::GetEnvironmentVariable('PATH', 'Machine')
$server = $null
$socket = $null
$passed = $false
$baseUrl = $null
$manifest = $null
$result = [ordered]@{ result = 'failed'; artifacts = $ArtifactsDirectory; package = $PackagePath }

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
function Tree-Hashes([string[]]$Roots) {
    $hashes = @()
    for ($index = 0; $index -lt $Roots.Count; $index++) {
        $root = $Roots[$index]
        foreach ($file in @(Get-ChildItem -LiteralPath $root -File -Recurse -Force | Sort-Object FullName)) {
            $relative = $file.FullName.Substring($root.Length).TrimStart('\')
            $hashes += "$index/$relative $((Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash)"
        }
    }
    return $hashes
}
function Get-Response([string]$Path) {
    Invoke-WebRequest -UseBasicParsing -Uri ($baseUrl + $Path) -TimeoutSec 5
}
function Response-Json($Response) {
    [Text.Encoding]::UTF8.GetString($Response.RawContentStream.ToArray()) | ConvertFrom-Json
}
function Send-Lsp($Message) {
    $bytes = [Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject $Message -Depth 12 -Compress))
    $segment = New-Object 'ArraySegment[byte]' -ArgumentList (, $bytes)
    $cancel = New-Object Threading.CancellationTokenSource(15000)
    try { $socket.SendAsync($segment, [Net.WebSockets.WebSocketMessageType]::Text, $true, $cancel.Token).GetAwaiter().GetResult() }
    finally { $cancel.Dispose() }
}
function Receive-Lsp {
    $buffer = New-Object byte[] 8192
    $segment = New-Object 'ArraySegment[byte]' -ArgumentList (, $buffer)
    $stream = New-Object IO.MemoryStream
    $cancel = New-Object Threading.CancellationTokenSource(15000)
    try {
        do {
            $received = $socket.ReceiveAsync($segment, $cancel.Token).GetAwaiter().GetResult()
            if ($received.MessageType -eq [Net.WebSockets.WebSocketMessageType]::Close) { throw 'LSP closed before initialization completed.' }
            $stream.Write($buffer, 0, $received.Count)
        } while (-not $received.EndOfMessage)
        return ([Text.Encoding]::UTF8.GetString($stream.ToArray()) | ConvertFrom-Json)
    } finally { $cancel.Dispose(); $stream.Dispose() }
}

try {
    $hash = (Get-FileHash -LiteralPath $PackagePath -Algorithm SHA256).Hash
    $expected = ((Get-Content -LiteralPath ($PackagePath + '.sha256') -Raw).Trim() -split '\s+')[0]
    if ($hash -ne $expected) { throw 'Release ZIP SHA256 does not match its sidecar.' }
    $result.package_sha256 = $hash
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::ExtractToDirectory($PackagePath, $extracted)
    $required = @('windows-package.json', 'scripts\install-skill.ps1', 'bin\IA2.exe', 'bin\cs.exe', 'bin\ia2-server.exe', 'bin\ia2-runtime.exe', 'bin\lsp-launcher.exe', '.claude\skills\industrial-automation-skill\SKILL.md',
        '.claude\skills\industrial-automation-skill\references\02-cli-reference.md', 'web\index.html', 'web\hmi.html')
    foreach ($path in $required) {
        if (-not (Test-Path -LiteralPath (Join-Path $extracted $path))) { throw "ZIP omitted required artifact: $path" }
    }
    $manifest = Get-Content -LiteralPath (Join-Path $extracted 'windows-package.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($manifest.target -ne 'x86_64-pc-windows-msvc') { throw 'Not a native x64 release package.' }
    $result.source = $manifest
    # Keep only Windows system tools; neither compiler nor JS toolchain may be needed.
    $env:PATH = (Join-Path $env:SystemRoot 'System32') + ';' + $env:SystemRoot + ';' + (Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0')
    $env:IA2_LIBRARY_DIR = $null
    $env:LSP_LAUNCHER = $null
    $minimalPath = $env:PATH
    $result.process_path = $minimalPath
    $installer = Join-Path $extracted 'scripts\install-skill.ps1'
    $roots = @($install, (Join-Path $claude 'skills\industrial-automation-skill'), (Join-Path $agents 'skills\industrial-automation-skill'))
    & $installer -InstallRoot $install -ClaudeDir $claude -AgentsDir $agents -NoShortcuts *> (Join-Path $ArtifactsDirectory 'install.log')
    $first = @(Tree-Hashes $roots)
    & $installer -InstallRoot $install -ClaudeDir $claude -AgentsDir $agents -NoShortcuts *> (Join-Path $ArtifactsDirectory 'upgrade.log')
    $second = @(Tree-Hashes $roots)
    if (@(Compare-Object $first $second).Count -ne 0) { throw 'Identical release changed installed content during upgrade.' }
    $second | Set-Content -LiteralPath (Join-Path $ArtifactsDirectory 'installed-hashes.txt') -Encoding UTF8
    $noticeHashes = @()
    foreach ($notice in @('LICENSE', 'NOTICE', 'THIRD-PARTY-NOTICES')) {
        $noticeSource = Join-Path $extracted $notice
        if (-not (Test-Path -LiteralPath $noticeSource)) { continue }
        $noticeFiles = if (Test-Path -LiteralPath $noticeSource -PathType Container) {
            @(Get-ChildItem -LiteralPath $noticeSource -File -Recurse -Force)
        } else { @(Get-Item -LiteralPath $noticeSource -Force) }
        foreach ($file in $noticeFiles) {
            $relative = $file.FullName.Substring($extracted.Length).TrimStart('\')
            $installedNotice = Join-Path $install $relative
            if (-not (Test-Path -LiteralPath $installedNotice -PathType Leaf)) { throw "Installer omitted license or notice: $relative" }
            $sourceHash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
            if ((Get-FileHash -LiteralPath $installedNotice -Algorithm SHA256).Hash -ne $sourceHash) {
                throw "Installed license or notice differs from package: $relative"
            }
            $noticeHashes += "$relative $sourceHash"
        }
    }
    $noticeHashes | Sort-Object | Set-Content -LiteralPath (Join-Path $ArtifactsDirectory 'installed-notice-hashes.txt') -Encoding UTF8
    $result.installed_notice_files_verified = $noticeHashes.Count
    if ((Get-FileHash -LiteralPath $sentinel).Hash -ne $sentinelHash) { throw 'Installer changed user data.' }
    if ($env:PATH -ne $minimalPath -or [Environment]::GetEnvironmentVariable('PATH', 'User') -ne $userPath -or [Environment]::GetEnvironmentVariable('PATH', 'Machine') -ne $machinePath) {
        throw 'Installer changed process or persistent PATH.'
    }
    $result.installed_files = $second.Count
    $result.repeated_install_hashes_equal = $true
    $result.user_data_preserved = $true
    Push-Location $working
    try {
        $cs = Join-Path $install 'bin\cs.exe'
        $serverPath = Join-Path $install 'bin\ia2-server.exe'
        Invoke-Native $cs @('--version')
        Invoke-Native $serverPath @('--help')
        Invoke-Native (Join-Path $install 'bin\ia2-runtime.exe') @('--help')
        # Wait explicitly for a GUI executable; & is not a reliable exit-code
        # check in PowerShell. This mode starts no desktop window or server.
        $checkOutput = Join-Path $ArtifactsDirectory 'desktop-runtime.stdout.json'
        $checkError = Join-Path $ArtifactsDirectory 'desktop-runtime.stderr.log'
        $desktopCheck = Start-Process -FilePath (Join-Path $install 'bin\IA2.exe') -ArgumentList '--check-runtime' -WorkingDirectory $working -PassThru -Wait `
            -RedirectStandardOutput $checkOutput -RedirectStandardError $checkError
        try {
            if ($desktopCheck.ExitCode -ne 0) { throw "Installed desktop prerequisite check failed (exit $($desktopCheck.ExitCode)); inspect desktop-runtime.stderr.log." }
            $runtimeCheck = Get-Content -LiteralPath $checkOutput -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($runtimeCheck.ok -ne $true -or [string]::IsNullOrWhiteSpace($runtimeCheck.webview2_version)) { throw 'Installed desktop did not confirm its WebView2 Runtime.' }
            $result.desktop_runtime = $runtimeCheck
        } finally { $desktopCheck.Dispose() }
        $listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
        $listener.Start()
        $port = $listener.LocalEndpoint.Port
        $listener.Stop()
        $baseUrl = 'http://127.0.0.1:' + $port
        $arguments = '--bind 127.0.0.1:' + $port + ' --static-dir "' + (Join-Path $install 'web') + '" --demo-modbus-addr 127.0.0.1:0'
        # Do not pass --library-dir: this verifies executable-adjacent discovery.
        $server = Start-Process -FilePath $serverPath -ArgumentList $arguments -WorkingDirectory $working -PassThru -NoNewWindow `
            -RedirectStandardOutput (Join-Path $ArtifactsDirectory 'server.stdout.log') -RedirectStandardError (Join-Path $ArtifactsDirectory 'server.stderr.log')
        $null = $server.Handle
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        $healthy = $false
        while ([DateTime]::UtcNow -lt $deadline) {
            $server.Refresh()
            if ($server.HasExited) { throw 'Installed server exited during startup; inspect server.stderr.log.' }
            try { $health = Response-Json (Get-Response '/health'); $healthy = ($health.status -eq 'ok') } catch { $healthy = $false }
            if ($healthy) { break }
            Start-Sleep -Milliseconds 100
        }
        if (-not $healthy) { throw 'Installed server did not become healthy.' }
        Invoke-Native $cs @('--server', $baseUrl, 'api', 'GET', '/health')
        foreach ($page in @('/index.html', '/hmi.html')) {
            $response = Get-Response $page
            $html = [Text.Encoding]::UTF8.GetString($response.RawContentStream.ToArray())
            if ($response.StatusCode -ne 200 -or $html -notmatch '<div id="root"' -or $html -notmatch 'src="(/assets/[^\"]+\.js)"') {
                throw "Installed page is missing its application entry: $page"
            }
            $asset = $Matches[1]
            $assetResponse = Get-Response $asset
            if ($assetResponse.StatusCode -ne 200 -or $assetResponse.RawContentStream.Length -lt 1000) { throw "Page JS asset missing: $asset" }
        }
        $libraries = @(Response-Json (Get-Response '/api/library'))
        $processLibrary = @($libraries | Where-Object { $_.name -eq 'process-control' })
        if ($processLibrary.Count -ne 1 -or @($processLibrary[0].blocks).Count -eq 0) { throw 'Executable-adjacent installed library was not discovered.' }
        $libraries | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $ArtifactsDirectory 'libraries.json') -Encoding UTF8
        $result.library_blocks = @($processLibrary[0].blocks).Count
        $socket = New-Object Net.WebSockets.ClientWebSocket
        $cancel = New-Object Threading.CancellationTokenSource(15000)
        try { $socket.ConnectAsync([uri]('ws://127.0.0.1:' + $port + '/api/lsp'), $cancel.Token).GetAwaiter().GetResult() }
        finally { $cancel.Dispose() }
        Send-Lsp @{ jsonrpc = '2.0'; id = 1; method = 'initialize'; params = @{ processId = $null; rootUri = $null; capabilities = @{}; initializationOptions = @{ dialect = 'iec61131-3-ed2' } } }
        $initialized = $false
        for ($i = 0; $i -lt 10; $i++) {
            $message = Receive-Lsp
            if ($message.PSObject.Properties.Name -contains 'id' -and $message.id -eq 1) {
                if ($message.PSObject.Properties.Name -notcontains 'result' -or $message.result.PSObject.Properties.Name -notcontains 'capabilities') { throw 'Installed LSP rejected initialize.' }
                $message | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $ArtifactsDirectory 'lsp-initialize.json') -Encoding UTF8
                $initialized = $true
                break
            }
        }
        if (-not $initialized) { throw 'Installed LSP did not answer initialize.' }
        Send-Lsp @{ jsonrpc = '2.0'; method = 'initialized'; params = @{} }
        $result.lsp_initialize = 'passed through installed server and sidecar'
        $result.server_port = $port
        $result.working_directory = $working
    } finally { Pop-Location }
    $passed = $true
} catch {
    $result.error = $_.Exception.Message
    throw
} finally {
    if ($null -ne $socket) { $socket.Dispose() }
    # Give the live WS proxy time to reap its sidecar before terminating
    # the server; killing the parent first would create an artificial orphan.
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $leftovers = @(Get-Process lsp-launcher -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq (Join-Path $install 'bin\lsp-launcher.exe') })
        if ($leftovers.Count -eq 0) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($leftovers.Count -gt 0) {
        $passed = $false
        $result.stop_error = 'Installed LSP sidecar remained after socket termination.'
        $leftovers | Stop-Process -Force
    }
    if ($null -ne $server) {
        $server.Refresh()
        if (-not $server.HasExited) { $server.Kill() }
        if (-not $server.WaitForExit(15000)) { $passed = $false; $result.stop_error = 'Server process did not exit.' }
        else { $result.server_stop = 'test process terminated and exit confirmed' }
        $server.Dispose()
    }
    $env:PATH = $savedPath
    $env:IA2_LIBRARY_DIR = $savedLibrary
    $env:LSP_LAUNCHER = $savedLsp
    $result.result = if ($passed) { 'passed' } else { 'failed' }
    $result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $ArtifactsDirectory 'result.json') -Encoding UTF8
    $result | ConvertTo-Json -Depth 12
}
if (-not $passed) { throw 'Installed-artifact acceptance failed; inspect result.json.' }
