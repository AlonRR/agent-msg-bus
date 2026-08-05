<#
.SYNOPSIS
  Install agent-msg-bus on a Windows machine: binary, relay service, SessionStart hook.

.DESCRIPTION
  The relay is registered as a Scheduled Task at logon rather than being started by a session.
  A relay started by a session dies with that session and is invisible when it fails; the standing
  rule is that anything which must survive belongs in a real timer on the host, not in a session.

  Run from the repo root after `cargo build --release`. Needs no elevation: the task is registered
  for the current user only.

.NOTES
  Idempotent - re-running updates the binary and re-registers the task.
#>
[CmdletBinding()]
param(
    [string]$InstallDir = "$env:LOCALAPPDATA\agent-msg-bus",
    [string]$Listen = '127.0.0.1:9451'
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$built = Join-Path $repoRoot 'target\release\agent-msg-bus.exe'
if (-not (Test-Path $built)) { throw "Not built. Run: cargo build --release" }

$cfg = Join-Path $env:USERPROFILE '.agent-msg-bus\config.json'
if (-not (Test-Path $cfg)) {
    throw "No config at $cfg. Create it first: {`"url`":`"http://<broker>:9450`",`"machine`":`"<name>`",`"token`":`"<token>`"}"
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$exe = Join-Path $InstallDir 'agent-msg-bus.exe'

# The running relay holds a lock on its own image, so it has to stop before the copy.
Get-CimInstance Win32_Process -Filter "Name='agent-msg-bus.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -like '*relay*' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
Start-Sleep -Milliseconds 500
Copy-Item $built $exe -Force
Write-Host "installed $exe"

# A console app launched by Task Scheduler shows a window; wscript with window style 0 suppresses it.
#
# The third argument MUST be True (wait). With False, wscript spawned the relay detached and exited 0
# in milliseconds, so Task Scheduler marked the task Completed and lost any handle on the relay -
# which made -RestartCount inert by construction, because restart-on-failure only fires when the
# *task* fails and this task always succeeded instantly. The relay could then die and nothing would
# bring it back, while `peers` kept working (that is HTTP straight to the broker) so the CLI looked
# healthy with inbound delivery dead. Found on machine-b, confirmed on machine-a: relay alive, task State=Ready.
#
# Waiting keeps the task Running for the relay's lifetime, and WScript.Quit propagates the exit code
# so a crash actually registers as a task failure.
$shim = Join-Path $InstallDir 'relay-hidden.vbs'
@"
Dim rc
rc = CreateObject("WScript.Shell").Run("""$exe"" relay --listen $Listen", 0, True)
WScript.Quit rc
"@ | Set-Content -Path $shim -Encoding ASCII
Write-Host "wrote $shim"

$taskName = 'agent-msg-bus relay'
Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
$action  = New-ScheduledTaskAction -Execute 'wscript.exe' -Argument "`"$shim`""

# Belt and braces. The blocking shim makes restart-on-failure work, but a clean exit is not a
# failure and would leave nothing running. A repetition trigger re-launches every 5 minutes
# regardless; the relay exits 0 immediately if the port is already bound, so a redundant run is a
# no-op rather than a restart loop.
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$repeat  = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) `
    -RepetitionInterval (New-TimeSpan -Minutes 5)
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger @($trigger, $repeat) -Settings $settings `
    -Description 'Holds the agent-msg-bus broker connection and re-serves it on loopback, because Monitor refuses to open a WebSocket to a private IP.' | Out-Null
Write-Host "registered scheduled task: $taskName (blocking shim + 5-minute self-heal)"

Start-ScheduledTask -TaskName $taskName
Start-Sleep -Seconds 3

try {
    $h = (Invoke-WebRequest -Uri "http://$Listen/health" -UseBasicParsing -TimeoutSec 5).Content
    Write-Host "relay healthy: $h"
} catch {
    Write-Warning "relay did not come up on $Listen - check: Get-ScheduledTaskInfo -TaskName '$taskName'"
}

Write-Host ''
Write-Host 'Still to do by hand: add the SessionStart hook to ~/.claude/settings.json:'
Write-Host '  "SessionStart": [{ "matcher": "startup|resume", "hooks": ['
Write-Host '    { "type": "command", "timeout": 15,'
Write-Host '      "command": "\"$LOCALAPPDATA/agent-msg-bus/agent-msg-bus.exe\" session-start" } ] }]'
Write-Host ''
Write-Host 'Note the POSIX $LOCALAPPDATA form with forward slashes, not %LOCALAPPDATA%.' -ForegroundColor Yellow
Write-Host 'Hook commands run through a POSIX shell, where the cmd-style form does not expand and'
Write-Host 'the hook fails silently - which looks exactly like a bus with no traffic.'
