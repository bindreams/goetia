# Task 3 Step 0 (goetia slice 5): does a monthly-weekday trigger with weeks 1 to 4 plus Last fire twice on a day
# whose weekday occurrence is both the 4th and the last of its month?
#
# Throwaway Windows CI runner only: it sets the machine's clock. The clock is set to a date at 12:00 local on which
# every weekday's 4th occurrence is also its last (found by the script), tasks are registered a few minutes ahead, and
# every firing appends a line to the task's own log (`tsprobe act rec`), under the Parallel policy so that two firings
# cannot be merged into one. A later one-time marker task fires after the triggers of the cases are due: its own log
# line is the proof that the minute passed, so the count of each case is read on a real condition, not a window.
# The clock is restored by the finally block: the original instant plus the time that elapsed on the monotonic clock.
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

# The first day D, from today on, that is the 4th occurrence of its weekday and the last one of its month.
$today = (Get-Date).Date
$d = $today
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
$due = "${dayText}T12:02:00"
# The trigger's boundary is 90 days before D, as in M2 (whose boundary preceded every listed day): D is not its first day.
$begun = $d.AddDays(-90).ToString('yyyy-MM-dd') + 'T12:02:00'
$from = $d.AddDays(-90).ToString('yyyy-MM-dd')
$marker = "${dayText}T12:06:00"
$cases = [ordered]@{
    'multi-1to4-last'  = @{ W = $allWeeks; D = @($dow, $other); What = "weeks 1-4 + Last, {$dow, $other}" }
    'single-1to4-last' = @{ W = $allWeeks; D = @($dow); What = "weeks 1-4 + Last, {$dow}" }
    'all7-1to4-last'   = @{ W = $allWeeks; D = $allDow; What = 'weeks 1-4 + Last, all seven weekdays' }
    'multi-1to4'       = @{ W = $noLast; D = @($dow, $other); What = "weeks 1-4 only, {$dow, $other} (control)" }
    'multi-last'       = @{ W = '<Week>Last</Week>'; D = @($dow, $other); What = "Last only, {$dow, $other} (control)" }
}

$orig = [DateTime]::UtcNow
$sw = [Diagnostics.Stopwatch]::StartNew()
"clock before: $($orig.ToString('o')) UTC; zone $((Get-TimeZone).Id)"
Stop-Service w32time -ErrorAction Continue
try {
    Set-Date -Date $d.AddHours(12) | Out-Null
    "clock now: $((Get-Date).ToString('o')) local"

    foreach ($k in $cases.Keys) {
        $log = "$Logs\dup-$k.log"; Remove-Item $log -ErrorAction SilentlyContinue
        $trig = Cal (ByMonthDow $cases[$k].W $cases[$k].D) $begun
        $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $log 0" -Triggers $trig -Policy 'Parallel'
        $t = Register-ProbeTask $folder $k $xml $P
        "registered $k ($($cases[$k].What)); NextRunTime=$($t.NextRunTime.ToString('o'))"
    }
    $mlog = "$Logs\dup-marker.log"; Remove-Item $mlog -ErrorAction SilentlyContinue
    $mxml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $mlog 0" -Triggers "<TimeTrigger><StartBoundary>$marker</StartBoundary><Enabled>true</Enabled></TimeTrigger>" -Policy 'Parallel'
    $null = Register-ProbeTask $folder 'marker' $mxml $P
    "registered the marker for $marker; clock $((Get-Date).ToString('o'))"

    Section "GetRunTimes on D, per case"
    $next = $d.AddDays(1).ToString('yyyy-MM-dd')
    foreach ($k in $cases.Keys) {
        "--- $k"
        & $Script:Exe runtimes "$Script:TaskFolder\$k" $dayText $next 1000 | Where-Object { $_ -match '^(count|first|duplicates|weekdays|times)' }
        "  over the 90 days before D as well (the listing's duplicates, as in M2):"
        & $Script:Exe runtimes "$Script:TaskFolder\$k" $from $next 1000 | Where-Object { $_ -match '^(count|last|weekdays)' }
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
