# M10: availability and access. Folder creation, its default ACL, whether an unelevated principal can reach its own task
# inside it, the error codes of missing/existing paths, deleting the last task and the folder, and (last, in a
# throwaway VM) whether the Schedule service can be stopped. Throwaway Windows CI runner only.
param([switch]$ServiceStop)
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$svc = Get-ProbeService
$F = '\Goetia'
function Try-It([string]$Label, [scriptblock]$Do) { try { $r = & $Do; "$Label : ok $r" } catch { "$Label : FAILED $($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" } }

if (-not $ServiceStop) {
    $admin = New-ProbeUser 'gprobem10a' -Admin
    $user = New-ProbeUser 'gprobem10u'
    $PA = Get-Principal 'user' $admin; $PU = Get-Principal 'user' $user; $PL = Get-Principal 'localservice'
    try { $svc.GetFolder($F).GetTasks(1) | ForEach-Object { $svc.GetFolder($F).DeleteTask($_.Name, 0) }; $svc.GetFolder('\').DeleteFolder('Goetia', 0) } catch {}

    Section "M10.1 a folder that does not exist"
    Try-It "GetFolder($F) before it exists" { $svc.GetFolder($F) }
    Try-It "GetFolder(\Goetia\sub) (a missing child)" { $svc.GetFolder('\Goetia\sub') }
    Try-It "GetTask on a missing folder" { $svc.GetFolder($F).GetTask('x') }

    Section "M10.2 creating the folder as the elevated administrator, its default ACL"
    Try-It "CreateFolder(Goetia)" { $null = $svc.GetFolder('\').CreateFolder('Goetia'); 'created' }
    $folder = $svc.GetFolder($F)
    Try-It "SDDL of the folder (owner, group, DACL)" { $folder.GetSecurityDescriptor(0x7) }
    Try-It "a second CreateFolder(Goetia)" { $null = $svc.GetFolder('\').CreateFolder('Goetia'); 'created again' }
    Try-It "CreateFolder with the name in another case (GOETIA)" { $null = $svc.GetFolder('\').CreateFolder('GOETIA'); 'created' }
    "the folder as a directory on disk: $(icacls C:\Windows\System32\Tasks\Goetia | Out-String)"

    Section "M10.3 RegisterTask: TASK_CREATE on an existing path, TASK_UPDATE on a missing one"
    $xml = New-TaskXml -P (Get-Principal 'system') -Command $Script:Exe -Arguments 'act noop'
    Try-It "TASK_CREATE (2) of a new task" { $null = $folder.RegisterTask('exists', $xml, 2, $null, $null, 0, $null); 'created' }
    Try-It "TASK_CREATE (2) of the same path again" { $null = $folder.RegisterTask('exists', $xml, 2, $null, $null, 0, $null); 'created' }
    Try-It "TASK_CREATE (2) of the same path in another case" { $null = $folder.RegisterTask('EXISTS', $xml, 2, $null, $null, 0, $null); 'created' }
    Try-It "TASK_UPDATE (4) of a missing path" { $null = $folder.RegisterTask('missing', $xml, 4, $null, $null, 0, $null); 'updated' }
    Try-It "TASK_UPDATE (4) of an existing path" { $null = $folder.RegisterTask('exists', $xml, 4, $null, $null, 0, $null); 'updated' }
    Try-It "TASK_CREATE_OR_UPDATE (6) of a missing path" { $null = $folder.RegisterTask('newcou', $xml, 6, $null, $null, 0, $null); 'ok' }
    Try-It "GetTask of a missing task in an existing folder" { $folder.GetTask('nosuchtask') }
    Try-It "DeleteTask of a missing task" { $folder.DeleteTask('nosuchtask', 0); 'deleted' }

    Section "M10.4 deleting the last task of the folder, and then the folder"
    "tasks now: $(@($folder.GetTasks(1) | ForEach-Object { $_.Name }) -join ', ')"
    Try-It "DeleteFolder while it still holds tasks" { $svc.GetFolder('\').DeleteFolder('Goetia', 0); 'deleted' }
    foreach ($n in @($folder.GetTasks(1) | ForEach-Object { $_.Name })) {
        $folder.DeleteTask($n, 0)
        "deleted task $n; folder still readable: $(try { $null = $svc.GetFolder($F); 'yes' } catch { 'NO: ' + $_.Exception.Message.Trim() }); tasks left $(@($svc.GetFolder($F).GetTasks(1)).Count)"
    }
    "the folder on disk after the last task went: $(Test-Path C:\Windows\System32\Tasks\Goetia)"
    Try-It "DeleteFolder of the now empty folder" { $svc.GetFolder('\').DeleteFolder('Goetia', 0); 'deleted' }
    Try-It "GetFolder after DeleteFolder" { $svc.GetFolder($F) }
    Try-It "DeleteFolder of a missing folder" { $svc.GetFolder('\').DeleteFolder('Goetia', 0); 'deleted' }

    Section "M10.5 an unelevated principal reaches its own task inside the folder (default ACL), and what else it can do there"
    $null = $svc.GetFolder('\').CreateFolder('Goetia')
    $folder = $svc.GetFolder($F)
    foreach ($case in @(
            @{ N = 'ls';   P = $PL;  R = 'LeastPrivilege';     D = 'LocalService' },
            @{ N = 'adm';  P = $PA;  R = 'LeastPrivilege';     D = 'a local administrator, LeastPrivilege (unelevated token)' },
            @{ N = 'adm2'; P = $PA;  R = 'HighestAvailable';   D = 'a local administrator, HighestAvailable (elevated token)' },
            @{ N = 'usr';  P = $PU;  R = 'LeastPrivilege';     D = 'a standard user' })) {
        Section "own-task reads as $($case.D)"
        $out = "$Logs\m10-$($case.N).txt"; Remove-Item $out -ErrorAction SilentlyContinue
        $xml = New-TaskXml -P $case.P -Command $Script:Exe -Arguments "act self $out noenv task=$F\$($case.N)" -RunLevel $case.R -DataXml '<Data>GOETIA-M10-DATA</Data>'
        $t = Register-ProbeTask $folder $case.N $xml $case.P
        $rt = $t.Run($null); Wait-NoInstances $t
        "LastTaskResult=$(HexOf $t.LastTaskResult)"
        Get-Content $out -ErrorAction SilentlyContinue | Where-Object { $_ -match '^(token-|com:)' }
    }

    Section "M10.6 the same with the folder's DACL restricted to SYSTEM and Administrators (SetSecurityDescriptor)"
    Try-It "SetSecurityDescriptor" { $folder.SetSecurityDescriptor('D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)', 0); 'set' }
    Try-It "SDDL now" { $folder.GetSecurityDescriptor(0x7) }
    foreach ($case in @(@{ N = 'ls'; D = 'LocalService' }, @{ N = 'usr'; D = 'a standard user' })) {
        Section "own-task reads as $($case.D), folder restricted"
        $out = "$Logs\m10r-$($case.N).txt"; Remove-Item $out -ErrorAction SilentlyContinue
        $t = $folder.GetTask($case.N)
        $xml = $t.Xml -replace '(<Arguments>)[^<]*(</Arguments>)', "`$1act self $out noenv task=$F\$($case.N)`$2"
        $t = Register-ProbeTask $folder $case.N $xml (Get-Principal $(if ($case.N -eq 'ls') { 'localservice' } else { 'user' }) $user) 4
        $rt = $t.Run($null); Wait-NoInstances $t
        "LastTaskResult=$(HexOf $t.LastTaskResult)"
        Get-Content $out -ErrorAction SilentlyContinue | Where-Object { $_ -match '^(token-user|com:)' }
    }

    Remove-ProbeTasks $svc
    try { $f = $svc.GetFolder($F); foreach ($t in @($f.GetTasks(1))) { $f.DeleteTask($t.Name, 0) }; $svc.GetFolder('\').DeleteFolder('Goetia', 0) } catch {}
    Remove-ProbeUser $admin; Remove-ProbeUser $user
}
else {
    Section "M10.7 the Schedule service: can an administrator stop it, disable it? What does Connect say then?"
    $q = { sc.exe queryex Schedule | Out-String }
    "state before: $(& $q)"
    "service config before: $(sc.exe qc Schedule | Out-String)"
    "sc stop Schedule (elevated administrator): $(sc.exe stop Schedule 2>&1 | Out-String)"
    "Stop-Service: $(try { Stop-Service Schedule -Force -ErrorAction Stop; 'ok' } catch { 'FAILED ' + $_.Exception.Message })"
    "state after: $(& $q)"
    "sc config Schedule start= disabled: $(sc.exe config Schedule start= disabled 2>&1 | Out-String)"
    "Connect() now:"
    try { $s = New-Object -ComObject Schedule.Service; $s.Connect(); "  Connect ok (the service answers: $((sc.exe query Schedule | Select-String STATE) -join ''))" } catch { "  Connect FAILED $($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" }
    try { $s = New-Object -ComObject Schedule.Service; $s.Connect(); $null = $s.GetFolder('\'); "  GetFolder ok" } catch { "  GetFolder FAILED $($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" }
    "state after Connect: $(& $q)"
    # As SYSTEM, through a task of its own: does SYSTEM manage what the administrator could not?
    "restore: $(sc.exe config Schedule start= auto 2>&1 | Out-String) $(sc.exe start Schedule 2>&1 | Out-String)"
    "state at the end: $(& $q)"
    # SYSTEM's attempt
    $P = Get-Principal 'system'
    $svc = Get-ProbeService
    $folder = Get-ProbeFolder $svc
    $out = "$Logs\m10-systemstop.txt"; Remove-Item $out -ErrorAction SilentlyContinue
    $xml = New-TaskXml -P $P -Command 'C:\Windows\System32\cmd.exe' -Arguments "/c sc stop Schedule > $out 2>&1 & sc query Schedule >> $out 2>&1"
    $t = Register-ProbeTask $folder 'sysstop' $xml $P
    $null = $t.Run($null)
    Wait-Lines $out 1
    "SYSTEM's own sc stop Schedule, from a task:"; Get-Content $out
    "state afterwards: $(& $q)"
    "restore: $(sc.exe start Schedule 2>&1 | Out-String)"
}
