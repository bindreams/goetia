# M2 addendum: GetRunTimes on a runner whose zone is not UTC. Are the window it is given and the times it returns local civil
# times or UTC? Sets the zone to US Eastern (DST), measures, restores. Throwaway Windows CI runner only.
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc
$orig = (tzutil /g)
"zone before: $orig"
tzutil /s "Eastern Standard Time"
"zone now: $(tzutil /g); local time now: $((Get-Date).ToString('o')); UTC now: $([DateTime]::UtcNow.ToString('o'))"

function Cal([string]$start) { "<CalendarTrigger><StartBoundary>$start</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }
$cases = [ordered]@{
    'daily 03:30 local, window 2027-03-12..2027-03-17 (spring forward on 03-14)' = @{ T = (Cal '2027-03-01T03:30:00'); From = '2027-03-12'; To = '2027-03-17' }
    'daily 02:30 local (a time that does not exist on 2027-03-14)'              = @{ T = (Cal '2027-03-01T02:30:00'); From = '2027-03-12'; To = '2027-03-17' }
    'daily 01:30 local, window 2027-11-05..2027-11-10 (fall back on 11-07)'      = @{ T = (Cal '2027-11-01T01:30:00'); From = '2027-11-05'; To = '2027-11-10' }
    'daily 03:30 local, window across no change 2027-06-10..2027-06-13'           = @{ T = (Cal '2027-06-01T03:30:00'); From = '2027-06-10'; To = '2027-06-13' }
    'daily 21:00 local, window 2027-06-10..2027-06-11 (the end at local 00:00 is 04:00 UTC; 21:00 local on 06-10 is 01:00 UTC on 06-11)' = @{ T = (Cal '2027-06-01T21:00:00'); From = '2027-06-10'; To = '2027-06-11' }
    'daily 21:00 local, window 2027-06-09..2027-06-10' = @{ T = (Cal '2027-06-01T21:00:00'); From = '2027-06-09'; To = '2027-06-10' }
    'daily 21:00 local, window 2027-06-11..2027-06-12' = @{ T = (Cal '2027-06-01T21:00:00'); From = '2027-06-11'; To = '2027-06-12' }
    'daily with StartBoundary in UTC (2027-06-01T07:30:00Z), same window'         = @{ T = (Cal '2027-06-01T07:30:00Z'); From = '2027-06-10'; To = '2027-06-13' }
    'daily with StartBoundary at an offset (2027-06-01T03:30:00-04:00), same window' = @{ T = (Cal '2027-06-01T03:30:00-04:00'); From = '2027-06-10'; To = '2027-06-13' }
}
$n = 0
try {
    foreach ($k in $cases.Keys) {
        $n++
        Section "case $n : $k"
        try {
            $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop' -Triggers $cases[$k].T
            $t = Register-ProbeTask $folder "tz$n" $xml $P
            "registered; NextRunTime=$($t.NextRunTime.ToString('o'))"
            & $Script:Exe runtimes "$Script:TaskFolder\tz$n" $cases[$k].From $cases[$k].To 1000 | Where-Object { $_ -match '^(count|first|last)' }
        } catch { "REFUSED: $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
    }
}
finally {
    tzutil /s $orig
    "zone restored: $(tzutil /g)"
}
Remove-ProbeTasks $svc
exit 0
