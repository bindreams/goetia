# M1: the values LastTaskResult, LastRunTime and NextRunTime take, as SYSTEM. Throwaway Windows CI runner only.
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe; Initialize-Native
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc
$future = '<TimeTrigger><StartBoundary>2030-01-01T03:30:00</StartBoundary><Enabled>true</Enabled></TimeTrigger>'
$futureOff = '<TimeTrigger><StartBoundary>2030-01-01T03:30:00</StartBoundary><Enabled>false</Enabled></TimeTrigger>'
$daily = '<CalendarTrigger><StartBoundary>2027-01-01T03:30:00</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>'
$dailyOff = '<CalendarTrigger><StartBoundary>2027-01-01T03:30:00</StartBoundary><Enabled>false</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>'

function Reg([string]$Name, [string]$ActArgs, [string]$Trig = '', [string]$Cmd = $Script:Exe, [bool]$Enabled = $true) {
    $xml = New-TaskXml -P $P -Command $Cmd -Arguments $ActArgs -Triggers $Trig -Enabled $Enabled
    Register-ProbeTask $folder $Name $xml $P
}
# Runs a short action to its end; reads the values at the process's exit and after the listing dropped.
function Run-To-End($t, [string]$Label) {
    $rt = $t.Run($null)
    $pid_ = $rt.EnginePID
    $p = [Diagnostics.Process]::GetProcessById([int]$pid_)
    $null = $p.Handle
    $p.WaitForExit()
    Info $t "$Label at the process's exit (exit code $($p.ExitCode) = $(HexOf $p.ExitCode))"
    Wait-NoInstances $t
    Info $t "$Label after the listing dropped"
}

Section "never run"
$t = Reg 'never-trig' "act rec $Logs\m1.log 0" $future; Info $t 'never run, one enabled future trigger'
$t = Reg 'never-notrig' "act rec $Logs\m1.log 0" '';    Info $t 'never run, no triggers'
$t = Reg 'never-alloff' "act rec $Logs\m1.log 0" $futureOff; Info $t 'never run, every trigger disabled'
$t = Reg 'never-daily' "act rec $Logs\m1.log 0" $daily; Info $t 'never run, an enabled daily trigger'
$t = Reg 'never-dailyoff' "act rec $Logs\m1.log 0" $dailyOff; Info $t 'never run, a disabled daily trigger'
$t = Reg 'never-taskoff' "act rec $Logs\m1.log 0" $daily '' $false; Info $t 'never run, the task itself disabled (Settings/Enabled false)'

Section "during a run"
Remove-Item "$Logs\m1-during.log" -ErrorAction SilentlyContinue
$t = Reg 'during' "act ctrl $Logs\m1-during.log block:0" $daily
Info $t 'before the run'
$rt = $t.Run($null)
Wait-Lines "$Logs\m1-during.log" 2
Info $t 'while it runs'
$proc = [Diagnostics.Process]::GetProcessById([int]$rt.EnginePID); $null = $proc.Handle
$rt.Stop(); $proc.WaitForExit(); Wait-NoInstances $t
Info $t 'after the run was stopped'

Section "after exit 0, 7 and 0xC000013A, a Run each"
foreach ($code in 0, 7, 3221225786) {
    $t = Reg "exit$code" "act rec $Logs\m1-exit$code.log $code" $daily
    Run-To-End $t "exit $code"
}

Section "cannot start (missing executable)"
$t = Reg 'nostart' '' $daily 'C:\probe\bin\missing.exe'
try {
    $rt = $t.Run($null)
    Write-Host "Run returned ok guid=$($rt.InstanceGuid) pid=$($rt.EnginePID) state=$($rt.State)"
    Wait-NoInstances $t
} catch { Write-Host "Run failed: $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
Info $t 'after Run of a missing exe'

Section "after the action's process was terminated (TerminateProcess code 0xC0DE0011)"
$t = Reg 'killed' "act ctrl $Logs\m1-killed.log block:0" $daily
Remove-Item "$Logs\m1-killed.log" -ErrorAction SilentlyContinue
$rt = $t.Run($null)
Wait-Lines "$Logs\m1-killed.log" 2
$pid_ = [uint32]$rt.EnginePID
$proc = [Diagnostics.Process]::GetProcessById([int]$pid_); $null = $proc.Handle
Info $t 'while the console action runs'
Write-Host "Terminate: win32 $([Native]::Terminate($pid_, [uint32]0xC0DE0011L))"
$proc.WaitForExit()
Info $t "at the process's exit (exit code $(HexOf $proc.ExitCode))"
Wait-NoInstances $t
Info $t 'after the listing dropped'

Section "after IRunningTask::Stop of a console action whose handler returns TRUE after 1 s"
Remove-Item "$Logs\m1-stop.log" -ErrorAction SilentlyContinue
$t = Reg 'stopped' "act ctrl $Logs\m1-stop.log block:1" $daily
$rt = $t.Run($null)
Wait-Lines "$Logs\m1-stop.log" 2
$proc = [Diagnostics.Process]::GetProcessById([int]$rt.EnginePID); $null = $proc.Handle
Info $t 'while the console action runs'
$rt.Stop()
$proc.WaitForExit()
Info $t "at the process's exit (exit code $(HexOf $proc.ExitCode))"
Wait-NoInstances $t
Info $t 'after the listing dropped'
Get-Content "$Logs\m1-stop.log"

Section "a task with a trigger, run on demand, then NextRunTime"
$t = Reg 'ondemand' "act rec $Logs\m1-od.log 0" $daily
Run-To-End $t 'daily trigger task after an on-demand run'
$t = Reg 'ondemand-notrig' "act rec $Logs\m1-od2.log 0" ''
Run-To-End $t 'no-trigger task after an on-demand run'
$t = Reg 'ondemand-off' "act rec $Logs\m1-od3.log 0" $dailyOff
Run-To-End $t 'disabled-trigger task after an on-demand run'

Remove-ProbeTasks $svc
