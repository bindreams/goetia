# L1-L3 for one principal. Runs on a throwaway Windows CI runner only.
#   L1  does OpenProcess(EnginePID) of an ended-but-listed instance open the same process (same creation time) every time;
#       does a listed pid ever name another process; how long does the listing linger; is a listed instance ever pid-less.
#   L2  Run / RunEx / a scheduled (event-triggered) fire while an ended instance is still listed, under IgnoreNew.
#   L3  IRunningTask::Stop on an instance still listed after its process ended; State after Refresh.
param(
    [Parameter(Mandatory)][ValidateSet('system', 'localservice', 'networkservice', 'user')][string]$Kind,
    [int]$Runs = 300,
    [string]$Only = 'all'   # a space-separated subset of: l1 l1s l1c l2 l2spam l3 fire nostart
)
function On([string]$Name) { $Only -eq 'all' -or (($Only -split ' ') -contains $Name) }
. "$PSScriptRoot\common.ps1"
Section "platform"
Show-Platform
Initialize-Probe
$user = $null
if ($Kind -eq 'user') { $user = New-ProbeUser 'gprobel' }
$P = Get-Principal $Kind $user
Write-Host "principal: $Kind ($($P.UserId))"
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc
wevtutil sl Microsoft-Windows-TaskScheduler/Operational /e:true
$t0 = Get-Date

function Reg([string]$Name, [string]$ActArgs, [string]$Trig = '', [string]$Cmd = $Script:Exe) {
    $xml = New-TaskXml -P $P -Command $Cmd -Arguments $ActArgs -Triggers $Trig
    $null = Register-ProbeTask $folder $Name $xml $P
}
function Ended([string]$Name, [string]$Exp, [int]$N, [string]$Label, [string]$Extra = '') {
    $log = "$Script:Logs\$Name.log"
    Remove-Item $log, "$log.token" -ErrorAction SilentlyContinue
    if ($Extra) { & $Script:Exe ended "$Script:TaskFolder\$Name" $Exp $N $Label $log $Extra } else { & $Script:Exe ended "$Script:TaskFolder\$Name" $Exp $N $Label $log }
    "tsprobe exit code $LASTEXITCODE"
}

try {
    if (On 'l1') {
        Section "L1 quiet: $Kind, $Runs runs"
        Reg 'l1' "act rec $Script:Logs\l1.log 7"
        Ended 'l1' 'l1' $Runs "L1 $Kind quiet"
    }
    if (On 'l1s') {
        Section "L1 quiet, an action that lives about 30 ms: $Kind, $Runs runs"
        Reg 'l1s' "act rec $Script:Logs\l1s.log 7 spin_ms=30"
        Ended 'l1s' 'l1' $Runs "L1 $Kind quiet 30ms"
    }
    if (On 'l1c') {
        Section "L1 with pid churn (three process-creating threads), an action that lives about 30 ms: $Kind, $Runs runs"
        Reg 'l1c' "act rec $Script:Logs\l1c.log 7 spin_ms=30"
        Ended 'l1c' 'l1' $Runs "L1 $Kind churn 30ms" 'churn'
    }
} catch { Write-Host "L1 FAILED: $_" }

try {
    if (On 'l2') { foreach ($exp in 'l2run', 'l2runex0', 'l2runex2') {
        Section "L2 $exp : $Kind, 100 runs"
        Reg $exp "act rec $Script:Logs\$exp.log 0 spin_ms=20"
        Ended $exp $exp 100 "L2 $Kind $exp"
    } }
} catch { Write-Host "L2 FAILED: $_" }

try {
    if (On 'l2spam') { foreach ($exp in 'l2spam', 'l2spamex0', 'l2spamex2') {
        Section "L2 $exp : $Kind, a thread calls Run/RunEx in a tight loop until one starts an instance, 60 runs"
        Reg $exp "act rec $Script:Logs\$exp.log 0 spin_ms=20"
        Ended $exp $exp 60 "L2 $Kind $exp"
    } }
} catch { Write-Host "L2 SPAM FAILED: $_" }

try {
    if (On 'l3') { foreach ($exp in 'l3stop', 'l3taskstop') {
        Section "L3 $exp : $Kind"
        Reg $exp "act rec $Script:Logs\$exp.log 0 spin_ms=20"
        Ended $exp $exp 100 "L3 $Kind $exp"
    } }
} catch { Write-Host "L3 FAILED: $_" }

if (On 'fire') { try {
    $sub = "&lt;QueryList&gt;&lt;Query Id=`"0`" Path=`"Application`"&gt;&lt;Select Path=`"Application`"&gt;*[System[Provider[@Name='GoetiaProbe']]]&lt;/Select&gt;&lt;/Query&gt;&lt;/QueryList&gt;"
    $trig = "<EventTrigger><Enabled>true</Enabled><Subscription>$sub</Subscription></EventTrigger>"
    foreach ($spin in 0, 5, 10, 20, 40, 80) {
        Section "L2 scheduled fire (event trigger): $Kind, the action spins $spin ms after writing the event"
        $name = "fire$spin"
        $log = "$Script:Logs\$name.log"
        Reg $name "act rec $log 0 emit=$log.token spin_ms=$spin" $trig
        Ended $name 'fire' 40 "L2 fire $Kind spin=$spin"
        $f = Get-ProbeFolder $svc
        $f.DeleteTask($name, 0)
    }
} catch { Write-Host "FIRE FAILED: $_" } }

if (On 'nostart') { try {
    Section "an action that cannot start: $Kind"
    Reg 'nostart' '' '' 'C:\probe\bin\missing.exe'
    & $Script:Exe nostart "$Script:TaskFolder\nostart" 30
} catch { Write-Host "NOSTART FAILED: $_" } }

Section "TaskScheduler Operational events during this leg, by id"
try {
    $ev = @(Get-WinEvent -FilterHashtable @{ LogName = 'Microsoft-Windows-TaskScheduler/Operational'; StartTime = $t0 } -ErrorAction Stop)
    $ev | Group-Object Id | Sort-Object { [int]$_.Name } | ForEach-Object { "id $($_.Name): $($_.Count)   e.g. $(($_.Group[0].Message -split "`n")[0])" }
    Section "the events with id 322 (a start that was not made), by task"
    $ev | Where-Object Id -eq 322 | ForEach-Object { [regex]::Match($_.Message, 'task "([^"]+)"').Groups[1].Value } | Group-Object | ForEach-Object { "$($_.Name): $($_.Count)" }
    Section "the events with id 101, 203 (a start that failed), by task"
    $ev | Where-Object { $_.Id -in 101, 203 } | ForEach-Object { "$($_.Id) " + [regex]::Match($_.Message, 'task "([^"]+)"').Groups[1].Value } | Group-Object | ForEach-Object { "$($_.Name): $($_.Count)" }
} catch { Write-Host "event read failed: $_" }

Remove-ProbeTasks $svc
Remove-ProbeUser $user
