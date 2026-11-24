# Question 1a: does Task Scheduler list a run as running from the moment it launches it? Driver of `tsprobe appear`.
#   q1-listed.ps1 -Kind system|user [-RunRounds 200] [-FireRounds 40]
# Run mode: `IRegisteredTask::Run` on an idle task, RunRounds times. Fire mode: a daily trigger repeating every minute all day, FireRounds
# consecutive ticks. Details of what is sampled and printed: src/appear.rs. Throwaway Windows CI runner only.
param(
    [Parameter(Mandatory)][ValidateSet('system', 'user')][string]$Kind,
    [int]$RunRounds = 200,
    [int]$FireRounds = 40
)
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$user = $null
try {
    if ($Kind -eq 'user') { $user = New-ProbeUser 'gappear' }
    $P = Get-Principal $Kind $user
    "principal: $Kind ($($P.UserId))"
    $svc = Get-ProbeService
    Remove-ProbeTasks $svc
    $folder = Get-ProbeFolder $svc

    $logR = "$Logs\appear-run.log"
    $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $logR 0 sleep_ms=1500"
    $t = Register-ProbeTask $folder 'apr' $xml $P
    # A new local user's first Run builds its profile: one run not counted.
    Section "warm-up"
    Run-Wait $t
    Remove-Item $logR -ErrorAction SilentlyContinue

    Section "run mode ($Kind), $RunRounds rounds"
    & $Script:Exe appear "$Script:TaskFolder\apr" $logR run $RunRounds "$Kind/run" *>&1 | ForEach-Object { "$_" }
    "exit=$LASTEXITCODE"

    Section "fire mode ($Kind), $FireRounds ticks"
    $logF = "$Logs\appear-fire.log"
    $trig = "<CalendarTrigger><StartBoundary>2000-01-01T00:00:00</StartBoundary><Enabled>true</Enabled><Repetition><Interval>PT1M</Interval><Duration>PT23H59M</Duration></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>"
    $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $logF 0 sleep_ms=1500" -Triggers $trig
    $null = Register-ProbeTask $folder 'apf' $xml $P
    "registered at $([DateTime]::UtcNow.ToString('o'))"
    & $Script:Exe appear "$Script:TaskFolder\apf" $logF fire $FireRounds "$Kind/fire" *>&1 | ForEach-Object { "$_" }
    "exit=$LASTEXITCODE"
}
finally {
    Remove-ProbeTasks $svc
    Remove-ProbeUser $user
}
exit 0
