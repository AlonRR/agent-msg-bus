<#
.SYNOPSIS
  One-command bootstrap for a new Windows client machine (e.g. machine-b).

.DESCRIPTION
  Writes ~/.agent-msg-bus/config.json, installs the binary, registers the relay as a Scheduled Task,
  and adds the SessionStart hook to ~/.claude/settings.json.

  Run this ON the machine being onboarded. machine-b is not reachable over SSH from machine-a - `machine-b`
  resolves through wildcard DNS to the Caddy container, so a ping "succeeding" proves nothing.

.PARAMETER Token
  That machine's own token from the broker. Retrieve it on the homelab host with:

      ssh <broker-host> 'cat /etc/agent-msg-bus/tokens.json'

  and take the entry matching -Machine. Each machine has its own; do not share one.

.EXAMPLE
  .\bootstrap-client.ps1 -Machine machine-b -Token <that machine's token>

.NOTES
  Needs the release binary next to the repo (cargo build --release) or passed via -BinaryPath.
  No elevation required.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Machine,
    [Parameter(Mandatory)][string]$Token,
    [string]$BrokerUrl = 'http://<broker-ip>:9450',
    [string]$Listen = '127.0.0.1:9451',
    [string]$BinaryPath,
    [string]$InstallDir = "$env:LOCALAPPDATA\agent-msg-bus"
)

$ErrorActionPreference = 'Stop'

if (-not $BinaryPath) {
    $repoRoot = Split-Path -Parent $PSScriptRoot
    $BinaryPath = Join-Path $repoRoot 'target\release\agent-msg-bus.exe'
}
if (-not (Test-Path $BinaryPath)) { throw "No binary at $BinaryPath. Run: cargo build --release" }

# --- 1. config ---------------------------------------------------------------
$cfgDir = Join-Path $env:USERPROFILE '.agent-msg-bus'
New-Item -ItemType Directory -Force -Path $cfgDir | Out-Null
$cfgPath = Join-Path $cfgDir 'config.json'
[ordered]@{ url = $BrokerUrl; machine = $Machine; token = $Token; relay = $Listen } |
    ConvertTo-Json | Set-Content -Path $cfgPath -Encoding UTF8

# Owner-only. The token is in here.
icacls $cfgPath /inheritance:r /grant:r ("{0}:(R,W)" -f $env:USERNAME) | Out-Null
Write-Host "wrote $cfgPath (owner-only)"

# --- 2. binary + relay service ------------------------------------------------
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$exe = Join-Path $InstallDir 'agent-msg-bus.exe'
Get-CimInstance Win32_Process -Filter "Name='agent-msg-bus.exe'" -ErrorAction SilentlyContinue |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
Start-Sleep -Milliseconds 500
Copy-Item $BinaryPath $exe -Force

# Third arg MUST be True (wait). With False, wscript exits immediately and Task Scheduler loses the
# relay, making -RestartCount inert - the relay can die and nothing brings it back.
$shim = Join-Path $InstallDir 'relay-hidden.vbs'
@"
Dim rc
rc = CreateObject("WScript.Shell").Run("""$exe"" relay --listen $Listen", 0, True)
WScript.Quit rc
"@ | Set-Content -Path $shim -Encoding ASCII

$taskName = 'agent-msg-bus relay'
Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
$action  = New-ScheduledTaskAction -Execute 'wscript.exe' -Argument "`"$shim`""
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
# Self-heal every 5 minutes: a clean exit is not a failure, so restart-on-failure alone would leave
# nothing running. The relay exits 0 if the port is already bound, so a redundant run is a no-op.
$repeat  = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) `
    -RepetitionInterval (New-TimeSpan -Minutes 5)
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger @($trigger, $repeat) -Settings $settings `
    -Description 'Holds the agent-msg-bus broker connection and re-serves it on loopback.' | Out-Null
Start-ScheduledTask -TaskName $taskName
Start-Sleep -Seconds 3
Write-Host "installed $exe and registered scheduled task"

# --- 3. SessionStart hook -----------------------------------------------------
# The POSIX $VAR form with forward slashes is deliberate: hook commands run through a POSIX shell,
# where %LOCALAPPDATA% does not expand. That failure is silent and looks exactly like a bus with no
# traffic, so it is worth getting right the first time.
$settingsPath = Join-Path $env:USERPROFILE '.claude\settings.json'
if (Test-Path $settingsPath) {
    Copy-Item $settingsPath "$settingsPath.bak-agentbus-$(Get-Date -Format yyyyMMdd-HHmmss)"
    $json = Get-Content $settingsPath -Raw | ConvertFrom-Json
} else {
    New-Item -ItemType Directory -Force -Path (Split-Path $settingsPath) | Out-Null
    $json = [pscustomobject]@{}
}
$cmd = '"$LOCALAPPDATA/agent-msg-bus/agent-msg-bus.exe" session-start'
if (-not $json.PSObject.Properties['hooks']) {
    $json | Add-Member -NotePropertyName hooks -NotePropertyValue ([pscustomobject]@{})
}
if (-not $json.hooks.PSObject.Properties['SessionStart']) {
    $json.hooks | Add-Member -NotePropertyName SessionStart -NotePropertyValue @()
}
$already = @($json.hooks.SessionStart | ForEach-Object { $_.hooks } | Where-Object { $_.command -like '*agent-msg-bus*' }).Count -gt 0
if ($already) {
    Write-Host 'SessionStart hook already registered'
} else {
    $entry = [pscustomobject]@{
        matcher = 'startup|resume'
        hooks   = @([pscustomobject]@{ type = 'command'; timeout = 15; command = $cmd })
    }
    $json.hooks.SessionStart = @($json.hooks.SessionStart) + $entry
    $json | ConvertTo-Json -Depth 12 | Set-Content $settingsPath -Encoding UTF8
    Write-Host "registered SessionStart hook in $settingsPath"
}

# --- 4. verify ----------------------------------------------------------------
Write-Host ''
try {
    $h = (Invoke-WebRequest -Uri "http://$Listen/health" -UseBasicParsing -TimeoutSec 5).Content
    Write-Host "relay healthy: $h"
} catch {
    Write-Warning "relay is NOT up on $Listen. Messages will not arrive until it is."
    Write-Warning "Check: Get-ScheduledTaskInfo -TaskName '$taskName'"
}
& $exe peers
Write-Host ''
Write-Host 'Done. Start a new Claude Code session on this machine - the SessionStart hook will'
Write-Host 'print its bus address and the Monitor call to arm the subscription.'
