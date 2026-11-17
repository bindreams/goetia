# M2: trigger shapes against GetRunTimes over two years (2027-01-01 .. 2029-01-01; 2028 is a leap year).
# Throwaway Windows CI runner only. The run times are Task Scheduler's own, read by `tsprobe runtimes`.
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

$allMonths = '<January/><February/><March/><April/><May/><June/><July/><August/><September/><October/><November/><December/>'
function Mon([string[]]$names) { ($names | ForEach-Object { "<$_/>" }) -join '' }
function Days([int[]]$d) { ($d | ForEach-Object { "<Day>$_</Day>" }) -join '' }
function Rep([string]$interval, [string]$duration = '', [bool]$stop = $false) {
    $d = if ($duration) { "<Duration>$duration</Duration>" } else { '' }
    "<Repetition><Interval>$interval</Interval>$d<StopAtDurationEnd>$("$stop".ToLower())</StopAtDurationEnd></Repetition>"
}
function Cal([string]$sched, [string]$rep = '', [string]$start = '2027-01-01T03:30:00', [bool]$en = $true) {
    "<CalendarTrigger><StartBoundary>$start</StartBoundary><Enabled>$("$en".ToLower())</Enabled>$rep$sched</CalendarTrigger>"
}
function ByDay([int]$n) { "<ScheduleByDay><DaysInterval>$n</DaysInterval></ScheduleByDay>" }
function ByWeek([string[]]$dows, [int]$n = 1) { "<ScheduleByWeek><WeeksInterval>$n</WeeksInterval><DaysOfWeek>$(Mon $dows)</DaysOfWeek></ScheduleByWeek>" }
function ByMonth([string]$days, [string]$months = $allMonths) { "<ScheduleByMonth><DaysOfMonth>$days</DaysOfMonth><Months>$months</Months></ScheduleByMonth>" }
function ByMonthDow([string]$weeks, [string[]]$dows, [string]$months = $allMonths) {
    "<ScheduleByMonthDayOfWeek><Weeks>$weeks</Weeks><DaysOfWeek>$(Mon $dows)</DaysOfWeek><Months>$months</Months></ScheduleByMonthDayOfWeek>"
}
$allWeeks = '<Week>1</Week><Week>2</Week><Week>3</Week><Week>4</Week><Week>Last</Week>'
$allDow = 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday', 'Sunday'

$shapes = [ordered]@{
    'daily-1'              = @{ T = (Cal (ByDay 1)) }
    'daily-2'              = @{ T = (Cal (ByDay 2)) }
    'daily-3'              = @{ T = (Cal (ByDay 3)) }
    'daily-2-anchored-2020' = @{ T = (Cal (ByDay 2) '' '2020-01-02T03:30:00') }
    'weekly-mon-fri'       = @{ T = (Cal (ByWeek 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday')) }
    'weekly-sunday'        = @{ T = (Cal (ByWeek 'Sunday')) }
    'weekly-all-days'      = @{ T = (Cal (ByWeek $allDow)) }
    'weekly-every-2-weeks-monday' = @{ T = (Cal (ByWeek 'Monday' 2)) }
    'monthly-1-15'         = @{ T = (Cal (ByMonth (Days 1, 15))) }
    'monthly-29-30-31'     = @{ T = (Cal (ByMonth (Days 29, 30, 31))) }
    'monthly-31'           = @{ T = (Cal (ByMonth (Days 31))) }
    'monthly-30'           = @{ T = (Cal (ByMonth (Days 30))) }
    'monthly-29'           = @{ T = (Cal (ByMonth (Days 29))) }
    'monthly-last-day'     = @{ T = (Cal (ByMonth '<Day>Last</Day>')) }
    'monthly-1-and-last'   = @{ T = (Cal (ByMonth ((Days 1) + '<Day>Last</Day>'))) }
    'monthly-all-1-to-31'  = @{ T = (Cal (ByMonth (Days (1..31)))) }
    'feb-30 (0 0 30 2 *)'  = @{ T = (Cal (ByMonth (Days 30) (Mon 'February'))) }
    'feb-29'               = @{ T = (Cal (ByMonth (Days 29) (Mon 'February'))) }
    'feb-28-29-30-31'      = @{ T = (Cal (ByMonth (Days 28, 29, 30, 31) (Mon 'February'))) }
    'apr-jun-sep-nov-31'   = @{ T = (Cal (ByMonth (Days 31) (Mon 'April', 'June', 'September', 'November'))) }
    'monthly-first-monday' = @{ T = (Cal (ByMonthDow '<Week>1</Week>' 'Monday')) }
    'monthly-second-monday' = @{ T = (Cal (ByMonthDow '<Week>2</Week>' 'Monday')) }
    'monthly-third-monday' = @{ T = (Cal (ByMonthDow '<Week>3</Week>' 'Monday')) }
    'monthly-fourth-monday' = @{ T = (Cal (ByMonthDow '<Week>4</Week>' 'Monday')) }
    'monthly-last-monday'  = @{ T = (Cal (ByMonthDow '<Week>Last</Week>' 'Monday')) }
    'monthly-weeks-1-to-last-monday' = @{ T = (Cal (ByMonthDow $allWeeks 'Monday')) }
    'monthly-week-5-monday (a fifth occurrence)' = @{ T = (Cal (ByMonthDow '<Week>5</Week>' 'Monday')) }
    'june-mondays (0 0 * 6 1)' = @{ T = (Cal (ByMonthDow $allWeeks 'Monday' (Mon 'June'))) }
    'mar-may-weekdays (0 3 * 3-5 1-5)' = @{ T = (Cal (ByMonthDow $allWeeks ('Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday') (Mon 'March', 'April', 'May'))) }
    'or-case: day 1 + mondays (two triggers)' = @{ T = ((Cal (ByMonth (Days 1))) + (Cal (ByWeek 'Monday'))) }
    'one-time 2027-06-15 12:00' = @{ T = '<TimeTrigger><StartBoundary>2027-06-15T12:00:00</StartBoundary><Enabled>true</Enabled></TimeTrigger>' }
    'repeat-15m-for-8h from 09:00' = @{ T = (Cal (ByDay 1) (Rep 'PT15M' 'PT8H') '2027-01-01T09:00:00') }
    'repeat-15m-for-8h stop-at-end' = @{ T = (Cal (ByDay 1) (Rep 'PT15M' 'PT8H' $true) '2027-01-01T09:00:00') }
    'repeat-15m-for-7h45m from 09:00' = @{ T = (Cal (ByDay 1) (Rep 'PT15M' 'PT7H45M') '2027-01-01T09:00:00') }
    'repeat-7m-for-1d from 00:00 (does not divide the hour)' = @{ T = (Cal (ByDay 1) (Rep 'PT7M' 'P1D') '2027-01-01T00:00:00') }
    'repeat-7m-for-24h from 00:00' = @{ T = (Cal (ByDay 1) (Rep 'PT7M' 'PT24H') '2027-01-01T00:00:00') }
    'repeat-7m-no-duration from 00:00' = @{ T = (Cal (ByDay 1) (Rep 'PT7M') '2027-01-01T00:00:00') }
    'repeat-1m-for-1d from 00:00 (* * * * *)' = @{ T = (Cal (ByDay 1) (Rep 'PT1M' 'P1D') '2027-01-01T00:00:00') }
    'repeat-1m-no-duration from 00:00' = @{ T = (Cal (ByDay 1) (Rep 'PT1M') '2027-01-01T00:00:00') }
    'repeat-1h-for-1d from 00:30 (30 * * * *)' = @{ T = (Cal (ByDay 1) (Rep 'PT1H' 'P1D') '2027-01-01T00:30:00') }
    'repeat-90m-for-1d from 00:00' = @{ T = (Cal (ByDay 1) (Rep 'PT90M' 'P1D') '2027-01-01T00:00:00') }
    'repeat-1h-for-4h from 22:00 (past midnight)' = @{ T = (Cal (ByDay 1) (Rep 'PT1H' 'PT4H') '2027-01-01T22:00:00') }
    'repeat-30m-for-1d from 03:30 weekly mon-fri' = @{ T = (Cal (ByWeek 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday') (Rep 'PT30M' 'P1D') '2027-01-01T03:30:00') }
    'repeat-1h-for-9h monthly day 1' = @{ T = (Cal (ByMonth (Days 1)) (Rep 'PT1H' 'PT9H') '2027-01-01T09:00:00') }
    'disabled trigger (daily)' = @{ T = (Cal (ByDay 1) '' '2027-01-01T03:30:00' $false) }
    'disabled + enabled trigger' = @{ T = ((Cal (ByDay 1) '' '2027-01-01T03:30:00' $false) + (Cal (ByWeek 'Monday') '' '2027-01-01T05:00:00')) }
    'task disabled (Settings/Enabled false), daily' = @{ T = (Cal (ByDay 1)); Off = $true }
    'daily ends 2027-03-01 (EndBoundary)' = @{ T = "<CalendarTrigger><StartBoundary>2027-01-01T03:30:00</StartBoundary><EndBoundary>2027-03-01T00:00:00</EndBoundary><Enabled>true</Enabled>$(ByDay 1)</CalendarTrigger>" }
}

# One task per shape, then GetRunTimes through tsprobe.
Section "GetRunTimes count capacity (the shape * * * * *)"
$xml = New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop' -Triggers (Cal (ByDay 1) (Rep 'PT1M' 'P1D') '2027-01-01T00:00:00')
$null = Register-ProbeTask $folder 'captest' $xml $P
& $Script:Exe runtimes "$Script:TaskFolder\captest" captest

$n = 0
foreach ($k in $shapes.Keys) {
    $n++
    $name = "m2-$n"
    Section "shape $n : $k"
    try {
        $off = [bool]$shapes[$k].Off
        $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop' -Triggers $shapes[$k].T -Enabled (-not $off)
        $t = Register-ProbeTask $folder $name $xml $P
        "registered; NextRunTime=$($t.NextRunTime.ToString('o')) State=$($t.State) triggers-read-back=$($t.Definition.Triggers.Count)"
        & $Script:Exe runtimes "$Script:TaskFolder\$name" 2027-01-01 2029-01-01 100000
    } catch { "REGISTRATION REFUSED: $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
}

Section "how many triggers does repetition count for: N triggers, each with a repetition"
foreach ($cnt in 48, 49) {
    $trigs = (1..$cnt | ForEach-Object { Cal (ByDay 1) (Rep 'PT10M' 'PT1H') ("2027-01-01T{0:00}:{1:00}:00" -f [math]::Floor(($_ - 1) / 2), (($_ - 1) % 2 * 30)) }) -join ''
    $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop' -Triggers $trigs
    try { $t = Register-ProbeTask $folder "cap$cnt" $xml $P; "$cnt repeating triggers: registered, read back $($t.Definition.Triggers.Count)" }
    catch { "$cnt repeating triggers: REFUSED $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
}

Remove-ProbeTasks $svc
