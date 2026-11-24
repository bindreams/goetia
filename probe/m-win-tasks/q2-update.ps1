# Question 2: does an install that changes a schedule with a repetition start a run? goetia's install of a changed task is
# `RegisterTask(TASK_UPDATE)` of the whole definition (src/backend/scm/manager/task/com.rs), so that is what runs here, with the
# triggers armed and `StartWhenAvailable` true as goetia writes them.
#   q2-update.ps1
# Many tasks are registered with a schedule S0 and, all at the same moment Tu (30 s past a minute boundary), updated to S1. Kinds:
#   rep-interval     S0 a repetition PT20M, S1 a repetition PT10M (both daily, StartBoundary 2000-01-01T<HH:MM>)
#   lone-to-rep      S0 a lone daily trigger, S1 a repetition PT10M
#   rep-start-moved  S0 a repetition PT10M that starts later, S1 the same repetition that starts L minutes before Tu
#   rep-duration     S0 a PT10M repetition of PT30M, S1 the same start and interval with PT3H
#   rep-identical    S0 = S1 (a repetition PT10M)
#   rep-to-lone      S0 a repetition, S1 a lone daily trigger L minutes before Tu
#   lone-moved       S0 a lone trigger before it, S1 a lone trigger L minutes before Tu (KB bc18321's case)
#   lone-identical   S0 = S1 (a lone trigger)
#   min-identical    S0 = S1, a repetition PT1M that has been ticking every minute (goetia's `* * * * *` shape)
#   min-interval     S0 a PT1M repetition ticking every minute, S1 PT2M from the hour's start
#   min-duration     S0 a PT1M repetition of PT30M that has run its duration, S1 the same with PT3H
#   min-to-lone      S0 a PT1M repetition ticking every minute, S1 a lone daily trigger L minutes before Tu
# S1's most recent tick (or its trigger time) is L minutes and 30 s before Tu, L in 1, 3, 8; three tasks per cell. Each action appends
# a line with its start time. Runs are read after the window and classified: `made-up` = within 10 s after the update call and not on S1's
# grid; `tick` = within 5 s after one of S1's grid ticks after Tu; other = `unexplained`. A lone S1 trigger has no tick until tomorrow.
# Three tasks per cell.
# Throwaway Windows CI runner only.
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

function Lone([string]$Start) { "<CalendarTrigger><StartBoundary>$Start</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }
function Rep([string]$Start, [string]$Every, [string]$For) { "<CalendarTrigger><StartBoundary>$Start</StartBoundary><Enabled>true</Enabled><Repetition><Interval>$Every</Interval><Duration>$For</Duration></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }
function Sb([DateTime]$d) { '2000-01-01T' + $d.ToString('HH:mm') + ':00' }

# Tu: 30 s past a minute boundary, 5 to 6 minutes ahead, at a minute of the hour (20 to 38) that keeps every start boundary in this hour.
# The wait is on the real clock, read again.
$tu = $null
while ($true) {
    $now = [DateTime]::UtcNow
    $c = $now.Date.AddMinutes([Math]::Ceiling($now.TimeOfDay.TotalMinutes) + 5).AddSeconds(30)
    if ($c.Minute -ge 20 -and $c.Minute -le 38) { $tu = $c; break }
    "waiting: Tu would be $($c.ToString('HH:mm:ss')), outside minutes 20-38"
    Start-Sleep -Seconds 20
}
$hh = $tu.Date.AddHours($tu.Hour)
$window = 24   # minutes watched after Tu
"Tu = $($tu.ToString('o')); watching until +$window min"

$kinds = 'rep-interval', 'lone-to-rep', 'rep-start-moved', 'rep-duration', 'rep-identical', 'rep-to-lone', 'lone-moved', 'lone-identical', 'min-identical', 'min-interval', 'min-duration', 'min-to-lone'
$tasks = @()
foreach ($k in $kinds) {
    foreach ($L in 1, 3, 8) {
        foreach ($rep in 1..3) {
            $name = "u-$k-L$L-r$rep"
            $tick = $hh.AddMinutes($tu.Minute - $L)       # S1's most recent tick or trigger time, L min 30 s before Tu
            $s1rep = Rep (Sb $tick) 'PT10M' 'PT3H'
            $s1lone = Lone (Sb $tick)
            $step = 10; $gridStart = $tick
            $hourStart = $hh
            switch ($k) {
                'rep-interval'    { $s0 = Rep (Sb $tick.AddMinutes(-3)) 'PT20M' 'PT3H'; $s1 = $s1rep; $grid = $true }
                'lone-to-rep'     { $s0 = Lone (Sb $tick.AddMinutes(-3)); $s1 = $s1rep; $grid = $true }
                'rep-start-moved' { $s0 = Rep (Sb $tick.AddMinutes(10)) 'PT10M' 'PT3H'; $s1 = $s1rep; $grid = $true }
                'rep-duration'    { $s0 = Rep (Sb $tick) 'PT10M' 'PT30M'; $s1 = $s1rep; $grid = $true }
                'rep-identical'   { $s0 = $s1rep; $s1 = $s1rep; $grid = $true }
                'rep-to-lone'     { $s0 = Rep (Sb $tick.AddMinutes(-3)) 'PT10M' 'PT3H'; $s1 = $s1lone; $grid = $false }
                'lone-moved'      { $s0 = Lone (Sb $tick.AddMinutes(-3)); $s1 = $s1lone; $grid = $false }
                'lone-identical'  { $s0 = $s1lone; $s1 = $s1lone; $grid = $false }
                'min-identical'   { $s1 = Rep (Sb $hourStart) 'PT1M' 'PT3H'; $s0 = $s1; $grid = $true; $step = 1; $gridStart = $hourStart }
                'min-interval'    { $s0 = Rep (Sb $hourStart) 'PT1M' 'PT3H'; $s1 = Rep (Sb $hourStart) 'PT2M' 'PT3H'; $grid = $true; $step = 2; $gridStart = $hourStart }
                'min-duration'    { $s0 = Rep (Sb $hourStart) 'PT1M' 'PT30M'; $s1 = Rep (Sb $hourStart) 'PT1M' 'PT3H'; $grid = $true; $step = 1; $gridStart = $hourStart }
                'min-to-lone'     { $s0 = Rep (Sb $hourStart) 'PT1M' 'PT3H'; $s1 = $s1lone; $grid = $false }
            }
            $log = "$Logs\u-$k-L$L-r$rep.log"; Remove-Item $log -ErrorAction SilentlyContinue
            $mk = { param($trig) New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $log 0" -Triggers $trig -StartWhenAvailable $true }
            $null = Register-ProbeTask $folder $name (& $mk $s0) $P
            $tasks += [pscustomobject]@{ Name = $name; Kind = $k; L = $L; Rep = $rep; Log = $log; Tick = $tick; Grid = $grid; Step = $step; GridStart = $gridStart; Xml1 = (& $mk $s1); CallStart = $null; CallEnd = $null; Err = $null }
        }
    }
}
"registered $($tasks.Count) tasks at $([DateTime]::UtcNow.ToString('o'))"
if ([DateTime]::UtcNow -ge $tu) { "STOP: registration took past Tu"; exit 1 }

# Phase alignment: wait on the clock for Tu (the experiment's own variable).
while ([DateTime]::UtcNow -lt $tu) { }
foreach ($t in $tasks) {
    $t.CallStart = [DateTime]::UtcNow
    try { $null = Register-ProbeTask $folder $t.Name $t.Xml1 $P 4 } catch { $t.Err = $_.Exception.Message }
    $t.CallEnd = [DateTime]::UtcNow
}
"updates: first call $($tasks[0].CallStart.ToString('HH:mm:ss.fff')), last call ended $($tasks[-1].CallEnd.ToString('HH:mm:ss.fff'))"
foreach ($t in $tasks) { if ($t.Err) { "UPDATE ERROR $($t.Name): $($t.Err)" } }

$end = $tu.AddMinutes($window)
while ([DateTime]::UtcNow -lt $end) { Start-Sleep -Seconds 5 }
"window over at $([DateTime]::UtcNow.ToString('o'))"

Section "runs after the update, per task (offsets in s from the task's update call)"
$summary = @{}
foreach ($t in $tasks) {
    $runs = @()
    foreach ($l in @(Get-Content $t.Log -ErrorAction SilentlyContinue)) {
        if ($l -match ' t=(\d+)') { $runs += [DateTime]::FromFileTimeUtc([long]$Matches[1]) }
    }
    $after = @($runs | Where-Object { $_ -ge $t.CallStart.AddSeconds(-1) })
    $before = $runs.Count - $after.Count
    $cls = @()
    foreach ($r in $after) {
        $off = ($r - $t.CallStart).TotalSeconds
        $kind = 'unexplained'
        if ($t.Grid) {
            $k = [Math]::Floor(($r - $t.GridStart).TotalMinutes / $t.Step)
            $g = $t.GridStart.AddMinutes($t.Step * $k)
            if ($g -gt $tu -and ($r - $g).TotalSeconds -lt 5) { $kind = 'tick' }
        }
        if ($kind -ne 'tick' -and $off -lt 10) { $kind = 'made-up' }
        $cls += "{0:N2}s:{1}" -f $off, $kind
    }
    $made = @($cls | Where-Object { $_ -like '*made-up' }).Count
    $unex = @($cls | Where-Object { $_ -like '*unexplained' }).Count
    "{0}: update call {1:HH:mm:ss.fff}-{2:HH:mm:ss.fff}; runs before the update {3}; after: {4}" -f $t.Name, $t.CallStart, $t.CallEnd, $before, ($cls -join ' ')
    $key = "$($t.Kind) L$($t.L)"
    if (-not $summary.ContainsKey($key)) { $summary[$key] = @{ Tasks = 0; Made = 0; Unex = 0 } }
    $summary[$key].Tasks++
    if ($made -gt 0) { $summary[$key].Made++ }
    if ($unex -gt 0) { $summary[$key].Unex++ }
}
Section "summary: tasks with a made-up run / tasks, tasks with an unexplained run"
foreach ($k in $kinds) {
    foreach ($L in 1, 3, 8) {
        $s = $summary["$k L$L"]
        "{0,-16} L={1}: made-up {2}/{3}; unexplained {4}" -f $k, $L, $s.Made, $s.Tasks, $s.Unex
    }
}
Remove-ProbeTasks $svc
exit 0
