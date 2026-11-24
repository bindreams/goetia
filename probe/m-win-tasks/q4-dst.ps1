# Question 4: Task Scheduler's registration-window extra runs on a spring-forward day (KB 1eaf2d1): how long before the change, which shapes,
# the day after. Adapted from the knowledge base's `dsthours.ps1` (same zone key, same hours, same shapes and action), spring only, with the
# change placed by -Leg relative to the registration (the clock is never moved; a custom zone selected with tzutil, every task registered
# after it):
#   ctrl    no zone change (UTC), the hours of d68 as if there were one
#   d10 d30 d68 d130 d250   the change U0 is that many minutes after the registration (d68 is the earlier probe's distance)
#   after   the zone's change was 24 h before d68's U0 (registered the day after the change, the same wall-clock hours)
#   before  the zone's change is 24 h after d68's U0 (registered the day before it)
# Hours: L0 is the civil hour boundary at which the change happens; A = [L0-1h, L0), B = [L0, L0+1h) skipped, C = [L0+1h, L0+2h). Shapes, one
# daily trigger per hour (A, B, C), all SYSTEM, IgnoreNew, instant action that appends its start time to its own log:
#   rep PT15M/PT45M @:00   prog PT5M/PT5M @:05   min PT1M/PT59M @:00   lone @:30   zero @:00   rep15 PT15M/PT30M @:15
#   rep1 PT1M/PT58M @:01   p2 PT2M/PT58M @:00   p5 PT5M/PT55M @:00   p10 PT10M/PT50M @:00   p30 PT30M/PT30M @:00
# The window ends at a marker (one-time trigger with a Z start) a few minutes after the hour the question concerns; the counts are read when
# its line appears. Output per task: the runs per real hour relative to U0 and every run's UTC minute. Throwaway Windows CI runner only.
param([Parameter(Mandatory)][ValidateSet('ctrl', 'd10', 'd30', 'd68', 'd130', 'd250', 'after', 'before')][string]$Leg)
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

$dist = switch ($Leg) { 'd10' { 10 } 'd30' { 30 } 'd130' { 130 } 'd250' { 250 } default { 68 } }
$shiftDays = switch ($Leg) { 'after' { -1 } 'before' { 1 } default { 0 } }
$zoneChange = $Leg -ne 'ctrl'

# U0: $dist minutes ahead (24 h earlier or later for after, before), on a whole minute. The reference U0 (before the shift) puts the first
# hour A at $dist - 60 minutes from now.
$now0 = [DateTime]::UtcNow
$u0ref = $now0.Date.AddMinutes([Math]::Ceiling($now0.TimeOfDay.TotalMinutes) + $dist)
$u0 = $u0ref.AddDays($shiftDays)
$bias = $u0.Minute - 60          # standard local time = UTC - bias: a whole hour at U0, 1 to 60 minutes ahead of UTC
$l0 = $u0.AddMinutes(-$bias)
$day = $l0.ToString('yyyy-MM-dd'); $nextDay = $l0.AddDays(1).ToString('yyyy-MM-dd')
function Civ([DateTime]$u) { $s = $u.AddMinutes(-$bias); if ($u -lt $u0) { $s } else { $s.AddMinutes(60) } }   # spring
$aStart = $u0ref.AddMinutes(-60)                                   # real start of hour A, on the registration's day
$endReal = if ($Leg -in 'd10', 'd30') { $u0ref.AddMinutes(4) } else { $aStart.AddMinutes(8) }
"leg $Leg; clock $($now0.ToString('o')); U0 = $($u0.ToString('yyyy-MM-dd HH:mm')) UTC (reference $($u0ref.ToString('HH:mm'))); bias $bias; L0 = $($l0.ToString('yyyy-MM-dd HH:mm')); A starts at $($aStart.ToString('HH:mm')) UTC; window ends at $($endReal.ToString('HH:mm')) UTC"

$zoneKey = 'Goetia Probe DST Hours'
$zoneRoot = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Time Zones'
function Rule([DateTime]$d) {
    $occ = [int][Math]::Ceiling($d.Day / 7.0)
    $b = [byte[]]::new(16)
    [BitConverter]::GetBytes([uint16]0).CopyTo($b, 0)
    [BitConverter]::GetBytes([uint16]$d.Month).CopyTo($b, 2)
    [BitConverter]::GetBytes([uint16][int]$d.DayOfWeek).CopyTo($b, 4)
    [BitConverter]::GetBytes([uint16]$occ).CopyTo($b, 6)
    [BitConverter]::GetBytes([uint16]$d.Hour).CopyTo($b, 8)
    [BitConverter]::GetBytes([uint16]$d.Minute).CopyTo($b, 10)
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
$tzi = [byte[]]::new(0)
$tzi += [BitConverter]::GetBytes([int]$bias)
$tzi += [BitConverter]::GetBytes([int]0)
$tzi += [BitConverter]::GetBytes([int]-60)
$tzi += (Rule-Other $l0 3)       # StandardDate, three months away
$tzi += (Rule $l0)               # DaylightDate: the change, in standard local time
function New-Zone {
    $k = New-Item -Path "$zoneRoot\$zoneKey" -Force
    New-ItemProperty -Path $k.PSPath -Name Display -Value '(UTC) Goetia probe hours' -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $k.PSPath -Name Std -Value 'Goetia Probe Hours Std' -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $k.PSPath -Name Dlt -Value 'Goetia Probe Hours Dlt' -PropertyType String -Force | Out-Null
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
function Reg([string]$Name, [string]$Trigger) {
    $log = "$Logs\dst-$Name.log"; Remove-Item $log -ErrorAction SilentlyContinue
    $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $log 0" -Triggers $Trigger -Policy 'IgnoreNew'
    Register-ProbeTask $folder $Name $xml $P
}
function Daily([string]$Start) { "<CalendarTrigger><StartBoundary>$Start</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }
function Rep([string]$Start, [string]$Every, [string]$For) { "<CalendarTrigger><StartBoundary>$Start</StartBoundary><Enabled>true</Enabled><Repetition><Interval>$Every</Interval><Duration>$For</Duration></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }
function Boundary([DateTime]$c) { "2000-01-01T$($c.ToString('HH:mm')):00" }

$orig = (tzutil /g)
"zone before: $orig"
$exit = 0
try {
    if ($zoneChange) {
        Section "zone"
        Zone-Line 'zone UTC'
        New-Zone
        "zone key made; tzutil /s '$zoneKey' -> $((tzutil /s $zoneKey 2>&1) -join ' ')"
        Zone-Line 'zone selected'
        $zb = Daily "${day}T$($u0.AddMinutes(30).ToString('HH:mm')):00Z"
        $null = Reg 'zB2' $zb
        $b2 = Listing 'zB2' $day $nextDay
        $b2
        "expected by the zone's rule: zB2 at $((Civ $u0.AddMinutes(30)).ToString('HH:mm')); the service follows it: $($b2 -match [regex]::Escape((Civ $u0.AddMinutes(30)).ToString('HH:mm')))"
    }
    Section "register"
    $hours = -60, 0, 60
    $shapes = [ordered]@{
        'rep'   = { param($h) Rep (Boundary $l0.AddMinutes($h)) 'PT15M' 'PT45M' }
        'prog'  = { param($h) Rep (Boundary $l0.AddMinutes($h + 5)) 'PT5M' 'PT5M' }
        'min'   = { param($h) Rep (Boundary $l0.AddMinutes($h)) 'PT1M' 'PT59M' }
        'lone'  = { param($h) Daily (Boundary $l0.AddMinutes($h + 30)) }
        'zero'  = { param($h) Daily (Boundary $l0.AddMinutes($h)) }
        'rep15' = { param($h) Rep (Boundary $l0.AddMinutes($h + 15)) 'PT15M' 'PT30M' }
        'rep1'  = { param($h) Rep (Boundary $l0.AddMinutes($h + 1)) 'PT1M' 'PT58M' }
        'p2'    = { param($h) Rep (Boundary $l0.AddMinutes($h)) 'PT2M' 'PT58M' }
        'p5'    = { param($h) Rep (Boundary $l0.AddMinutes($h)) 'PT5M' 'PT55M' }
        'p10'   = { param($h) Rep (Boundary $l0.AddMinutes($h)) 'PT10M' 'PT50M' }
        'p30'   = { param($h) Rep (Boundary $l0.AddMinutes($h)) 'PT30M' 'PT30M' }
    }
    $mlog = "$Logs\dst-marker.log"; Remove-Item $mlog -ErrorAction SilentlyContinue
    $markerReal = $endReal.AddMinutes(1)
    foreach ($s in $shapes.Keys) {
        $trig = ($hours | ForEach-Object { & $shapes[$s] $_ }) -join ''
        $null = Reg $s $trig
    }
    $mxml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $mlog 0" -Triggers "<TimeTrigger><StartBoundary>$($markerReal.ToString('yyyy-MM-ddTHH:mm:00'))Z</StartBoundary><Enabled>true</Enabled></TimeTrigger>" -Policy 'Parallel'
    $null = Register-ProbeTask $folder 'marker' $mxml $P
    $regAt = [DateTime]::UtcNow
    "registered at $($regAt.ToString('o')) UTC; the change is $([Math]::Round(($u0 - $regAt).TotalMinutes, 1)) min away; marker due at $($markerReal.ToString('HH:mm')) UTC"
    Listing 'marker' $day $nextDay
    Section "GetRunTimes of the change day, as registered"
    foreach ($s in 'min', 'rep1', 'rep', 'lone') { Listing $s $day $nextDay }
    foreach ($s in $shapes.Keys) { Info ($folder.GetTask($s)) $s }

    # The marker's line is the real condition; progress every five minutes.
    $lastP = [DateTime]::MinValue
    while (-not ((Test-Path $mlog) -and @(Get-Content $mlog -ErrorAction SilentlyContinue).Count -ge 1)) {
        if ([DateTime]::UtcNow - $lastP -gt [TimeSpan]::FromMinutes(5)) {
            $lastP = [DateTime]::UtcNow
            "progress $($lastP.ToString('HH:mm:ss')) UTC: " + (($shapes.Keys | ForEach-Object { "$_=$(@(Get-Content "$Logs\dst-$_.log" -ErrorAction SilentlyContinue).Count)" }) -join ' ')
        }
        Start-Sleep -Seconds 5
    }
    "marker fired; clock $([DateTime]::UtcNow.ToString('o')) UTC"
    Zone-Line 'zone at the end'

    Section "runs per shape: per real hour relative to U0 (-1 is [U0-60 min, U0)); runs before A starts at $($aStart.ToString('HH:mm')) UTC are marked extra"
    foreach ($s in $shapes.Keys) {
        $ls = @(Get-Content "$Logs\dst-$s.log" -ErrorAction SilentlyContinue)
        $buckets = @{}
        $extra = 0
        $times = foreach ($l in $ls) {
            if ($l -match ' t=(\d+)') {
                $u = [DateTime]::FromFileTimeUtc([long]$Matches[1])
                $k = [int][Math]::Floor(($u - $u0).TotalMinutes / 60)
                $buckets[$k] = 1 + [int]$buckets[$k]
                if ($u -lt $aStart) { $extra++ }
                $u.ToString('HH:mm:ss')
            }
        }
        $per = ($buckets.Keys | Sort-Object | ForEach-Object { "real-hour $_ => $($buckets[$_])" }) -join '; '
        "{0}: runs={1} extra-before-A={2} [{3}]" -f $s, $ls.Count, $extra, $per
        "    " + ($times -join ' ')
    }
}
finally {
    if ($zoneChange) { tzutil /s $orig }
    Remove-ProbeTasks $svc
    Remove-Item "$zoneRoot\$zoneKey" -Recurse -ErrorAction SilentlyContinue
    if ($zoneChange) { Zone-Line "zone restored (was $orig)" }
}
exit $exit
