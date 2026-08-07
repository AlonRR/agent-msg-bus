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

# EVERY process running this image holds a lock on it, not just the relay. This filter used to match
# only '*relay*', which was correct until `watch` shipped - a session's subscription runs the SAME
# binary, so it kept a lock the installer could not clear and the copy failed with "being used by
# another process". Stop them all, and name what was stopped: a killed `watch` ends that session's
# subscription, which it must be told about rather than left to discover.
$holders = @(Get-CimInstance Win32_Process -Filter "Name='agent-msg-bus.exe'" -ErrorAction SilentlyContinue)
foreach ($h in $holders) {
    $kind = if ($h.CommandLine -match '\bwatch\b') { 'watch (a session subscription)' }
            elseif ($h.CommandLine -match '\brelay\b') { 'relay' }
            else { 'agent-msg-bus' }
    Write-Host "  stopping $kind (pid $($h.ProcessId))"
    Stop-Process -Id $h.ProcessId -Force -ErrorAction SilentlyContinue
}
if ($holders | Where-Object { $_.CommandLine -match '\bwatch\b' }) {
    Write-Warning "A `watch` was stopped - any session using it must re-arm its Monitor subscription."
}
Start-Sleep -Milliseconds 800
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

# THE REPETITION TRIGGER IS THE SUPERVISOR. -RestartCount below does NOT recover a died relay.
#
# Measured on machine-b, twice, by actually killing the relay rather than reading the config: recovery
# took 184s both times, landing exactly on the repetition grid, while LastTaskResult stayed
# 267009 (SCHED_S_TASK_RUNNING) and never showed a failure code. Task Scheduler's restart-on-failure
# fires when a task ends *unexpectedly* - fails to start, or is terminated by the service. An action
# that exits non-zero is recorded in LastTaskResult but the task counts as completed normally, so no
# restart is scheduled. Propagating the exit code buys observability, not supervision.
#
# So the interval IS the worst-case inbound-delivery outage. At 1 minute that is ~60s worst case,
# ~30s mean. The only cost of the short interval is a wscript spawn per minute that exits
# immediately, because the relay returns 0 on AddrInUse when one is already running.
#
# -RestartCount/-RestartInterval are kept for the case they genuinely cover (the task failing to
# start at all) and are deliberately NOT relied on for crash recovery.
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$repeat  = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) `
    -RepetitionInterval (New-TimeSpan -Minutes 1)
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger @($trigger, $repeat) -Settings $settings `
    -Description 'Holds the agent-msg-bus broker connection and re-serves it on loopback, because Monitor refuses to open a WebSocket to a private IP. Recovery is driven by the 1-minute repetition trigger, NOT by restart-on-failure.' | Out-Null
Write-Host "registered scheduled task: $taskName (blocking shim + 1-minute repetition supervisor)"

Start-ScheduledTask -TaskName $taskName

# Poll rather than sleep-then-check-once. A single request after a fixed 3s produced a false
# "relay did not come up" on machine-b, where wscript plus relay startup occasionally exceeds three
# seconds. That warning points at exactly the supervision area we spent a long time fixing, so a
# false one sends the next person chasing a bug that is not there - a check that lies is worse than
# no check.
$healthy = $null
$deadline = (Get-Date).AddSeconds(15)
while ((Get-Date) -lt $deadline) {
    try {
        $healthy = (Invoke-WebRequest -Uri "http://$Listen/health" -UseBasicParsing -TimeoutSec 2).Content
        break
    } catch {
        Start-Sleep -Seconds 1
    }
}
if ($healthy) {
    Write-Host "relay healthy: $healthy"
} else {
    Write-Warning "relay did not come up on $Listen within 15s. Messages will not arrive until it does."
    Write-Warning "Check: Get-ScheduledTaskInfo -TaskName '$taskName'"
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
