# Does a task with the two triggers an either-or cron schedule compiles to run once or twice when both fire at the same
# instant under MultipleInstancesPolicy IgnoreNew? (goetia slice 5, Task 4 Step 0 (i).)
#
# The triggers are a MonthlyDays trigger on day D and a Weekly trigger on D's weekday, both at the same minute, which is
# what a cron schedule like `M H D * W` (day of the month OR weekday) compiles to. Each task's action appends one line to
# its own log and exits at once (`tsprobe act rec`), so the starts of a task are the lines in its log. Samples are tasks:
# -Samples of them per minute boundary, three boundaries, under IgnoreNew; a Parallel control (two triggers, two starts
# expected) shows the probe can see a second start at all.
#
# A correct run prints `RESULT ...` lines: for IgnoreNew, how many tasks started 0, 1, 2 or more times (every sample 1 means
# the single run holds), and for the control, how many started twice. Platform: a throwaway Windows CI runner only; it
# sets the machine's clock, so it never runs anywhere else.
#
# The clock is set once, to 23:56 on the day before D, and runs on by itself. The boundaries are 00:06, 00:07 and 00:08 on D
# for the IgnoreNew samples and 00:09 for the control; a one-time marker task at 00:12 fires after them, so its line proves
# the boundaries have passed and the counts are read on that condition, with no timed wait. The finally block restores the
# clock: the original instant plus the time that elapsed on the monotonic clock.
param([Parameter(Mandatory)][ValidateSet('system', 'user')][string]$Kind, [int]$Samples = 150, [int]$Controls = 30)
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$user = $null
if ($Kind -eq 'user') { $user = New-ProbeUser 'tsor' }
$P = Get-Principal $Kind $user
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

# D is a day of the month every month has (1 to 28), from tomorrow on.
$d = (Get-Date).Date.AddDays(1)
while ($d.Day -gt 28) { $d = $d.AddDays(1) }
$dow = [string]$d.DayOfWeek
"D = $($d.ToString('yyyy-MM-dd')) ($dow), day of the month $($d.Day)"

$allMonths = '<January/><February/><March/><April/><May/><June/><July/><August/><September/><October/><November/><December/>'
function Triggers([int]$minute) {
    # The start boundary is a fixed past date at the time of day, as compile's triggers will be written.
    $start = '2000-01-01T00:{0:D2}:00' -f $minute
    $byMonth = "<ScheduleByMonth><DaysOfMonth><Day>$($d.Day)</Day></DaysOfMonth><Months>$allMonths</Months></ScheduleByMonth>"
    $byWeek = "<ScheduleByWeek><WeeksInterval>1</WeeksInterval><DaysOfWeek><$dow/></DaysOfWeek></ScheduleByWeek>"
    "<CalendarTrigger><StartBoundary>$start</StartBoundary><Enabled>true</Enabled>$byMonth</CalendarTrigger>" +
    "<CalendarTrigger><StartBoundary>$start</StartBoundary><Enabled>true</Enabled>$byWeek</CalendarTrigger>"
}

$cases = [System.Collections.Generic.List[object]]::new()
foreach ($b in 0..2) {
    foreach ($n in 1..$Samples) { $cases.Add(@{ Name = "ign-$b-$n"; Policy = 'IgnoreNew'; Minute = 6 + $b }) }
}
foreach ($n in 1..$Controls) { $cases.Add(@{ Name = "par-$n"; Policy = 'Parallel'; Minute = 9 }) }

$orig = [DateTime]::UtcNow
$sw = [Diagnostics.Stopwatch]::StartNew()
"clock before: $($orig.ToString('o')) UTC; zone $((Get-TimeZone).Id)"
Stop-Service w32time -ErrorAction Continue
try {
    Set-Date -Date $d.AddMinutes(-4) | Out-Null
    "clock now: $((Get-Date).ToString('o')) local"
    foreach ($c in $cases) {
        $log = "$Logs\or-$($c.Name).log"; Remove-Item $log -ErrorAction SilentlyContinue
        $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $log 0" -Triggers (Triggers $c.Minute) -Policy $c.Policy
        $null = Register-ProbeTask $folder $c.Name $xml $P
    }
    $mlog = "$Logs\or-marker.log"; Remove-Item $mlog -ErrorAction SilentlyContinue
    $mxml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $mlog 0" -Policy 'Parallel' `
        -Triggers "<TimeTrigger><StartBoundary>$($d.ToString('yyyy-MM-dd'))T00:12:00</StartBoundary><Enabled>true</Enabled></TimeTrigger>"
    $null = Register-ProbeTask $folder 'marker' $mxml $P
    $now = Get-Date
    "registered $($cases.Count) tasks and the marker; clock $($now.ToString('o'))"
    if ($now -ge $d.AddMinutes(6)) { "INVALID: registration ran past the first boundary" }

    # The marker is due after every case's minute; then no task of the folder has an instance left.
    Wait-Lines $mlog 1
    foreach ($t in @($folder.GetTasks(1))) { while ($t.GetInstances(0).Count -gt 0) { } }
    "marker fired and the folder is idle; clock $((Get-Date).ToString('o'))"

    Section "starts per task (lines in the action's own log)"
    $tally = @{}
    foreach ($c in $cases) {
        $log = "$Logs\or-$($c.Name).log"
        $n = @(Get-Content $log -ErrorAction SilentlyContinue).Count
        $key = "$($c.Policy) $n"
        $tally[$key] = 1 + [int]$tally[$key]
        if ($c.Policy -eq 'IgnoreNew' -and $n -ne 1) {
            "NOT ONE: $($c.Name) (minute $($c.Minute)) started $n times; LastTaskResult=$($folder.GetTask($c.Name).LastTaskResult)"
            Get-Content $log -ErrorAction SilentlyContinue | ForEach-Object { "    $_" }
        }
    }
    foreach ($k in ($tally.Keys | Sort-Object)) { "RESULT $Kind $k starts: $($tally[$k]) tasks" }
    $ign = @($cases | Where-Object { $_.Policy -eq 'IgnoreNew' }).Count
    "RESULT $Kind IgnoreNew samples: $ign; Parallel control samples: $Controls"
}
finally {
    $restored = $orig.AddTicks($sw.Elapsed.Ticks)
    Set-Date -Date $restored.ToLocalTime() | Out-Null
    Start-Service w32time -ErrorAction Continue
    "clock restored: $((Get-Date).ToString('o')) local; expected $($restored.ToLocalTime().ToString('o'))"
    Remove-ProbeTasks $svc
    Remove-ProbeUser $user
}
exit 0
