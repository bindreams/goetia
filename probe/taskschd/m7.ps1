# M7: a fixed past StartBoundary with StartWhenAvailable; flipping a trigger's Enabled; a one-time trigger at a past
# boundary; Run on a task whose triggers are all disabled, or that has none; IgnoreNew with Run against a live instance.
# Throwaway Windows CI runner only.
#
# A "fires nothing" result is a negative observation: it needs a stated observation window. $Watch is that window (a
# measurement parameter, not a synchronisation); every positive result is read from the action's own log.
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe; Initialize-Native
$Watch = 30
$P = Get-Principal 'system'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

$now = Get-Date
$tenAgo = $now.AddMinutes(-10)
$missedStart = $now.AddDays(-2).Date.Add($tenAgo.TimeOfDay).ToString('s')   # daily at (now - 10 min), begun two days ago
$pastFixed = '2020-01-01T00:00:00'
"now (local) = $($now.ToString('o')); the daily trigger of the 'missed' cases is at $($tenAgo.ToString('HH:mm:ss')) every day, begun $missedStart"

function Daily([string]$start, [bool]$en = $true) { "<CalendarTrigger><StartBoundary>$start</StartBoundary><Enabled>$("$en".ToLower())</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>" }
function Once([string]$start, [bool]$en = $true) { "<TimeTrigger><StartBoundary>$start</StartBoundary><Enabled>$("$en".ToLower())</Enabled></TimeTrigger>" }
function Reg([string]$Name, [string]$Trig, [bool]$Swa, [string]$Policy = 'IgnoreNew', [string]$Act = 'rec') {
    $log = "$Logs\m7-$Name.log"; Remove-Item $log -ErrorAction SilentlyContinue
    $args_ = if ($Act -eq 'rec') { "act rec $log 0" } else { "act ctrl $log block:0" }
    $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments $args_ -Triggers $Trig -StartWhenAvailable $Swa -Policy $Policy
    Register-ProbeTask $folder $Name $xml $P
}
function Starts([string]$Name) { @(Get-Content "$Logs\m7-$Name.log" -ErrorAction SilentlyContinue).Count }

Section "M7.1 registration with a past boundary: does anything fire within the $Watch s observation window?"
$cases = [ordered]@{
    'missed-swa'      = @{ T = (Daily $missedStart); S = $true;  D = 'daily at now-10min, begun 2 days ago, StartWhenAvailable=true' }
    'missed-noswa'    = @{ T = (Daily $missedStart); S = $false; D = 'the same, StartWhenAvailable=false (control)' }
    'fixedpast-swa'   = @{ T = (Daily $pastFixed);   S = $true;  D = "daily from $pastFixed, StartWhenAvailable=true" }
    'oncepast-swa'    = @{ T = (Once $pastFixed);    S = $true;  D = "one-time trigger at $pastFixed, StartWhenAvailable=true" }
    'oncepast-noswa'  = @{ T = (Once $pastFixed);    S = $false; D = "one-time trigger at $pastFixed, StartWhenAvailable=false" }
    'oncepast-disabled-swa' = @{ T = (Once $pastFixed $false); S = $true; D = 'one-time past trigger registered disabled' }
    'fixedpast-disabled-swa' = @{ T = (Daily $pastFixed $false); S = $true; D = 'daily past trigger registered disabled' }
}
foreach ($k in $cases.Keys) {
    $t = Reg $k $cases[$k].T $cases[$k].S
    Info $t "$k ($($cases[$k].D)) at registration"
}
Write-Host "observing for $Watch s ..."
Start-Sleep -Seconds $Watch
foreach ($k in $cases.Keys) {
    $t = $folder.GetTask($k)
    "{0}: action starts in the window = {1}; {2}" -f $k, (Starts $k), (Info $t 'values')
}

Section "M7.2 flipping a trigger's Enabled from false to true (a past boundary, StartWhenAvailable=true)"
foreach ($k in 'oncepast-disabled-swa', 'fixedpast-disabled-swa') {
    $t = $folder.GetTask($k)
    $d = $t.Definition
    $d.Triggers.Item(1).Enabled = $true
    $null = $folder.RegisterTaskDefinition($k, $d, 4, $null, $null, 0)
    "$k flipped to Enabled=true via Definition + TASK_UPDATE; starts so far: $(Starts $k)"
}
# the same by XML
foreach ($k in 'oncepast-disabled-swa') {
    $t = $folder.GetTask($k)
    "xml readback of the triggers: $(([regex]::Match($t.Xml, '(?s)<Triggers>.*?</Triggers>')).Value)"
}
Write-Host "observing for $Watch s ..."
Start-Sleep -Seconds $Watch
foreach ($k in 'oncepast-disabled-swa', 'fixedpast-disabled-swa') { "{0}: action starts after the flip = {1}; {2}" -f $k, (Starts $k), (Info $folder.GetTask($k) 'values') }

Section "M7.3 Run on a task whose triggers are all disabled, and on one with none"
$t = Reg 'run-alloff' (Daily '2030-01-01T03:30:00' $false) $false
try { $rt = $t.Run($null); Write-Host "Run all-triggers-disabled: ok guid=$($rt.InstanceGuid) pid=$($rt.EnginePID)"; Wait-NoInstances $t } catch { Write-Host "Run all-triggers-disabled: FAILED $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
"starts: $(Starts 'run-alloff'); $(Info $t 'values')"
$t = Reg 'run-notrig' '' $false
try { $rt = $t.Run($null); Write-Host "Run no-triggers: ok guid=$($rt.InstanceGuid) pid=$($rt.EnginePID)"; Wait-NoInstances $t } catch { Write-Host "Run no-triggers: FAILED $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
"starts: $(Starts 'run-notrig'); $(Info $t 'values')"
$t = Reg 'run-taskoff' (Daily '2030-01-01T03:30:00') $false
$d = $t.Definition; $d.Settings.Enabled = $false
$t = $folder.RegisterTaskDefinition('run-taskoff', $d, 4, $null, $null, 0)
try { $rt = $t.Run($null); Write-Host "Run task-disabled: ok" } catch { Write-Host "Run task-disabled: FAILED $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
try { $rt = $t.RunEx($null, 2, 0, $null); Write-Host "RunEx(IGNORE_CONSTRAINTS) task-disabled: ok" } catch { Write-Host "RunEx(IGNORE_CONSTRAINTS) task-disabled: FAILED $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }

Section "M7.4 IgnoreNew with Run against a live instance"
$t = Reg 'live' '' $false 'IgnoreNew' 'ctrl'
$rt1 = $t.Run($null)
Wait-Lines "$Logs\m7-live.log" 2
"first Run: guid=$($rt1.InstanceGuid) pid=$($rt1.EnginePID)"
$rt2 = $t.Run($null)
"second Run (live instance): ok guid=$($rt2.InstanceGuid) pid=$($rt2.EnginePID) state=$($rt2.State)"
"listing: $((@($t.GetInstances(0)) | ForEach-Object { "$($_.InstanceGuid)/$($_.EnginePID)" }) -join ', ')"
$rt3 = $t.RunEx($null, 0, 0, $null)
"RunEx (live instance): ok guid=$($rt3.InstanceGuid) pid=$($rt3.EnginePID)"
"listing: $((@($t.GetInstances(0)) | ForEach-Object { "$($_.InstanceGuid)/$($_.EnginePID)" }) -join ', ')"
$proc = [Diagnostics.Process]::GetProcessById([int]$rt1.EnginePID); $null = $proc.Handle
$rt1.Stop(); $proc.WaitForExit(); Wait-NoInstances $t
"action starts (ctrl action logs 2 lines + handler lines per start; count of 'start spec' lines): $(@(Get-Content "$Logs\m7-live.log" | Where-Object { $_ -match 'start spec' }).Count)"

Remove-ProbeTasks $svc
