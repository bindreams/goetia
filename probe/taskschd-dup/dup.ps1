# Task 3 Step 0 (goetia slice 5): does a monthly-weekday trigger with weeks 1 to 4 plus Last fire twice on a day
# whose weekday occurrence is both the 4th and the last of its month?
#
# Throwaway Windows CI runner only: it sets the machine's clock. D is the first day from today on that is the 4th
# occurrence of its weekday and the last of its month (the script finds it).
#
# Phase A, the clock at D-2 12:00: five triggers are registered and GetRunTimes lists every run from D-2 to D+60, so the
# listing's duplicated dates are known, D's among them or not.
# Phase B, the clock at D 12:00: the same triggers are registered afresh at 12:02 of D, and every firing appends a line to
# the task's own log (`tsprobe act rec`), under the Parallel policy so that two firings cannot be merged into one. A
# later one-time marker task fires after the cases are due: its log line proves the minute passed, so the count of each
# case is read on a real condition, not a window.
# The clock is restored by the finally block: the original instant plus the time that elapsed on the monotonic clock.
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

$d = (Get-Date).Date
while (-not ((([int]$d.Day - 1) / 7 -ge 3) -and ([int]$d.Day -le 28) -and ($d.AddDays(7).Month -ne $d.Month))) { $d = $d.AddDays(1) }
$dow = [string]$d.DayOfWeek
$other = [string]$d.AddDays(1).DayOfWeek
"target day D = $($d.ToString('yyyy-MM-dd')) ($dow); the day after is $other; days in month = $([DateTime]::DaysInMonth($d.Year, $d.Month))"

function Mon([string[]]$names) { ($names | ForEach-Object { "<$_/>" }) -join '' }
$allMonths = '<January/><February/><March/><April/><May/><June/><July/><August/><September/><October/><November/><December/>'
$allWeeks = '<Week>1</Week><Week>2</Week><Week>3</Week><Week>4</Week><Week>Last</Week>'
$noLast = '<Week>1</Week><Week>2</Week><Week>3</Week><Week>4</Week>'
function ByMonthDow([string]$weeks, [string[]]$dows) {
    "<ScheduleByMonthDayOfWeek><Weeks>$weeks</Weeks><DaysOfWeek>$(Mon $dows)</DaysOfWeek><Months>$allMonths</Months></ScheduleByMonthDayOfWeek>"
}
function Cal([string]$sched, [string]$start) {
    "<CalendarTrigger><StartBoundary>$start</StartBoundary><Enabled>true</Enabled>$sched</CalendarTrigger>"
}
$allDow = 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday', 'Sunday'

$dayText = $d.ToString('yyyy-MM-dd')
# The boundary is 90 days before D, as in M2 (whose boundary preceded every listed day): D is not the trigger's first day.
$begun = $d.AddDays(-90).ToString('yyyy-MM-dd') + 'T12:02:00'
$marker = "${dayText}T12:06:00"
$cases = [ordered]@{
    'multi-1to4-last'  = @{ W = $allWeeks; D = @($dow, $other); What = "weeks 1-4 + Last, {$dow, $other}" }
    'single-1to4-last' = @{ W = $allWeeks; D = @($dow); What = "weeks 1-4 + Last, {$dow}" }
    'all7-1to4-last'   = @{ W = $allWeeks; D = $allDow; What = 'weeks 1-4 + Last, all seven weekdays' }
    'multi-1to4'       = @{ W = $noLast; D = @($dow, $other); What = "weeks 1-4 only, {$dow, $other} (control)" }
    'multi-last'       = @{ W = '<Week>Last</Week>'; D = @($dow, $other); What = "Last only, {$dow, $other} (control)" }
}

function Register-Cases([string]$prefix, [string]$logPrefix) {
    foreach ($k in $cases.Keys) {
        $log = "$Logs\$logPrefix-$k.log"; Remove-Item $log -ErrorAction SilentlyContinue
        $trig = Cal (ByMonthDow $cases[$k].W $cases[$k].D) $begun
        $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $log 0" -Triggers $trig -Policy 'Parallel'
        $t = Register-ProbeTask $folder "$prefix$k" $xml $P
        "registered $prefix$k ($($cases[$k].What)); NextRunTime=$($t.NextRunTime.ToString('o'))"
    }
}

$orig = [DateTime]::UtcNow
$sw = [Diagnostics.Stopwatch]::StartNew()
"clock before: $($orig.ToString('o')) UTC; zone $((Get-TimeZone).Id)"
Stop-Service w32time -ErrorAction Continue
try {
    Section "phase A: the listing, with the clock at D-2"
    Set-Date -Date $d.AddDays(-2).AddHours(12) | Out-Null
    "clock now: $((Get-Date).ToString('o')) local"
    Register-Cases 'list-' 'list'
    $env:TSPROBE_ALL = '1'
    $from = $d.AddDays(-2).ToString('yyyy-MM-dd')
    $to = $d.AddDays(60).ToString('yyyy-MM-dd')
    foreach ($k in $cases.Keys) {
        "--- $k, run times in [$from, $to), every one"
        & $Script:Exe runtimes "$Script:TaskFolder\list-$k" $from $to 100000 | Where-Object { $_ -match '^(t |count|weekdays)' }
    }
    Remove-Item Env:\TSPROBE_ALL
    Remove-ProbeTasks $svc
    $folder = Get-ProbeFolder $svc

    Section "phase B: the firing, with the clock at D 12:00"
    Set-Date -Date $d.AddHours(12) | Out-Null
    "clock now: $((Get-Date).ToString('o')) local"
    Register-Cases 'fire-' 'dup'
    $mlog = "$Logs\dup-marker.log"; Remove-Item $mlog -ErrorAction SilentlyContinue
    $mxml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $mlog 0" -Triggers "<TimeTrigger><StartBoundary>$marker</StartBoundary><Enabled>true</Enabled></TimeTrigger>" -Policy 'Parallel'
    $null = Register-ProbeTask $folder 'marker' $mxml $P
    "registered the marker for $marker; clock $((Get-Date).ToString('o'))"
    $next = $d.AddDays(1).ToString('yyyy-MM-dd')
    foreach ($k in $cases.Keys) {
        "--- $k, GetRunTimes on D"
        & $Script:Exe runtimes "$Script:TaskFolder\fire-$k" $dayText $next 1000 | Where-Object { $_ -match '^(count|first|duplicates|weekdays)' }
    }

    # The marker is due after every case's trigger: its line proves the cases' minute has passed.
    Wait-Lines $mlog 1
    "marker fired; clock $((Get-Date).ToString('o'))"
    Section "firings per case (lines in the action's own log)"
    foreach ($k in $cases.Keys) {
        $log = "$Logs\dup-$k.log"
        $n = @(Get-Content $log -ErrorAction SilentlyContinue).Count
        "{0}: firings={1}" -f $k, $n
        Get-Content $log -ErrorAction SilentlyContinue | ForEach-Object { "    $_" }
    }
}
finally {
    $restored = $orig.AddTicks($sw.Elapsed.Ticks)
    Set-Date -Date $restored.ToLocalTime() | Out-Null
    Start-Service w32time -ErrorAction Continue
    "clock restored: $((Get-Date).ToString('o')) local; expected $($restored.ToLocalTime().ToString('o'))"
    Remove-ProbeTasks $svc
}
exit 0
