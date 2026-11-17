# M3: changing the armed state (a trigger's Enabled flag) of a task registered with a stored password.
# Do the stored credentials survive a re-registration that carries no password? Does schtasks /change reach trigger flags?
# Throwaway Windows CI runner only.
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$user = New-ProbeUser 'gprobem3'
$P = Get-Principal 'user' $user
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc
$trig = '<CalendarTrigger><StartBoundary>2030-01-01T03:30:00</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>'
$out = "$Logs\m3-self.txt"

function Fresh([string]$Name) {
    # The task as goetia would register it: XML, password logon.
    $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act self $out noenv" -Triggers $trig
    Register-ProbeTask $folder $Name $xml $P
}
function TrigFlags($t) { (@($t.Definition.Triggers) | ForEach-Object { $_.Enabled }) -join ',' }
function Principal($t) { $p = $t.Definition.Principal; "UserId=$($p.UserId) LogonType=$($p.LogonType) RunLevel=$($p.RunLevel)" }
# Does a Run still work as the stored user? (the action reports the token's user sid.)
function Try-Run($t, [string]$Label) {
    Remove-Item $out -ErrorAction SilentlyContinue
    $e = Run-Wait $t
    $sid = (Get-Content $out -ErrorAction SilentlyContinue | Where-Object { $_ -like 'token-user-sid=*' })
    "  Run after '$Label': $(if ($e) { $e } else { 'Run returned ok' }); the action ran: $([bool]$sid) $sid; LastTaskResult=$(HexOf $t.LastTaskResult) LastRunTime=$($t.LastRunTime.ToString('o'))"
}
function Attempt([string]$Label, [scriptblock]$Do, [string]$Name) {
    Section $Label
    $t = Fresh $Name
    "before: trigger Enabled flags [$(TrigFlags $t)]; $(Principal $t)"
    try {
        & $Do $t
        "  registration: ok"
    } catch { "  registration: FAILED $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
    $t2 = $folder.GetTask($Name)
    "after:  trigger Enabled flags [$(TrigFlags $t2)]; $(Principal $t2)"
    Try-Run $t2 $Label
}

Section "baseline: the task as registered with its password"
$t = Fresh 'base'
"flags [$(TrigFlags $t)]; $(Principal $t)"
Try-Run $t 'registration with the password'

# 1. Definition edited, re-registered with TASK_UPDATE (4), no user, no password, TASK_LOGON_NONE (0)
Attempt "Definition edited; RegisterTaskDefinition(TASK_UPDATE, null user, null password, TASK_LOGON_NONE)" {
    param($t) $d = $t.Definition; $d.Triggers.Item(1).Enabled = $false
    $null = $folder.RegisterTaskDefinition('m3a', $d, 4, $null, $null, 0)
} 'm3a'

# 2. same with TASK_CREATE_OR_UPDATE (6)
Attempt "Definition edited; RegisterTaskDefinition(TASK_CREATE_OR_UPDATE, null user, null password, TASK_LOGON_NONE)" {
    param($t) $d = $t.Definition; $d.Triggers.Item(1).Enabled = $false
    $null = $folder.RegisterTaskDefinition('m3b', $d, 6, $null, $null, 0)
} 'm3b'

# 3. with the user but no password, TASK_LOGON_PASSWORD (1)
Attempt "Definition edited; RegisterTaskDefinition(TASK_UPDATE, user, null password, TASK_LOGON_PASSWORD)" {
    param($t) $d = $t.Definition; $d.Triggers.Item(1).Enabled = $false
    $null = $folder.RegisterTaskDefinition('m3c', $d, 4, $user.UserId, $null, 1)
} 'm3c'

# 4. with user and password, TASK_LOGON_PASSWORD
Attempt "Definition edited; RegisterTaskDefinition(TASK_UPDATE, user, password, TASK_LOGON_PASSWORD)" {
    param($t) $d = $t.Definition; $d.Triggers.Item(1).Enabled = $false
    $null = $folder.RegisterTaskDefinition('m3d', $d, 4, $user.UserId, $user.Password, 1)
} 'm3d'

# 5. the task's XML edited and registered with RegisterTask, TASK_UPDATE, null user and password, TASK_LOGON_NONE
Attempt "Xml edited; RegisterTask(TASK_UPDATE, null user, null password, TASK_LOGON_NONE)" {
    param($t) $x = $t.Xml -replace '<Enabled>true</Enabled>(\s*<ScheduleByDay>)', '<Enabled>false</Enabled>$1'
    $null = $folder.RegisterTask('m3e', $x, 4, $null, $null, 0, $null)
} 'm3e'

# 6. same, TASK_LOGON_PASSWORD with the user, null password
Attempt "Xml edited; RegisterTask(TASK_UPDATE, user, null password, TASK_LOGON_PASSWORD)" {
    param($t) $x = $t.Xml -replace '<Enabled>true</Enabled>(\s*<ScheduleByDay>)', '<Enabled>false</Enabled>$1'
    $null = $folder.RegisterTask('m3f', $x, 4, $user.UserId, $null, 1, $null)
} 'm3f'

# 7. same, with password
Attempt "Xml edited; RegisterTask(TASK_UPDATE, user, password, TASK_LOGON_PASSWORD)" {
    param($t) $x = $t.Xml -replace '<Enabled>true</Enabled>(\s*<ScheduleByDay>)', '<Enabled>false</Enabled>$1'
    $null = $folder.RegisterTask('m3g', $x, 4, $user.UserId, $user.Password, 1, $null)
} 'm3g'

# 8. The password changed behind the task's back: do stored credentials follow? (control: the stored one is stale)
Section "control: the account's password is changed, then a null-password update, then Run (is the stored one really used?)"
$t = Fresh 'm3h'
Try-Run $t 'a first Run, the password still right'
net user $user.Name 'Pb1!changedXYZ12' | Out-Null
Try-Run $t 'the account password changed after registration (the stored one is now stale)'
$d = $t.Definition; $d.Triggers.Item(1).Enabled = $false
try { $null = $folder.RegisterTaskDefinition('m3h', $d, 4, $null, $null, 0); "  null-password update: ok" } catch { "  null-password update: FAILED $($_.Exception.Message)" }
Try-Run $folder.GetTask('m3h') 'null-password update'
net user $user.Name $user.Password | Out-Null

# 9. schtasks /change
Section "schtasks /change: what does it reach?"
$t = Fresh 'm3s'
"before: flags [$(TrigFlags $t)] task Enabled=$($t.Enabled)"
$tn = "$Script:TaskFolder\m3s"
foreach ($a in @('/disable'), @('/enable'), @('/st', '04:00'), @('/ri', '10', '/du', '01:00'), @('/ed', '2031-01-01'), @('/sd', '2030-02-02')) {
    $o = (schtasks /change /tn $tn @a 2>&1) -join ' '
    $t2 = $folder.GetTask('m3s')
    "schtasks /change $($a -join ' '): exit $LASTEXITCODE '$o' => flags [$(TrigFlags $t2)] task Enabled=$($t2.Enabled); $(Principal $t2)"
}
Try-Run $folder.GetTask('m3s') 'schtasks /change calls'
$o = (schtasks /change /tn $tn /ru $user.Qualified /rp $user.Password 2>&1) -join ' '
"schtasks /change /ru /rp: exit $LASTEXITCODE '$o'"

Remove-ProbeTasks $svc
Remove-ProbeUser $user
