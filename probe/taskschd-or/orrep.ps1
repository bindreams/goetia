# Does a repetition on a monthly-weekdays trigger restart on each firing day? (goetia slice 5, Task 4 Step 0 (ii).)
#
# One task with a ScheduleByMonthDayOfWeek trigger (weeks 1 to 4 and Last, Monday to Friday, March to May) at 09:00 that
# repeats every 15 minutes for PT8H, and a Daily trigger with the same repetition as the control that is known to restart
# each day. `GetRunTimes` over 2027-03-01 to 2027-06-01 is read per day: a day restarts the repetition when its times are
# the whole grid 09:00 to 17:00 every 15 minutes (33 ticks, the last included).
#
# A correct run prints `RESULT ...` lines: the days listed against the Monday-to-Friday days of March to May, and how many
# of them hold the whole grid. Platform: a throwaway Windows CI runner only (it registers scheduled tasks).
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

$allMonths = '<January/><February/><March/><April/><May/><June/><July/><August/><September/><October/><November/><December/>'
$springMonths = '<March/><April/><May/>'
$weekdays = '<Monday/><Tuesday/><Wednesday/><Thursday/><Friday/>'
$allWeeks = '<Week>1</Week><Week>2</Week><Week>3</Week><Week>4</Week><Week>Last</Week>'
$rep = '<Repetition><Interval>PT15M</Interval><Duration>PT8H</Duration><StopAtDurationEnd>false</StopAtDurationEnd></Repetition>'
$start = '2000-01-01T09:00:00'
function Cal([string]$sched) { "<CalendarTrigger><StartBoundary>$start</StartBoundary><Enabled>true</Enabled>$rep$sched</CalendarTrigger>" }
$cases = [ordered]@{
    'monthly-weekdays' = Cal "<ScheduleByMonthDayOfWeek><Weeks>$allWeeks</Weeks><DaysOfWeek>$weekdays</DaysOfWeek><Months>$springMonths</Months></ScheduleByMonthDayOfWeek>"
    'daily-control'    = Cal '<ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>'
}

$grid = foreach ($m in 0..32) { '{0:D2}:{1:D2}' -f [int](9 + [math]::Floor($m / 4)), [int](15 * ($m % 4)) }
"the grid: $($grid.Count) ticks, $($grid[0]) to $($grid[-1])"

$expectedDays = [System.Collections.Generic.List[string]]::new()
for ($day = [datetime]'2027-03-01'; $day -lt [datetime]'2027-06-01'; $day = $day.AddDays(1)) {
    if ($day.DayOfWeek -notin 'Saturday', 'Sunday') { $expectedDays.Add($day.ToString('yyyy-MM-dd')) }
}
"Monday-to-Friday days in [2027-03-01, 2027-06-01): $($expectedDays.Count)"

$env:TSPROBE_ALL = '1'
foreach ($k in $cases.Keys) {
    Section $k
    $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $Logs\orrep.log 0" -Triggers $cases[$k]
    try { $null = Register-ProbeTask $folder $k $xml $P } catch { "RESULT ${k}: registration refused: $($_.Exception.Message.Trim())"; continue }
    $lines = @(& $Script:Exe runtimes "$Script:TaskFolder\$k" 2027-03-01 2027-06-01 100000 | Where-Object { $_ -match '^t ' })
    $byDay = [ordered]@{}
    foreach ($l in $lines) {
        $day = $l.Substring(2, 10); $time = $l.Substring(13, 5)
        if (-not $byDay.Contains($day)) { $byDay[$day] = [System.Collections.Generic.List[string]]::new() }
        $byDay[$day].Add($time)
    }
    $full = 0; $dupDays = 0; $other = [System.Collections.Generic.List[string]]::new()
    foreach ($day in $byDay.Keys) {
        $times = $byDay[$day]
        if (@($times | Select-Object -Unique).Count -ne $times.Count) { $dupDays++ }
        $got = ($times | Select-Object -Unique) -join ' '
        $want = $grid -join ' '
        if ($got -eq $want) { $full++ } else { $other.Add("$day [$($times.Count)]: $(($times | Select-Object -First 4) -join ' ')...") }
    }
    $notExpected = @($byDay.Keys | Where-Object { $_ -notin $expectedDays })
    $missing = @($expectedDays | Where-Object { -not $byDay.Contains($_) })
    "RESULT ${k}: run times listed $($lines.Count); days listed $($byDay.Count); days with the whole grid $full; days with duplicated times $dupDays; Monday-to-Friday days not listed $($missing.Count); days listed that are not Monday to Friday $($notExpected.Count)"
    foreach ($o in ($other | Select-Object -First 12)) { "  not the whole grid: $o" }
    foreach ($m in ($missing | Select-Object -First 5)) { "  not listed: $m" }
}
Remove-Item Env:\TSPROBE_ALL
Remove-ProbeTasks $svc
exit 0
