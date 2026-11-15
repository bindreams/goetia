# M6: the CTRL_CLOSE_EVENT window of a console action under Task Scheduler; Stop per principal.
#   -Kind X -Grid:$false  : Stop of a console action whose handler returns TRUE at once (does an administrator's Stop reach X?)
#   -Kind X -Grid:$true   : the timing grid (handler blocking 2, 6, 15, 35 s; ends the process; never returns while the run
#                           thread ends the process with a code; the shim's child on the same console)
# Throwaway Windows CI runner only. The handler's blocking time is the independent variable, not a synchronisation.
param(
    [Parameter(Mandatory)][ValidateSet('system', 'localservice', 'networkservice', 'user')][string]$Kind,
    [switch]$Grid
)
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$user = $null
if ($Kind -eq 'user') { $user = New-ProbeUser 'gprobem6' }
$P = Get-Principal $Kind $user
Write-Host "principal: $Kind ($($P.UserId))"
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc
$n = 0

function Probe-Stop([string]$Spec, [string]$Child = '') {
    $script:n++
    $name = "ctrl$script:n"
    $log = "$Logs\m6-$name.log"; Remove-Item $log -ErrorAction SilentlyContinue
    $extra = if ($Child) { " child=$Child" } else { '' }
    $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act ctrl $log $Spec$extra"
    $t = Register-ProbeTask $folder $name $xml $P
    Write-Host ""
    Write-Host "--- spec=$Spec child=$(if ($Child) { $Child } else { 'none' }) as $Kind"
    $rt = $t.Run($null)
    Wait-Match $log ' ready$'
    $pid_ = [int]$rt.EnginePID
    $p = [Diagnostics.Process]::GetProcessById($pid_); $null = $p.Handle
    $childPid = $null
    $cl = Get-Content $log | Where-Object { $_ -match 'child-spawned' }
    if ($cl) { $childPid = [int](($cl -split ' ' | Where-Object { $_ -like 'pid=*' } | Select-Object -Last 1) -replace 'pid=', '') }
    # the child's own pid is in the child-spawned line as `pid=<child>` after the parent's own `pid=`
    $cp = $null
    if ($childPid) { try { $cp = [Diagnostics.Process]::GetProcessById($childPid); $null = $cp.Handle } catch { Write-Host "  child pid $childPid not openable: $($_.Exception.Message)" } }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $stopErr = $null
    try { $rt.Stop() } catch { $stopErr = "$($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" }
    $tStop = $sw.Elapsed.TotalMilliseconds
    "  Stop() returned after {0:N0} ms: {1}" -f $tStop, $(if ($stopErr) { "ERROR $stopErr" } else { 'ok' })
    if ($stopErr) {
        # The action is unreached: end it ourselves so the leg can go on.
        $null = $p.Kill()
    }
    $p.WaitForExit()
    $tExit = $sw.Elapsed.TotalMilliseconds
    "  the action process exited {0:N0} ms after Stop was called; exit code {1} ({2})" -f $tExit, $p.ExitCode, (HexOf $p.ExitCode)
    $tInst = $null
    Wait-NoInstances $t
    $tDrop = $sw.Elapsed.TotalMilliseconds
    "  the listing dropped {0:N0} ms after Stop; LastTaskResult={1} ({2})" -f $tDrop, $t.LastTaskResult, (HexOf $t.LastTaskResult)
    if ($cp) {
        $cp.WaitForExit()
        "  the shim-style child (pid $childPid) exited {0:N0} ms after Stop; exit code {1} ({2}); 77 would mean it ended itself after its own 60 s bound" -f $sw.Elapsed.TotalMilliseconds, $cp.ExitCode, (HexOf $cp.ExitCode)
    }
    "  the action's own log (clock = FILETIME ticks since the first line, in ms):"
    $first = $null
    foreach ($l in Get-Content $log) {
        $tm = [regex]::Match($l, ' t=(\d+)')
        if ($tm.Success) { $v = [int64]$tm.Groups[1].Value; if ($null -eq $first) { $first = $v }; "    +{0,9:N1} ms  {1}" -f (($v - $first) / 10000.0), ($l -replace '^pid=\d+ ct=\d+ t=\d+ ', '') }
        else { "    $l" }
    }
    $folder.DeleteTask($name, 0)
}

if (-not $Grid) {
    Section "Stop of a console action as $Kind (handler returns TRUE at once)"
    Probe-Stop 'block:0'
}
else {
    Section "the timing grid as $Kind"
    foreach ($s in 0, 2, 6, 15, 35) { Probe-Stop "block:$s" }
    Probe-Stop 'exit:2:3'
    Probe-Stop 'exit:35:3'
    foreach ($how in 'exitprocess', 'terminate', 'stdexit') { Probe-Stop "never:${how}:4660" }
    foreach ($c in 'plain', 'job', 'jobkill') { Probe-Stop 'block:0' $c }
    Probe-Stop 'block:6' 'jobkill'
    Probe-Stop 'never:exitprocess:4660' 'job'
}

Remove-ProbeTasks $svc
Remove-ProbeUser $user
