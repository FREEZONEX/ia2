#requires -Version 5.1
<# Native Windows gate. AdaptationOnly is an offline installer/discovery contract check. #>
[CmdletBinding()]
param([switch]$AdaptationOnly)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'Run the native gate in Windows PowerShell.' }
$SourceRoot = Split-Path $PSScriptRoot -Parent
function Assert([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
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
Push-Location $SourceRoot
$scratch = Join-Path ([IO.Path]::GetTempPath()) "IA2 adaptation with spaces $([guid]::NewGuid().ToString('N'))"
try {
    $skill = Join-Path $SourceRoot '.claude\skills\industrial-automation-skill'
    $mirror = Join-Path $SourceRoot '.agents\skills\industrial-automation-skill'
    Assert ((Get-Item -LiteralPath 'AGENTS.md').Length -le 32768) 'AGENTS.md exceeds the default Codex 32 KiB budget.'
    Assert (Test-Path -LiteralPath (Join-Path $skill 'SKILL.md')) 'Canonical SKILL.md is missing.'
    Assert (Test-Path -LiteralPath $mirror) 'Repository skill discovery entry is missing.'
    # Git can materialize a symlink as a text file without Developer Mode.
    if (Test-Path -LiteralPath $mirror -PathType Leaf) {
        Assert ((Get-Content -LiteralPath $mirror -Raw).Trim() -eq '../../.claude/skills/industrial-automation-skill') 'Invalid Git symlink placeholder.'
    } else {
        Assert ((Get-FileHash -LiteralPath (Join-Path $mirror 'SKILL.md')).Hash -eq (Get-FileHash -LiteralPath (Join-Path $skill 'SKILL.md')).Hash) 'Repository skill mirror differs from its canonical source.'
    }
    $metadata = Get-Content -LiteralPath (Join-Path $skill 'SKILL.md') -Raw
    Assert ($metadata -match '(?m)^name: industrial-automation-skill\r?$') 'Invalid skill name.'
    Assert ($metadata -match '(?m)^description: .+') 'Skill description is missing.'
    Assert ((Get-Content -LiteralPath (Join-Path $skill 'agents\openai.yaml') -Raw).Contains('default_prompt: "Use $industrial-automation-skill')) 'Codex default prompt must invoke the skill.'
    Assert (Test-Path -LiteralPath (Join-Path $skill 'checklists\offline-readiness.md')) 'Offline handoff checklist is missing.'
    foreach ($script in @('install-skill.ps1', 'package-windows.ps1', 'build-windows.ps1', 'check-windows.ps1', 'test-windows-install.ps1', 'test-windows-runtime.ps1', 'test-windows-fieldbus.ps1')) {
        $tokens = $null
        $errors = $null
        [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot $script), [ref]$tokens, [ref]$errors) | Out-Null
        Assert ($errors.Count -eq 0) "PowerShell syntax errors in ${script}: $errors"
    }
    $claude = Join-Path $scratch 'claude'
    $agents = Join-Path $scratch 'agents'
    # Run twice: upgrade is safe and needs neither Cargo nor network nor symlinks.
    for ($i = 0; $i -lt 2; $i++) {
        & (Join-Path $PSScriptRoot 'install-skill.ps1') -SkillOnly -ClaudeDir $claude -AgentsDir $agents -NoShortcuts
    }
    foreach ($root in @($claude, $agents)) {
        foreach ($source in @(Get-ChildItem -LiteralPath $skill -File -Recurse -Force)) {
            $relative = $source.FullName.Substring($skill.Length).TrimStart('\')
            $installed = Join-Path $root "skills\industrial-automation-skill\$relative"
            Assert ((Get-FileHash -LiteralPath $source.FullName).Hash -eq (Get-FileHash -LiteralPath $installed).Hash) "Installed skill differs: $relative"
        }
    }
    $conflict = Join-Path $scratch 'conflict'
    New-Item -ItemType Directory -Path (Join-Path $conflict 'agents\skills\industrial-automation-skill') -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $conflict 'agents\skills\industrial-automation-skill\keep.txt') -Value 'preserve'
    $refused = $false
    try { & (Join-Path $PSScriptRoot 'install-skill.ps1') -SkillOnly -ClaudeDir (Join-Path $conflict 'claude') -AgentsDir (Join-Path $conflict 'agents') -NoShortcuts }
    catch { $refused = $true }
    Assert $refused 'Installer accepted an unmanaged skill destination.'
    Assert (-not (Test-Path -LiteralPath (Join-Path $conflict 'claude'))) 'Installer changed files before refusing a conflict.'
    $refused = $false
    try { & (Join-Path $PSScriptRoot 'install-skill.ps1') -SkillOnly -ClaudeDir (Join-Path $SourceRoot '.claude') -AgentsDir (Join-Path $scratch 'alias') -NoShortcuts }
    catch { $refused = $true }
    Assert $refused 'Installer accepted its source as the destination.'
    Assert (Test-Path -LiteralPath (Join-Path $skill 'SKILL.md')) 'Installer damaged the canonical skill.'
    $recursiveDestination = Join-Path $skill 'recursive-install-test'
    $refused = $false
    try { & (Join-Path $PSScriptRoot 'install-skill.ps1') -SkillOnly -ClaudeDir $recursiveDestination -AgentsDir (Join-Path $scratch 'recursive') -NoShortcuts }
    catch { $refused = $true }
    Assert $refused 'Installer accepted a destination inside its own copy source.'
    Assert (-not (Test-Path -LiteralPath $recursiveDestination)) 'Installer mutated its source before refusing recursive copy.'
    $nestedClaude = Join-Path $scratch 'nested\claude'
    $nestedAgents = Join-Path $nestedClaude 'skills\industrial-automation-skill\agents'
    $refused = $false
    try { & (Join-Path $PSScriptRoot 'install-skill.ps1') -SkillOnly -ClaudeDir $nestedClaude -AgentsDir $nestedAgents -NoShortcuts }
    catch { $refused = $true }
    Assert $refused 'Installer accepted overlapping skill destinations.'
    Assert (-not (Test-Path -LiteralPath $nestedClaude)) 'Installer changed nested destinations before refusing them.'
    Write-Host 'Windows agent adaptation checks passed (including paths with spaces and refusal guards).'
    if (-not $AdaptationOnly) {
        # Explicit native server build prevents executable-discovery tests from skipping.
        Invoke-Native cargo @('fmt', '--all', '--check')
        Invoke-Native cargo @('clippy', '--locked', '--workspace', '--', '-D', 'warnings')
        Invoke-Native cargo @('build', '--locked', '-p', 'ia2-desktop', '-p', 'server', '-p', 'ia2-cli', '-p', 'lsp-launcher')
        Invoke-Native cargo @('test', '--locked', '--workspace')
        & (Join-Path $PSScriptRoot 'build-windows.ps1')
        & (Join-Path $PSScriptRoot 'test-windows-runtime.ps1') -RuntimePath (Join-Path $SourceRoot 'target\x86_64-pc-windows-msvc\release\ia2-runtime.exe')
        & (Join-Path $PSScriptRoot 'test-windows-fieldbus.ps1') -RuntimePath (Join-Path $SourceRoot 'target\x86_64-pc-windows-msvc\release\ia2-runtime.exe')
        Invoke-Native pnpm @('--filter', '@cs/web', 'build')
        Invoke-Native pnpm @('--filter', '@cs/web', 'test')
        Write-Host 'Native Windows quality gate passed.'
    }
} finally {
    if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force }
    Pop-Location
}
