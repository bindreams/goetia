# Question 3: does arming a trigger just after a minute boundary start that minute's run? Arming is what goetia's `enable` does: the task
# is re-registered (`RegisterTask(TASK_UPDATE)`) with its triggers' <Enabled> true. Per cycle, one minute boundary B and, for each offset in
# $Offsets, two tasks registered with their triggers disabled and armed by an update whose call starts at about B + offset:
#   rep   a daily trigger from 00:00 repeating every PT1M for PT23H59M (a repetition in progress, ticks at every minute boundary)
#   repf  as rep, but its StartBoundary is B - 1 min (a repetition not yet started when registered, a start in the future of the registration)
#   lone  a lone daily trigger at B's hour and minute (its time has just passed)
# A control task per cycle (`repf-never`) is registered like repf and never armed. B is chosen after an estimate of how long creating the tasks takes,
# so that repf's start is still ahead when the last task is created.
# `StartWhenAvailable` true as goetia writes it. The tasks are read at B + 55 s (a normal tick of `rep` is at about B + 61.5 s) and deleted.
# A run in the window is made-up. Offset 30 s is the control. Output per task: the update call's start and end offsets from B, the
# runs' offsets from B. Throwaway Windows CI runner only.
param([int]$Cycles = 24)
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

$offsets = 0.0, 0.2, 0.4, 0.6, 0.8, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 30.0
function RepTrig([bool]$On) { "<CalendarTrigger><StartBoundary>2000-01-01T00:00:00</StartBoundary><Enabled>$("$On".ToLower())</Enabled><Repetition><Interval>PT1M</Interval><Duration>PT23H59M</Duration></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }
function RepFTrig([DateTime]$B, [bool]$On) { "<CalendarTrigger><StartBoundary>2000-01-01T$($B.AddMinutes(-1).ToString('HH:mm')):00</StartBoundary><Enabled>$("$On".ToLower())</Enabled><Repetition><Interval>PT1M</Interval><Duration>PT23H59M</Duration></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }
function LoneTrig([DateTime]$B, [bool]$On) { "<CalendarTrigger><StartBoundary>2000-01-01T$($B.ToString('HH:mm')):00</StartBoundary><Enabled>$("$On".ToLower())</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }

$all = @()
$est = 150.0   # seconds to create the tasks of a cycle; re-estimated from the last cycle
foreach ($cycle in 1..$Cycles) {
    $began = [DateTime]::UtcNow
    $now = $began
    $b = $now.Date.AddMinutes([Math]::Ceiling(($now.AddSeconds($est + 70)).TimeOfDay.TotalMinutes))
    $tasks = @()
    $names = @()
    foreach ($o in $offsets) {
        foreach ($shape in 'rep', 'repf', 'lone') {
            $name = "a$cycle-$shape-$([int]($o * 100))"
            $log = "$Logs\$name.log"; Remove-Item $log -ErrorAction SilentlyContinue
            $off = switch ($shape) { 'rep' { RepTrig $false } 'repf' { RepFTrig $b $false } 'lone' { LoneTrig $b $false } }
            $on = switch ($shape) { 'rep' { RepTrig $true } 'repf' { RepFTrig $b $true } 'lone' { LoneTrig $b $true } }
            $mk = { param($trig) New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $log 0" -Triggers $trig -StartWhenAvailable $true }
            $null = Register-ProbeTask $folder $name (& $mk $off) $P
            $target = $o + @{ rep = 0.0; repf = 0.03; lone = 0.06 }[$shape]
            $tasks += [pscustomobject]@{ Cycle = $cycle; Name = $name; Shape = $shape; Offset = $o; Target = $target; Log = $log; Xml = (& $mk $on); B = $b; Start = $null; End = $null; Err = $null }
        }
    }
    $nlog = "$Logs\a$cycle-never.log"; Remove-Item $nlog -ErrorAction SilentlyContinue
    $nx = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $nlog 0" -Triggers (RepFTrig $b $false) -StartWhenAvailable $true
    $null = Register-ProbeTask $folder "a$cycle-repf-never" $nx $P
    $createdAt = [DateTime]::UtcNow
    $est = ($createdAt - $began).TotalSeconds * 1.3 + 10
    "cycle ${cycle}: B=$($b.ToString('HH:mm:ss')); created $($tasks.Count + 1) tasks in $([Math]::Round(($createdAt - $began).TotalSeconds, 1)) s, $([Math]::Round(($b - $createdAt).TotalSeconds, 1)) s before B"
    if (($b - $createdAt).TotalSeconds -lt 66) { "cycle ${cycle}: SKIPPED, creation ended too close to B (repf's start would have passed)"; Remove-ProbeTasks $svc; $folder = Get-ProbeFolder $svc; continue }
    foreach ($t in ($tasks | Sort-Object Target)) {
        $at = $b.AddSeconds($t.Target)
        while ([DateTime]::UtcNow -lt $at) { }
        $t.Start = [DateTime]::UtcNow
        try { $null = Register-ProbeTask $folder $t.Name $t.Xml $P 4 } catch { $t.Err = $_.Exception.Message }
        $t.End = [DateTime]::UtcNow
    }
    $readAt = $b.AddSeconds(55)
    while ([DateTime]::UtcNow -lt $readAt) { Start-Sleep -Milliseconds 200 }
    foreach ($t in $tasks) {
        $runs = @()
        foreach ($l in @(Get-Content $t.Log -ErrorAction SilentlyContinue)) {
            if ($l -match ' t=(\d+)') { $runs += [DateTime]::FromFileTimeUtc([long]$Matches[1]) }
        }
        $t | Add-Member -NotePropertyName Runs -NotePropertyValue $runs
        $ro = ($runs | ForEach-Object { '{0:N3}' -f ($_ - $t.B).TotalSeconds }) -join ' '
        "{0}: B={1:HH:mm:ss} update call {2:N3}..{3:N3}s runs at [{4}] {5}" -f $t.Name, $t.B, ($t.Start - $t.B).TotalSeconds, ($t.End - $t.B).TotalSeconds, $ro, $(if ($t.Err) { "ERR $($t.Err)" } else { '' })
    }
    $nruns = @(Get-Content $nlog -ErrorAction SilentlyContinue | ForEach-Object { if ($_ -match ' t=(\d+)') { '{0:N3}' -f ([DateTime]::FromFileTimeUtc([long]$Matches[1]) - $b).TotalSeconds } })
    "a$cycle-repf-never (never armed): runs at [$($nruns -join ' ')]"
    $all += $tasks
    Remove-ProbeTasks $svc
    $folder = Get-ProbeFolder $svc
}

Section "summary: update call start offset from B (bins of 0.25 s; 30 s is the control) -> tasks with a run before B+55 s / tasks"
foreach ($shape in 'rep', 'repf', 'lone') {
    "-- $shape"
    $bins = @{}
    foreach ($t in $all) {
        if ($t.Shape -ne $shape -or $t.Err) { continue }
        $s = ($t.Start - $t.B).TotalSeconds
        $bin = if ($s -ge 25) { 'control' } else { '{0:N2}' -f ([Math]::Floor($s / 0.25) * 0.25) }
        if (-not $bins.ContainsKey($bin)) { $bins[$bin] = @{ N = 0; Ran = 0; Offsets = @() } }
        $bins[$bin].N++
        if ($t.Runs.Count -gt 0) { $bins[$bin].Ran++; $bins[$bin].Offsets += ($t.Runs[0] - $t.End).TotalSeconds }
    }
    foreach ($k in ($bins.Keys | Sort-Object)) {
        $o = $bins[$k].Offsets
        $lag = if ($o.Count) { 'first run after the call returned: ' + (($o | ForEach-Object { '{0:N2}' -f $_ }) -join ' ') } else { '' }
        "{0,8}: {1}/{2} {3}" -f $k, $bins[$k].Ran, $bins[$k].N, $lag
    }
}
exit 0
