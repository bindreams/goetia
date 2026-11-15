# M5: does the Schedule service (or any engine process) hold a handle to the action's process while the instance is
# listed, and until when? Throwaway Windows CI runner only.
param([Parameter(Mandatory)][ValidateSet('system', 'user')][string]$Kind, [int]$Runs = 8)
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$user = $null
if ($Kind -eq 'user') { $user = New-ProbeUser 'gprobem5' }
$P = Get-Principal $Kind $user
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc
$sp = (Get-CimInstance Win32_Service -Filter "Name='Schedule'").ProcessId
"Schedule service pid: $sp"
"services hosted in that process: $((tasklist /svc /fi "PID eq $sp" /fo csv | Out-String).Trim())"
$ev = 'Global\GoetiaProbeM5'
$log = "$Logs\m5.log"; Remove-Item $log -ErrorAction SilentlyContinue
$xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act hold $log $ev 0"
$null = Register-ProbeTask $folder 'm5' $xml $P
Section "handle holders of the action's process object, as $Kind"
& $Script:Exe handles "$Script:TaskFolder\m5" $ev $sp $Runs $log
"tsprobe exit code $LASTEXITCODE"
Remove-ProbeTasks $svc
Remove-ProbeUser $user
