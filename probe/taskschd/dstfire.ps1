# A real fire across a DST change: does a repetition's grid follow elapsed time or local civil time? The runner's clock is set
# 12 minutes before US Pacific's change, a daily trigger at 01:51 local repeats every 5 minutes for 30 minutes, and the action
# records each start with its UTC time. Throwaway Windows CI runner only (it sets the system clock and zone, and restores both).
#
#   spring: 2027-03-14 02:00 PST -> 03:00 PDT (10:00Z). Elapsed: ticks 01:51, 01:56 PST, then 03:01 ... 03:21 PDT (10:01Z ... 10:21Z).
#           Civil: the span ends at 02:21 civil, so no tick at or after 03:00 PDT.
#   fall:   2027-11-07 02:00 PDT -> 01:00 PST (09:00Z). Elapsed: 01:51, 01:56 PDT, then 01:01 ... 01:21 PST (09:01Z ... 09:21Z).
#           Civil: 02:01 ... 02:21 are PST, 10:01Z ... 10:21Z.
param([Parameter(Mandatory)][ValidateSet('spring', 'fall')][string]$Change)
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Clock {
    [StructLayout(LayoutKind.Sequential)]
    public struct ST { public ushort Year, Month, DayOfWeek, Day, Hour, Minute, Second, Ms; }
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetSystemTime(ref ST t);
    public static int Set(DateTime utc) {
        var s = new ST { Year = (ushort)utc.Year, Month = (ushort)utc.Month, Day = (ushort)utc.Day, Hour = (ushort)utc.Hour, Minute = (ushort)utc.Minute, Second = (ushort)utc.Second, Ms = (ushort)utc.Millisecond };
        return SetSystemTime(ref s) ? 0 : Marshal.GetLastWin32Error();
    }
}
'@

if ($Change -eq 'spring') {
    $setUtc = [DateTime]::new(2027, 3, 14, 9, 48, 0, [DateTimeKind]::Utc)    # 01:48 PST
    $endUtc = [DateTime]::new(2027, 3, 14, 10, 30, 0, [DateTimeKind]::Utc)   # 03:30 PDT
    $start = '2027-03-14T01:51:00'
} else {
    $setUtc = [DateTime]::new(2027, 11, 7, 8, 48, 0, [DateTimeKind]::Utc)    # 01:48 PDT
    $endUtc = [DateTime]::new(2027, 11, 7, 10, 30, 0, [DateTimeKind]::Utc)   # 02:30 PST
    $start = '2027-11-07T01:51:00'
}
$log = "$Logs\dstfire-$Change.log"
$pacific = [TimeZoneInfo]::FindSystemTimeZoneById('Pacific Standard Time')

$orig = (tzutil /g)
"zone before: $orig"
foreach ($name in 'w32time', 'vmictimesync') {
    $s = Get-Service -Name $name -ErrorAction SilentlyContinue
    if ($s) { Set-Service -Name $name -StartupType Disabled -ErrorAction SilentlyContinue; Stop-Service -Name $name -Force -ErrorAction SilentlyContinue; "$name : $((Get-Service -Name $name).Status)" } else { "$name : no such service" }
}
$realStart = [DateTime]::UtcNow
$watch = [Diagnostics.Stopwatch]::StartNew()
tzutil /s "Pacific Standard Time"
"zone now: $(tzutil /g)"
try {
    "setting the clock to $($setUtc.ToString('o')): win32 result $([Clock]::Set($setUtc))"
    "clock now (UTC): $([DateTime]::UtcNow.ToString('o')); local $([TimeZoneInfo]::ConvertTimeFromUtc([DateTime]::UtcNow, $pacific).ToString('o'))"
    $trigger = "<CalendarTrigger><StartBoundary>$start</StartBoundary><Enabled>true</Enabled><Repetition><Interval>PT5M</Interval><Duration>PT30M</Duration><StopAtDurationEnd>false</StopAtDurationEnd></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>"
    $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $log 0" -Triggers $trigger
    $t = Register-ProbeTask $folder 'dstfire' $xml $P
    "registered; NextRunTime=$($t.NextRunTime.ToString('o'))"

    # Wait on the real condition, the clock reaching the end; the job's timeout is the bound if the clock stopped.
    $last = [DateTime]::MinValue
    while ([DateTime]::UtcNow -lt $endUtc) {
        if ([DateTime]::UtcNow - $last -gt [TimeSpan]::FromMinutes(5)) { $last = [DateTime]::UtcNow; "clock $($last.ToString('o')); fires so far: $(@(Get-Content $log -ErrorAction SilentlyContinue).Count)" }
        Start-Sleep -Seconds 2
    }
    Section "fires (action's UTC time, and Pacific local time)"
    foreach ($l in @(Get-Content $log -ErrorAction SilentlyContinue)) {
        if ($l -match ' t=(\d+)') {
            $u = [DateTime]::FromFileTimeUtc([long]$Matches[1])
            "{0}Z  =  {1} local ({2})" -f $u.ToString('yyyy-MM-dd HH:mm:ss'), ([TimeZoneInfo]::ConvertTimeFromUtc($u, $pacific)).ToString('yyyy-MM-dd HH:mm:ss'), $(if ($pacific.IsDaylightSavingTime([TimeZoneInfo]::ConvertTimeFromUtc($u, $pacific))) { 'PDT' } else { 'PST' })
        } else { $l }
    }
    Info $t 'task'
}
finally {
    "restoring the clock: win32 result $([Clock]::Set($realStart + $watch.Elapsed))"
    tzutil /s $orig
    "zone restored: $(tzutil /g); clock (UTC) $([DateTime]::UtcNow.ToString('o'))"
}
Remove-ProbeTasks $svc
exit 0
