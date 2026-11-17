# Does Task Scheduler fire a repetition and a lone daily trigger by civil time across a DST change, and how many runs does each
# case get under IgnoreNew and under Parallel? The change is reached without moving the clock: a custom time zone (a key under
# HKLM\...\Time Zones, selected with tzutil) whose change is ~12 minutes after the script starts. Throwaway Windows CI runner only
# (it makes a zone key, selects the zone, and restores both).
#
#   check   the zone only: does the Schedule service follow it? (GetRunTimes before and after selecting; a daily trigger whose
#           StartBoundary has a Z suffix lists in the zone's local time). Fires nothing.
#   spring  zone UTC+0 with DST UTC+1 starting at U0 (the clock jumps from 00 to 60 minutes later; the hour [U0, U0+60) civil is skipped).
#   fall    zone UTC+0 with DST UTC+1, and DST ending at U0 (the hour [U0, U0+60) civil occurs twice).
#
# U0 is the UTC instant of the change. Civil time here is the zone's local time, written as `civ(U)`:
#   spring: civ(U) = U      for U <  U0, U + 60 min for U >= U0
#   fall:   civ(U) = U + 60 min for U <  U0, U      for U >= U0
# Cases (SYSTEM, each registered under IgnoreNew and, beside it, Parallel; the Parallel control is `-par`):
#   rep   goetia's repetition shape (daily, StartBoundary 2000-01-01Thh:mm, Interval PT1M, Duration PT67M, no StopAtDurationEnd)
#         starting 4 civil minutes before the change, so the span covers the whole skipped (or repeated) hour and 3-4 minutes beyond.
#   lone  a daily trigger at a civil time inside the skipped (spring) or repeated (fall) hour, 55 minutes after U0 in civil numbers.
#   long  the same shapes under IgnoreNew whose action runs 20 s (spin) instead of ending at once.
# Each run appends one line (with its UTC time) to its task's own log. A marker task (one-time, Parallel) is due after every
# case's last possible fire under any reading (U0 + 130 minutes real); the counts are read when its line is written.
param([Parameter(Mandatory)][ValidateSet('check', 'spring', 'fall')][string]$Mode)
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
public static class Tz {
    [StructLayout(LayoutKind.Sequential)] public struct ST { public ushort Y, Mo, Dow, D, H, Mi, S, Ms; }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct DTZI {
        public int Bias;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string StdName;
        public ST StdDate; public int StdBias;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string DltName;
        public ST DltDate; public int DltBias;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string Key;
        public byte Disabled;
    }
    [DllImport("kernel32.dll")] static extern uint GetDynamicTimeZoneInformation(out DTZI i);
    public static string Describe() {
        DTZI i; uint r = GetDynamicTimeZoneInformation(out i);
        string state = r == 1 ? "STANDARD" : r == 2 ? "DAYLIGHT" : r == 0 ? "UNKNOWN" : "invalid " + r;
        return string.Format("GetDynamicTimeZoneInformation: {0}; key='{1}' Bias={2} StdBias={3} DltBias={4}; StdDate={5}-{6} wd{7} occ{8} {9}:{10:D2}; DltDate={11}-{12} wd{13} occ{14} {15}:{16:D2}",
            state, i.Key, i.Bias, i.StdBias, i.DltBias, i.StdDate.Y, i.StdDate.Mo, i.StdDate.Dow, i.StdDate.D, i.StdDate.H, i.StdDate.Mi, i.DltDate.Y, i.DltDate.Mo, i.DltDate.Dow, i.DltDate.D, i.DltDate.H, i.DltDate.Mi);
    }
}
'@

# U0: ~12 minutes ahead, on a whole minute, at a time of day where every time used stays on this UTC day. Waiting for that is a
# wait on the real clock, re-read each time.
function Get-U0 {
    while ($true) {
        $now = [DateTime]::UtcNow
        $u = $now.Date.AddMinutes([Math]::Ceiling($now.TimeOfDay.TotalMinutes) + 12)
        if ($u.TimeOfDay -ge [TimeSpan]::FromHours(1.5) -and $u.TimeOfDay -le [TimeSpan]::FromHours(20)) { return $u }
        "waiting: the clock is $($now.ToString('HH:mm')) UTC, outside the window in which the change fits the UTC day"
        Start-Sleep -Seconds 30
    }
}
$u0 = Get-U0
$u0text = $u0.ToString('yyyy-MM-dd HH:mm')
$day = $u0.ToString('yyyy-MM-dd')
$nextDay = $u0.AddDays(1).ToString('yyyy-MM-dd')
$shape = if ($Mode -eq 'fall') { 'fall' } else { 'spring' }
function Civ([DateTime]$u) {
    if ($shape -eq 'spring') { if ($u -lt $u0) { $u } else { $u.AddMinutes(60) } }
    else { if ($u -lt $u0) { $u.AddMinutes(60) } else { $u } }
}
function Ct([DateTime]$c) { $c.ToString('yyyy-MM-ddTHH:mm:00') }

# The zone: standard UTC+0, daylight UTC+1 (DaylightBias -60). The change is on U0's date, the weekday occurrence it falls on.
$zoneKey = 'Goetia Probe DST'
$zoneRoot = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Time Zones'
function Rule([DateTime]$d, [int]$h, [int]$mi) {
    $occ = [int][Math]::Ceiling($d.Day / 7.0)
    $b = [byte[]]::new(16)
    [BitConverter]::GetBytes([uint16]0).CopyTo($b, 0)
    [BitConverter]::GetBytes([uint16]$d.Month).CopyTo($b, 2)
    [BitConverter]::GetBytes([uint16][int]$d.DayOfWeek).CopyTo($b, 4)
    [BitConverter]::GetBytes([uint16]$occ).CopyTo($b, 6)
    [BitConverter]::GetBytes([uint16]$h).CopyTo($b, 8)
    [BitConverter]::GetBytes([uint16]$mi).CopyTo($b, 10)
    , $b
}
function Rule-Other([DateTime]$d, [int]$months) {
    $m = (($d.Month - 1 + $months) % 12 + 12) % 12 + 1
    $b = [byte[]]::new(16)
    [BitConverter]::GetBytes([uint16]$m).CopyTo($b, 2)
    [BitConverter]::GetBytes([uint16]0).CopyTo($b, 4)
    [BitConverter]::GetBytes([uint16]1).CopyTo($b, 6)
    [BitConverter]::GetBytes([uint16]3).CopyTo($b, 8)
    , $b
}
if ($shape -eq 'spring') {
    # DaylightDate is the change to daylight time, given in standard local time: U0 (local = UTC).
    $stdDate = Rule-Other $u0 3
    $dltDate = Rule $u0 $u0.Hour $u0.Minute
} else {
    # StandardDate is the change back, given in daylight local time: U0 + 60 min.
    $l = $u0.AddMinutes(60)
    $stdDate = Rule $l $l.Hour $l.Minute
    $dltDate = Rule-Other $u0 (-3)
}
$tzi = [byte[]]::new(0)
$tzi += [BitConverter]::GetBytes([int]0)      # Bias
$tzi += [BitConverter]::GetBytes([int]0)      # StandardBias
$tzi += [BitConverter]::GetBytes([int]-60)    # DaylightBias
$tzi += $stdDate
$tzi += $dltDate
function New-Zone {
    $k = New-Item -Path "$zoneRoot\$zoneKey" -Force
    New-ItemProperty -Path $k.PSPath -Name Display -Value '(UTC+00:00) Goetia probe' -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $k.PSPath -Name Std -Value 'Goetia Probe Standard Time' -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $k.PSPath -Name Dlt -Value 'Goetia Probe Daylight Time' -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $k.PSPath -Name TZI -Value $tzi -PropertyType Binary -Force | Out-Null
}
function Zone-Line([string]$What) { "$What : tzutil=$(tzutil /g) ; clock UTC $([DateTime]::UtcNow.ToString('o'))"; [Tz]::Describe() }

function Listing($Name, [string]$From, [string]$To) {
    $env:TSPROBE_ALL = '1'
    $lines = @(& $Script:Exe runtimes "$Script:TaskFolder\$Name" $From $To 100000 | Where-Object { $_ -match '^t ' })
    Remove-Item Env:\TSPROBE_ALL
    $times = foreach ($l in $lines) { $l.Substring(2, 16) }
    "{0} [{1}]: {2}" -f $Name, $lines.Count, ($times -join ' ')
}
function Reg([string]$Name, [string]$Trigger, [string]$Policy = 'IgnoreNew', [int]$SpinMs = 0) {
    $log = "$Logs\cz-$Name.log"; Remove-Item $log -ErrorAction SilentlyContinue
    $spin = if ($SpinMs -gt 0) { " spin_ms=$SpinMs" } else { '' }
    $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $log 0$spin" -Triggers $Trigger -Policy $Policy
    Register-ProbeTask $folder $Name $xml $P
}
function Daily([string]$Start) { "<CalendarTrigger><StartBoundary>$Start</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }
function Rep([string]$Start) { "<CalendarTrigger><StartBoundary>$Start</StartBoundary><Enabled>true</Enabled><Repetition><Interval>PT1M</Interval><Duration>PT67M</Duration></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }

$orig = (tzutil /g)
"zone before: $orig"
"mode $Mode; U0 = $u0text UTC; civil numbers: spring skips civ [U0, U0+60) ; fall repeats civ [U0, U0+60)"
$exit = 0
try {
    # ---- Step 1: does the Schedule service follow the custom zone? A daily trigger with a Z boundary lists in local time.
    Section "step 1: the Schedule service and the zone"
    $zA = Ct $u0.AddMinutes(-30); $zB = Ct $u0.AddMinutes(30)
    $null = Reg 'zA' (Daily "${day}T$($u0.AddMinutes(-30).ToString('HH:mm')):00Z")
    $null = Reg 'zB' (Daily "${day}T$($u0.AddMinutes(30).ToString('HH:mm')):00Z")
    Zone-Line 'zone UTC'
    "-- GetRunTimes of $day (local civil), zone as it came --"
    Listing 'zA' $day $nextDay; Listing 'zB' $day $nextDay
    New-Zone
    "zone key made; tzutil /s '$zoneKey' -> $((tzutil /s $zoneKey 2>&1) -join ' ')"
    Zone-Line 'zone selected'
    "-- GetRunTimes of $day after selecting (tasks registered before) --"
    Listing 'zA' $day $nextDay; Listing 'zB' $day $nextDay
    $null = Reg 'zA2' (Daily "${day}T$($u0.AddMinutes(-30).ToString('HH:mm')):00Z")
    $null = Reg 'zB2' (Daily "${day}T$($u0.AddMinutes(30).ToString('HH:mm')):00Z")
    "-- GetRunTimes of $day after selecting (tasks registered after) --"
    $a2 = Listing 'zA2' $day $nextDay; $b2 = Listing 'zB2' $day $nextDay
    $a2; $b2
    "expected by the zone's rule: zA2 at $((Civ $u0.AddMinutes(-30)).ToString('HH:mm')), zB2 at $((Civ $u0.AddMinutes(30)).ToString('HH:mm')); by UTC: zA2 at $($u0.AddMinutes(-30).ToString('HH:mm')), zB2 at $($u0.AddMinutes(30).ToString('HH:mm'))"
    $follows = ($a2 -match ([regex]::Escape((Civ $u0.AddMinutes(-30)).ToString('HH:mm')))) -and ($b2 -match ([regex]::Escape((Civ $u0.AddMinutes(30)).ToString('HH:mm'))))
    "the service follows the custom zone: $follows"
    if ($Mode -eq 'check') { return }
    if (-not $follows) { "STOP: the Schedule service does not follow the custom zone"; $exit = 1; return }

    # ---- Step 2: the cases, registered in the zone.
    Section "step 2: register"
    $repStart = Ct (Civ $u0.AddMinutes(-4))
    if ($shape -eq 'fall') { $repStart = Ct (Civ ($u0.AddMinutes(-4))) }
    $loneAt = Ct $u0.AddMinutes(55)                     # civil numbers [U0, U0+60) are the skipped or the repeated hour
    $repTrig = Rep "2000-01-01T$($repStart.Substring(11))"
    $loneTrig = Daily "2000-01-01T$($loneAt.Substring(11))"
    "rep starts at civil $($repStart.Substring(11)) (real $($u0.AddMinutes(-4).ToString('HH:mm')) UTC); lone at civil $($loneAt.Substring(11)); change at U0 = $($u0.ToString('HH:mm')) UTC"
    $names = @('rep-ign', 'rep-par', 'lone-ign', 'lone-par', 'rep-ign-long', 'lone-ign-long')
    $null = Reg 'rep-ign' $repTrig 'IgnoreNew'
    $null = Reg 'rep-par' $repTrig 'Parallel'
    $null = Reg 'lone-ign' $loneTrig 'IgnoreNew'
    $null = Reg 'lone-par' $loneTrig 'Parallel'
    $null = Reg 'rep-ign-long' $repTrig 'IgnoreNew' 20000
    $null = Reg 'lone-ign-long' $loneTrig 'IgnoreNew' 20000
    $markerCiv = Ct (Civ $u0.AddMinutes(130))
    $mlog = "$Logs\cz-marker.log"; Remove-Item $mlog -ErrorAction SilentlyContinue
    $mxml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $mlog 0" -Triggers "<TimeTrigger><StartBoundary>$markerCiv</StartBoundary><Enabled>true</Enabled></TimeTrigger>" -Policy 'Parallel'
    $null = Register-ProbeTask $folder 'marker' $mxml $P
    "marker due at civil $markerCiv (real $($u0.AddMinutes(130).ToString('HH:mm')) UTC)"
    "registered at $([DateTime]::UtcNow.ToString('o')) UTC"
    foreach ($n in $names) { Info ($folder.GetTask($n)) $n }
    Section "GetRunTimes of the change day, as registered"
    foreach ($n in 'lone-ign', 'rep-ign') { Listing $n $day $nextDay }

    # ---- Step 3: wait for the marker's line, the real condition; print the progress every five minutes.
    $lastP = [DateTime]::MinValue
    while (-not ((Test-Path $mlog) -and @(Get-Content $mlog -ErrorAction SilentlyContinue).Count -ge 1)) {
        if ([DateTime]::UtcNow - $lastP -gt [TimeSpan]::FromMinutes(5)) {
            $lastP = [DateTime]::UtcNow
            $c = ($names | ForEach-Object { "$_=$(@(Get-Content "$Logs\cz-$_.log" -ErrorAction SilentlyContinue).Count)" }) -join ' '
            "progress $($lastP.ToString('HH:mm:ss')) UTC: $c"
        }
        Start-Sleep -Seconds 5
    }
    "marker fired; clock $([DateTime]::UtcNow.ToString('o')) UTC"
    Zone-Line 'zone at the end'

    # ---- Step 4: the counts and every run's time.
    Section "runs per case (lines in the task's own log); each time as UTC and as the zone's civil time"
    foreach ($n in $names) {
        $log = "$Logs\cz-$n.log"
        $ls = @(Get-Content $log -ErrorAction SilentlyContinue)
        "{0}: runs={1}" -f $n, $ls.Count
        $times = foreach ($l in $ls) {
            if ($l -match ' t=(\d+)') {
                $u = [DateTime]::FromFileTimeUtc([long]$Matches[1])
                '{0}Z/{1}' -f $u.ToString('HH:mm:ss'), (Civ $u).ToString('HH:mm:ss')
            } else { $l }
        }
        "    " + ($times -join ' ')
    }
    foreach ($n in $names) { Info ($folder.GetTask($n)) $n }
}
finally {
    tzutil /s $orig
    Remove-ProbeTasks $svc
    Remove-Item "$zoneRoot\$zoneKey" -Recurse -ErrorAction SilentlyContinue
    Zone-Line "zone restored (was $orig)"
}
exit $exit
