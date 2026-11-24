# Question 5: Task Scheduler's Operational log as a per-run source. Is it on or off on the runner, can it be turned on with
# `wevtutil sl ... /e:true` without a restart, which events does a run write, do they carry the instance GUID goetia names run
# records by (the `InstanceGuid` of the listing entry the shim finds itself in, which `Run` also returns), and what do they say for a run
# that was stopped and whose action process Windows ended about a second later.
#   q5-log.ps1 -Kind system|user [-Reps 12]
# Runs: `exit7` (an action that exits 7 at once), `stop-block` (a console action whose CTRL_CLOSE handler blocks; Windows ends it about 1 s
# after `IRunningTask::Stop`), `stop-shim` (a parent console action with a child on its console that never leaves its handler: the shape of
# goetia's shim and its program; both are ended about 1 s after the Stop), `stop-exit` (a handler that never returns while the run thread
# ends the process at once with exit code 0x1234). The waits for the log are on the instance's own terminal event (102, 111 or 201) with a
# printed failure bound of 30 s. Throwaway Windows CI runner only.
param(
    [Parameter(Mandatory)][ValidateSet('system', 'user')][string]$Kind,
    [int]$Reps = 12
)
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$LogName = 'Microsoft-Windows-TaskScheduler/Operational'
function Service-Line {
    $s = Get-CimInstance Win32_Service -Filter "Name='Schedule'"
    $p = Get-Process -Id $s.ProcessId -ErrorAction SilentlyContinue
    "Schedule service: state=$($s.State) pid=$($s.ProcessId) process-start=$($p.StartTime.ToUniversalTime().ToString('o'))"
}
function Log-Line {
    $l = Get-WinEvent -ListLog $LogName
    "log: IsEnabled=$($l.IsEnabled) LogMode=$($l.LogMode) MaximumSizeInBytes=$($l.MaximumSizeInBytes) RecordCount=$($l.RecordCount) FileSize=$($l.FileSize)"
}
function Norm([string]$g) { $g.Trim('{', '}').ToUpperInvariant() }

# Every event of the log since $Since, as objects with its EventData fields.
function Read-Events([DateTime]$Since) {
    $evs = Get-WinEvent -FilterHashtable @{ LogName = $LogName; StartTime = $Since.ToLocalTime() } -ErrorAction SilentlyContinue
    foreach ($e in @($evs)) {
        $x = [xml]$e.ToXml()
        $d = [ordered]@{}
        foreach ($n in @($x.Event.EventData.Data)) { if ($n.Name) { $d[$n.Name] = "$($n.'#text')" } }
        [pscustomobject]@{ Id = $e.Id; Time = $e.TimeCreated.ToUniversalTime(); Rec = $e.RecordId; Data = $d; Opcode = $e.OpcodeDisplayName; Task = $e.TaskDisplayName; Msg = $e }
    }
}

$user = $null
$ok = $false
try {
    if ($Kind -eq 'user') { $user = New-ProbeUser 'glogq' }
    $P = Get-Principal $Kind $user
    "principal: $Kind ($($P.UserId))"
    $svc = Get-ProbeService
    Remove-ProbeTasks $svc
    $folder = Get-ProbeFolder $svc

    Section "the log before anything"
    Service-Line
    Log-Line
    wevtutil gl $LogName
    $startAll = [DateTime]::UtcNow

    # A run while the log is off.
    $log0 = "$Logs\q5-off.log"
    $xml0 = New-TaskXml -P $P -Command $Script:Exe -Arguments "act rec $log0 7"
    $t0 = Register-ProbeTask $folder 'off' $xml0 $P
    Section "a run while the log is as it came"
    $err = Run-Wait $t0
    "run: $err ; $(Info $t0 'off')"
    $before = @(Read-Events $startAll)
    "events in the log since the start of the job: $($before.Count)"
    if (-not (Get-WinEvent -ListLog $LogName).IsEnabled) {
        $ev0 = @($before | Where-Object { $_.Data['TaskName'] -like '*\off' })
        "events for the run while the log was off: $($ev0.Count)"
    }

    Section "turn the log on"
    $svcBefore = Service-Line
    wevtutil sl $LogName /e:true
    "wevtutil sl /e:true exit code: $LASTEXITCODE"
    Log-Line
    $svcAfter = Service-Line
    "service unchanged by the switch: $($svcBefore -eq $svcAfter)"
    $startOn = [DateTime]::UtcNow
    $Script:StartOn = $startOn

    $actions = [ordered]@{
        'exit7'      = @{ Args = { param($l) "act rec $l 7" }; Ready = 0; Stop = $false }
        'stop-block' = @{ Args = { param($l) "act ctrl $l block:35" }; Ready = 1; Stop = $true }
        'stop-shim'  = @{ Args = { param($l) "order parent $l cosca c p2 single" }; Ready = 2; Stop = $true }
        'stop-exit'  = @{ Args = { param($l) "act ctrl $l never:exitprocess:4660" }; Ready = 1; Stop = $true }
    }
    $runs = @()
    foreach ($a in $actions.Keys) {
        $spec = $actions[$a]
        $log = "$Logs\q5-$a.log"
        $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments (& $spec.Args $log)
        $t = Register-ProbeTask $folder $a $xml $P
        Section "$Kind/$a"
        foreach ($rep in (0..$Reps)) {   # rep 0 is a warm-up (a new local user's first Run builds its profile), not counted
            Remove-Item $log -ErrorAction SilentlyContinue
            $r = [ordered]@{ Row = "$Kind/$a/$rep"; Action = $a; Warm = ($rep -eq 0) }
            $r.RunCall = [DateTime]::UtcNow
            try { $rt = $t.Run($null) } catch { "R $($r.Row): Run FAILED $($_.Exception.Message)"; continue }
            $r.RunRet = [DateTime]::UtcNow
            $r.RunGuid = "$($rt.InstanceGuid)"
            $r.Pid = [int]$rt.EnginePID
            $proc = $null
            if ($r.Pid -ne 0) {
                try { $proc = [Diagnostics.Process]::GetProcessById($r.Pid); $null = $proc.Handle }
                catch { "B pid $($r.Pid) from Run is not a running process right after Run; action log: [$((Get-Content $log -ErrorAction SilentlyContinue) -join ' | ')]" }
            }
            if ($spec.Ready -gt 0) {
                $t1 = Get-Date
                while (@(Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -match ' ready$' }).Count -lt $spec.Ready) {
                    if (((Get-Date) - $t1).TotalSeconds -gt 60) { "B no ready lines in 60 s"; break }
                }
            }
            $r.Ready = [DateTime]::UtcNow
            $li = @($t.GetInstances(0))
            $r.ListGuid = if ($li.Count) { "$($li[0].InstanceGuid)" } else { '-' }
            $r.ListPid = if ($li.Count) { [int]$li[0].EnginePID } else { 0 }
            if ($spec.Stop) {
                $r.StopCall = [DateTime]::UtcNow
                try { $rt.Stop() ; $r.StopErr = '' } catch { $r.StopErr = "$($_.Exception.Message.Trim())" }
                $r.StopRet = [DateTime]::UtcNow
            }
            if ($proc) {
                if (-not $proc.WaitForExit(120000)) { "B process did not end in 120 s" }
                $r.ProcExit = [DateTime]::UtcNow
                $r.ExitCode = $proc.ExitCode
                $r.ProcExitTime = $proc.ExitTime.ToUniversalTime()
            }
            Wait-NoInstances $t
            $r.ListEmpty = [DateTime]::UtcNow
            $r.LastResult = HexOf $t.LastTaskResult
            # The instance's own terminal event; a failure bound of 30 s is printed.
            $g = Norm $r.RunGuid
            $seen = @{}
            $t2 = Get-Date
            while ($true) {
                $evs = @(Read-Events $startOn | Where-Object { $_.Data['TaskInstanceId'] -and (Norm $_.Data['TaskInstanceId']) -eq $g })
                foreach ($e in $evs) { if (-not $seen.ContainsKey($e.Rec)) { $seen[$e.Rec] = @{ Ev = $e; SeenAt = [DateTime]::UtcNow } } }
                if (@($evs | Where-Object { $_.Id -in 102, 111, 201 }).Count -gt 0) { break }
                if (((Get-Date) - $t2).TotalSeconds -gt 30) { "B no terminal event for $g in 30 s"; break }
            }
            $r.Events = @($seen.Values | Sort-Object { $_.Ev.Time })
            $runs += [pscustomobject]$r
            if ($r.Warm) { "(warm-up row not counted)" }
            $rel = { param($x) if ($x -and $r.StopCall) { '{0:N0}' -f (($x - $r.StopCall).TotalMilliseconds) } elseif ($x) { '{0:N0}' -f (($x - $r.RunCall).TotalMilliseconds) } else { '-' } }
            $base = if ($r.StopCall) { 'Stop call' } else { 'Run call' }
            "R $($r.Row): run-guid=$($r.RunGuid) listing-guid=$($r.ListGuid) pid=$($r.Pid)/$($r.ListPid) exit-code=$($r.ExitCode) (0x$('{0:X}' -f [int]$r.ExitCode)) LastTaskResult=$($r.LastResult) stop-err=[$($r.StopErr)]"
            "    times in ms from the $base : run-ret $(& $rel $r.RunRet) ready $(& $rel $r.Ready) stop-ret $(& $rel $r.StopRet) proc-exit $(& $rel $r.ProcExit) (kernel exit time $(& $rel $r.ProcExitTime)) listing-empty $(& $rel $r.ListEmpty)"
            foreach ($s in $r.Events) {
                $e = $s.Ev
                $d = ($e.Data.Keys | ForEach-Object { "$_=$($e.Data[$_])" }) -join ' '
                "    event $($e.Id) at $(& $rel $e.Time) ms (first read $(& $rel $s.SeenAt) ms; created UTC $($e.Time.ToString('HH:mm:ss.fff'))) $($e.Task)/$($e.Opcode): $d"
            }
        }
    }

    Section "every event id in the log since it was switched on: count, and one rendered message"
    $all = @(Read-Events $startOn)
    $all | Group-Object Id | Sort-Object { [int]$_.Name } | ForEach-Object {
        $msg = try { ($_.Group[0].Msg.FormatDescription() -replace '\s+', ' ') } catch { '?' }
        if ($msg.Length -gt 200) { $msg = $msg.Substring(0, 200) }
        "id $($_.Name): $($_.Count) events; data fields: $($_.Group[0].Data.Keys -join ','); e.g. [$msg]"
    }
    Section "per row: the ids of the events that carry the row's instance guid (counted over all rows after the warm-ups)"
    foreach ($a in $actions.Keys) {
        $rows = @($runs | Where-Object { $_.Action -eq $a -and -not $_.Warm })
        $shapes = $rows | ForEach-Object { ($_.Events | ForEach-Object { $_.Ev.Id }) -join ',' } | Group-Object | ForEach-Object { "[$($_.Name)] x$($_.Count)" }
        $match = @($rows | Where-Object { $_.RunGuid -eq $_.ListGuid }).Count
        "$Kind/${a}: rows=$($rows.Count); Run's guid equals the listing's in $match; event-id sequences: $($shapes -join ' ')"
    }
    $ok = $true
}
finally {
    Remove-ProbeTasks $svc
    Remove-ProbeUser $user
    # The log is left as the job found it only in that the runner is thrown away.
}
exit 0
