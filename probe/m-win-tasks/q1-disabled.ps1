# Question 1b: does Task Scheduler refuse to run a disabled task, by `schtasks /run`, `Start-ScheduledTask`, `IRegisteredTask::Run`,
# `RunEx`? Each start call, on a task disabled three ways and on three controls, -Reps times each.
#   q1-disabled.ps1 -Kind system|user [-Reps 5]
# Output per call: `C <kind>/<variant>/<method>/<rep>: <what the call said> ; instances-right-after=<n> ; action-lines=<n> ; State=.. Enabled=..`.
# A refused call starts nothing: instances-right-after is read at once; the action's log is read after a 3 s window (a negative
# read over that window, not a proof). Throwaway Windows CI runner only.
param(
    [Parameter(Mandatory)][ValidateSet('system', 'user')][string]$Kind,
    [int]$Reps = 5
)
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$user = $null
try {
    if ($Kind -eq 'user') { $user = New-ProbeUser 'gdisab' }
    $P = Get-Principal $Kind $user
    "principal: $Kind ($($P.UserId))"
    $svc = Get-ProbeService
    Remove-ProbeTasks $svc
    $folder = Get-ProbeFolder $svc

    $trigOn = "<CalendarTrigger><StartBoundary>2000-01-01T03:00:00</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>"
    $trigOff = $trigOn.Replace('<Enabled>true</Enabled>', '<Enabled>false</Enabled>')

    function Lines([string]$Log) { @(Get-Content $Log -ErrorAction SilentlyContinue).Count }

    function Start-By([string]$Method, $Task, [string]$Path) {
        try {
            switch ($Method) {
                'com-Run' { $r = $Task.Run($null); return "ok guid=$($r.InstanceGuid) enginePID=$($r.EnginePID)" }
                'com-RunEx0' { $r = $Task.RunEx($null, 0, 0, ''); return "ok guid=$($r.InstanceGuid) enginePID=$($r.EnginePID)" }
                'com-RunEx2' { $r = $Task.RunEx($null, 2, 0, ''); return "ok guid=$($r.InstanceGuid) enginePID=$($r.EnginePID)" }
                'schtasks-run' {
                    $out = (& schtasks.exe /run /tn $Path 2>&1) -join ' | '
                    return "exit=$LASTEXITCODE out=[$out]"
                }
                'Start-ScheduledTask' {
                    $leaf = Split-Path $Path -Leaf; $dir = (Split-Path $Path -Parent) + '\'
                    Start-ScheduledTask -TaskPath $dir -TaskName $leaf -ErrorAction Stop
                    return 'ok'
                }
            }
        } catch { return "FAILED $($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" }
    }

    $methods = 'com-Run', 'com-RunEx0', 'com-RunEx2', 'schtasks-run', 'Start-ScheduledTask'
    $variants = [ordered]@{
        'flag-false-in-xml'      = @{ Triggers = $trigOn; Enabled = $false; Pre = $null }
        'flag-false-by-com'      = @{ Triggers = $trigOn; Enabled = $true; Pre = { param($t, $p) $t.Enabled = $false } }
        'flag-false-by-schtasks' = @{ Triggers = $trigOn; Enabled = $true; Pre = { param($t, $p) & schtasks.exe /change /tn $p /disable | Out-Null } }
        'control-triggers-off'   = @{ Triggers = $trigOff; Enabled = $true; Pre = $null }
        'control-no-triggers'    = @{ Triggers = ''; Enabled = $true; Pre = $null }
        'control-enabled'        = @{ Triggers = $trigOn; Enabled = $true; Pre = $null }
    }
    $n = 0
    foreach ($v in $variants.Keys) {
        $n++
        $name = "dis$n"
        $path = "$Script:TaskFolder\$name"
        $log = "$Logs\disabled-$name.log"; Remove-Item $log -ErrorAction SilentlyContinue
        $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $log 0" -Triggers $variants[$v].Triggers -Enabled $variants[$v].Enabled
        $t = Register-ProbeTask $folder $name $xml $P
        if ($variants[$v].Pre) { & $variants[$v].Pre $t $path; $t = $folder.GetTask($name) }
        Section "$Kind/$v : $(Info $t 'state before')"
        foreach ($m in $methods) {
            foreach ($rep in 1..$Reps) {
                $before = Lines $log
                $said = Start-By $m $t $path
                $inst = @($t.GetInstances(0)).Count
                if ($said -like 'FAILED*' -or $said -like 'exit=[1-9]*') {
                    Start-Sleep -Seconds 3   # the observation window of a negative read
                } else {
                    # Started: wait for the action's line, then for the listing to drop.
                    $t0 = Get-Date
                    while ((Lines $log) -le $before) { if (((Get-Date) - $t0).TotalSeconds -gt 120) { "B no action line in 120 s"; break } }
                    Wait-NoInstances $t
                }
                $t = $folder.GetTask($name)
                "C $Kind/$v/$m/${rep}: $said ; instances-right-after=$inst ; action-lines=$((Lines $log) - $before) ; State=$($t.State) Enabled=$($t.Enabled)"
            }
        }
    }
}
finally {
    Remove-ProbeTasks $svc
    Remove-ProbeUser $user
}
exit 0
