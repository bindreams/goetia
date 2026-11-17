# Does a Task Scheduler repetition count elapsed time or local civil time across a DST change? GetRunTimes over windows
# that contain each change of US Pacific, for the two shapes goetia compiles a repetition to. Throwaway Windows CI runner only.
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc
$orig = (tzutil /g)
"zone before: $orig"
tzutil /s "Pacific Standard Time"
"zone now: $(tzutil /g)"

function Cal([string]$interval, [string]$duration, [bool]$stop = $false) {
    $s = "$stop".ToLower()
    "<CalendarTrigger><StartBoundary>2027-01-01T01:00:00</StartBoundary><Enabled>true</Enabled><Repetition><Interval>$interval</Interval><Duration>$duration</Duration><StopAtDurationEnd>$s</StopAtDurationEnd></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>"
}
# What goetia compiles `*/15 1-3 * * *` and `0 1,3,5 * * *` to.
$cases = [ordered]@{
    'min15 (01:00 every 15 min for PT165M)'          = @{ T = (Cal 'PT15M' 'PT165M'); }
    'min15 StopAtDurationEnd (01:00 every 15 min for PT165M)' = @{ T = (Cal 'PT15M' 'PT165M' $true); }
    'hours2 (01:00 every PT120M for PT240M)'         = @{ T = (Cal 'PT120M' 'PT240M'); }
}
$windows = [ordered]@{
    'spring forward 2027-03-14 02:00 -> 03:00' = @('2027-03-12', '2027-03-17')
    'fall back 2027-11-07 02:00 -> 01:00'      = @('2027-11-05', '2027-11-10')
    'no change 2027-06-10'                     = @('2027-06-10', '2027-06-12')
}
$env:TSPROBE_ALL = '1'
$n = 0
try {
    foreach ($k in $cases.Keys) {
        $n++
        $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop' -Triggers $cases[$k].T
        try { $t = Register-ProbeTask $folder "dst$n" $xml $P } catch { Section "case ${n}: $k"; "REFUSED: $($_.Exception.Message)"; continue }
        foreach ($w in $windows.Keys) {
            Section "case ${n}: $k ; $w"
            $lines = @(& $Script:Exe runtimes "$Script:TaskFolder\dst$n" $windows[$w][0] $windows[$w][1] 100000 | Where-Object { $_ -match '^t ' })
            $byDay = [ordered]@{}
            foreach ($l in $lines) {
                $day = $l.Substring(2, 10); $time = $l.Substring(13, 5)
                if (-not $byDay.Contains($day)) { $byDay[$day] = [System.Collections.Generic.List[string]]::new() }
                $byDay[$day].Add($time)
            }
            foreach ($day in $byDay.Keys) { "$day [$($byDay[$day].Count)]: $($byDay[$day] -join ' ')" }
        }
    }
}
finally {
    tzutil /s $orig
    "zone restored: $(tzutil /g)"
}
Remove-ProbeTasks $svc
exit 0
