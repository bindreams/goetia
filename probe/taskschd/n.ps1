# N1-N4: the task-level Enabled flag (IRegisteredTask.Enabled = false, put_Enabled) as the first step of an uninstall: no
# registration, so no password. Throwaway Windows CI runner only.
#   N1  put_Enabled(false) with no password is accepted on a password-logon task; Xml then reads Settings/Enabled false and
#       the triggers' own Enabled flags are unchanged.
#   N2  put_Enabled(false) then DeleteTask, both with no password, work after the account's password changed, and after
#       the account was deleted.
#   N3  with the task-level flag false and an enabled trigger due inside the observation window, nothing starts (a control
#       task with the flag true starts once, so the window is long enough to see a start).
#   N4  put_Enabled(false) does not end a live instance, and IRunningTask.Stop of that instance by GUID still stops it.
#
# A "starts nothing" result is a negative observation: it needs a stated observation window. $Watch is that window (a
# measurement parameter, not a synchronisation); every positive result is read from the action's own log, and the
# waits for the due time and for a process are on the real condition.
param([Parameter(Mandatory)][ValidateSet('system', 'user')][string]$Kind)
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$Watch = 30
$user = $null
if ($Kind -eq 'user') { $user = New-ProbeUser 'gproben' }
$P = Get-Principal $Kind $user
Write-Host "principal: $Kind ($($P.UserId))"
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

function Daily([string]$start, [bool]$en = $true) { "<CalendarTrigger><StartBoundary>$start</StartBoundary><Enabled>$("$en".ToLower())</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }
function Once([string]$start) { "<TimeTrigger><StartBoundary>$start</StartBoundary><Enabled>true</Enabled></TimeTrigger>" }
function Reg([string]$Name, [string]$ActArgs, [string]$Trig = '', [hashtable]$Who = $P) {
    $xml = New-TaskXml -P $Who -Command $Script:Exe -Arguments $ActArgs -Triggers $Trig
    Register-ProbeTask $folder $Name $xml $Who
}
function TrigFlags($t) { (@($t.Definition.Triggers) | ForEach-Object { $_.Enabled }) -join ',' }
function SettingsEnabled($t) { $m = [regex]::Matches(([regex]::Match($t.Xml, '(?s)<Settings>.*?</Settings>')).Value, '<Enabled>[^<]*</Enabled>'); if ($m.Count) { ($m | ForEach-Object { $_.Value }) -join '' } else { '(no Enabled element: the default, true)' } }
function Show([string]$Label, $t) { "{0}: Task.Enabled={1}; Xml Settings: {2}; trigger flags [{3}]; NextRunTime={4}" -f $Label, $t.Enabled, (SettingsEnabled $t), (TrigFlags $t), $t.NextRunTime.ToString('o') }
function Disable-NoPassword($t) {
    try { $t.Enabled = $false; "  put_Enabled(false) with no password: accepted" } catch { "  put_Enabled(false) with no password: FAILED $($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" }
}
function Delete-NoPassword([string]$Name) {
    try { $folder.DeleteTask($Name, 0); "  DeleteTask($Name) with no password: accepted" } catch { "  DeleteTask($Name) FAILED $($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" }
    "  still listed: $([bool](@($folder.GetTasks(1)) | Where-Object { $_.Name -eq $Name }))"
}
$far = '2030-01-01T03:30:00'

try {
    Section "N1 put_Enabled(false) with no password on a $Kind task with an enabled trigger"
    $t = Reg 'n1' "act rec $Logs\n1.log 0" (Daily $far)
    Show 'registered' $t
    Disable-NoPassword $t
    Show 'after, same object' $t
    Show 'after, read afresh' ($folder.GetTask('n1'))
    $t2 = $folder.GetTask('n1')
    try { $t2.Enabled = $true; "  put_Enabled(true): accepted" } catch { "  put_Enabled(true) FAILED $($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" }
    Show 'after put_Enabled(true)' ($folder.GetTask('n1'))
    Delete-NoPassword 'n1'

    if ($Kind -eq 'user') {
        Section "N2a put_Enabled(false) then DeleteTask, with no password, after the account's password changed"
        $t = Reg 'n2a' "act rec $Logs\n2a.log 0" (Daily $far)
        net user $user.Name ('Pc2!' + [guid]::NewGuid().ToString('N').Substring(0, 10)) | Out-Null
        "  the account's password is changed (the stored one is stale)"
        $t = $folder.GetTask('n2a')
        Disable-NoPassword $t
        Show 'after' ($folder.GetTask('n2a'))
        Delete-NoPassword 'n2a'

        Section "N2b put_Enabled(false) then DeleteTask, with no password, after the account was deleted"
        $gone = New-ProbeUser 'gprobend'
        $Pg = Get-Principal 'user' $gone
        $t = Reg 'n2b' "act rec $Logs\n2b.log 0" (Daily $far) $Pg
        Show 'registered' $t
        Remove-ProbeUser $gone
        "  the account is deleted"
        $t = $folder.GetTask('n2b')
        Disable-NoPassword $t
        Show 'after' ($folder.GetTask('n2b'))
        Delete-NoPassword 'n2b'
    }

    Section "N3 a due trigger under a task-level false flag starts nothing ($Kind)"
    $d = (Get-Date).AddSeconds(75)
    $due = [datetime]::new($d.Year, $d.Month, $d.Day, $d.Hour, $d.Minute, 0)
    $dueText = $due.ToString('s')
    "now (local) = $((Get-Date).ToString('o')); the one-time triggers are due at $dueText"
    $logOff = "$Logs\n3-off.log"; $logCtl = "$Logs\n3-control.log"
    Remove-Item $logOff, $logCtl -ErrorAction SilentlyContinue
    $off = Reg 'n3-off' "act rec $logOff 0" (Once $dueText)
    $ctl = Reg 'n3-control' "act rec $logCtl 0" (Once $dueText)
    Disable-NoPassword $off
    Show 'n3-off' ($folder.GetTask('n3-off'))
    Show 'n3-control' ($folder.GetTask('n3-control'))
    # Waits for the control's own start (the positive observation), with no cap: the job's timeout is the failure bound.
    Wait-Match $logCtl 'pid='
    "the control started at $((Get-Date).ToString('o')); watching the disabled task for $Watch s more ..."
    Start-Sleep -Seconds $Watch
    "n3-control starts: $(@(Get-Content $logCtl -ErrorAction SilentlyContinue).Count)"
    "n3-off starts in the window: $(@(Get-Content $logOff -ErrorAction SilentlyContinue).Count)"
    Info ($folder.GetTask('n3-off')) 'n3-off values'
    Info ($folder.GetTask('n3-control')) 'n3-control values'

    Section "N4 put_Enabled(false) does not end a live instance; Stop by GUID still does ($Kind)"
    $log4 = "$Logs\n4.log"; Remove-Item $log4 -ErrorAction SilentlyContinue
    $t = Reg 'n4' "act ctrl $log4 block:0"
    $rt = $t.Run($null)
    Wait-Lines $log4 2
    $guid = $rt.InstanceGuid; $pid4 = [int]$rt.EnginePID
    "instance $guid pid $pid4 is running"
    $proc = [Diagnostics.Process]::GetProcessById($pid4); $null = $proc.Handle
    Disable-NoPassword $t
    "listing after put_Enabled(false): $((@($t.GetInstances(0)) | ForEach-Object { "$($_.InstanceGuid)/$($_.EnginePID)" }) -join ', ')"
    "process exited right after put_Enabled(false): $($proc.HasExited)"
    Start-Sleep -Seconds 5
    "process exited 5 s after put_Enabled(false): $($proc.HasExited)"
    Show 'task' ($folder.GetTask('n4'))
    $inst = @($t.GetInstances(0)) | Where-Object { $_.InstanceGuid -eq $guid }
    try { $inst.Stop(); "  IRunningTask.Stop of $guid : accepted" } catch { "  IRunningTask.Stop FAILED $($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" }
    $proc.WaitForExit()
    "process exited after Stop: $($proc.HasExited); exit code $($proc.ExitCode)"
    Wait-NoInstances $t
    Show 'task after the stop' ($folder.GetTask('n4'))
    try { $null = $t.Run($null); "  Run on the task-level disabled task: ok" } catch { "  Run on the task-level disabled task: FAILED $($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" }
    Delete-NoPassword 'n4'
}
finally {
    Remove-ProbeTasks $svc
    Remove-ProbeUser $user
}
