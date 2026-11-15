# M8 (per principal: its own instance and Data from inside the action, the run-record directory under a one-principal
# ACL, SeBatchLogonRight, RunLevel), M9 (argv, working directory, environment) and M11 (Priority).
# Throwaway Windows CI runner only.
param([Parameter(Mandatory)][ValidateSet('system', 'localservice', 'networkservice', 'user')][string]$Kind)
. "$PSScriptRoot\common.ps1"
$Script:Folder = '\Goetia'
Section "platform"; Show-Platform
Initialize-Probe
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc
$pd = [Environment]::GetFolderPath('CommonApplicationData')
$runs = "$pd\Goetia\runs"
"ProgramData = $pd"

function Say-Out([string]$Path, [string]$Pattern = '.') { Get-Content $Path -ErrorAction SilentlyContinue | Where-Object { $_ -match $Pattern } | ForEach-Object { "    $_" } }
function Set-RunsDir([string]$Dir, [string]$Sid) {
    New-Item -ItemType Directory -Force $Dir | Out-Null
    # protected: SYSTEM and Administrators full control, the principal modify, nothing inherited
    icacls $Dir '/inheritance:r' '/grant' '*S-1-5-18:(OI)(CI)F' '/grant' '*S-1-5-32-544:(OI)(CI)F' '/grant' "*${Sid}:(OI)(CI)M" | Out-Null
    "    ACL of ${Dir}:"; icacls $Dir | ForEach-Object { "      $_" }
}
function Reg([hashtable]$P, [string]$Name, [string]$ActArgs, [string]$RunLevel = 'HighestAvailable', [int]$Priority = 7, [string]$Wd = '', [string]$Cmd = $Script:Exe) {
    $xml = New-TaskXml -P $P -Command $Cmd -Arguments $ActArgs -RunLevel $RunLevel -Priority $Priority -WorkingDirectory $Wd -DataXml '<Data>GOETIA-M8-DATA-0123456789</Data>'
    Register-ProbeTask $folder $Name $xml $P
}

$user = $null
if ($Kind -eq 'user') { $user = New-ProbeUser 'gprobem8' -NoBatchRight }
$P = Get-Principal $Kind $user
Section "M8: $Kind runs its action; the action reads its own task and writes a run record"
$dir = "$runs\m8-$Kind"
Set-RunsDir $dir $P.Sid
$out = "$Logs\m8-$Kind.txt"; Remove-Item $out -ErrorAction SilentlyContinue
$t = Reg $P 'm8' "act self $out task=\Goetia\m8 rundir=$dir"
if ($Kind -eq 'user') {
    "The account has no SeBatchLogonRight yet. Registration succeeded."
    $e = Run-Wait $t
    "first Run without the right: $(if ($e) { $e } else { 'Run returned ok' }); LastTaskResult=$(HexOf $t.LastTaskResult); output written: $(Test-Path $out)"
    Grant-Right $user.Name 'SeBatchLogonRight'
    Remove-Item $out -ErrorAction SilentlyContinue
    $e = Run-Wait $t
    "first Run after LsaAddAccountRights(SeBatchLogonRight): $(if ($e) { $e } else { 'Run returned ok' }); LastTaskResult=$(HexOf $t.LastTaskResult); output written: $(Test-Path $out)"
}
else {
    $e = Run-Wait $t
    "Run: $(if ($e) { $e } else { 'ok' }); LastTaskResult=$(HexOf $t.LastTaskResult)"
}
"the action's own report (everything but the env block):"
Say-Out $out '^(argc|argv|rawcmd|CommandLineToArgvW|cwd|exe|envcount|priority|child-|token-|com:|record:|== )'
"the environment the action saw, as $Kind (M9: the daemon inherits it):"
Say-Out $out '^env:'
"the run-record directory after the action: $(@(Get-ChildItem $dir -ErrorAction SilentlyContinue | ForEach-Object Name) -join ', ')"

if ($Kind -eq 'user') {
    Section "M8: RunLevel HighestAvailable against LeastPrivilege, for an administrator account and a standard one"
    $adm = New-ProbeUser 'gprobem8a' -Admin
    $PA = Get-Principal 'user' $adm
    foreach ($c in @(@{ N = 'adm-highest'; P = $PA; R = 'HighestAvailable'; D = 'administrator, HighestAvailable' },
            @{ N = 'adm-least'; P = $PA; R = 'LeastPrivilege'; D = 'administrator, LeastPrivilege' },
            @{ N = 'std-highest'; P = $P; R = 'HighestAvailable'; D = 'standard user, HighestAvailable' },
            @{ N = 'std-least'; P = $P; R = 'LeastPrivilege'; D = 'standard user, LeastPrivilege' })) {
        $o = "$Logs\m8-$($c.N).txt"; Remove-Item $o -ErrorAction SilentlyContinue
        $tt = Reg $c.P $c.N "act self $o noenv" $c.R
        $e = Run-Wait $tt
        "$($c.D): $(if ($e) { $e } else { 'ran' }); LastTaskResult=$(HexOf $tt.LastTaskResult)"
        Say-Out $o '^token-'
    }
    Remove-ProbeUser $adm
}
if ($Kind -eq 'system') {
    Section "M8: SYSTEM with HighestAvailable and LeastPrivilege"
    foreach ($r in 'HighestAvailable', 'LeastPrivilege') {
        $o = "$Logs\m8-sys-$r.txt"; Remove-Item $o -ErrorAction SilentlyContinue
        $tt = Reg $P "sys-$r" "act self $o noenv" $r
        $e = Run-Wait $tt; "SYSTEM, ${r}: $(if ($e) { $e } else { 'ran' })"; Say-Out $o '^token-'
    }
}

Section "M9: the action's invocation, as $Kind"
$cases = @(
    'a b c', '"a b" c', 'a\ b', '"C:\dir with space\" x', '"C:\dir with space\\" x', 'C:\x\ y', 'a"b', '\"', '""', '"a""b"',
    'trailing\\ "q"', '%ProgramData%\x', '日本語 é', '--goetia-task my-id', 'a  b', "'single quoted' x", '"" "" x', '"unterminated x', 'back\\\"slash'
)
$i = 0
foreach ($c in $cases) {
    $i++
    $o = "$Logs\m9-$Kind-$i.txt"; Remove-Item $o -ErrorAction SilentlyContinue
    $tt = Reg $P "arg$i" "act self $o noenv $c"
    $e = Run-Wait $tt
    "case $i Arguments=<act self ... noenv $c> : $(if ($e) { $e } else { 'ran' })"
    Say-Out $o '^(rawcmdline|argv\[|CommandLineToArgvW)'
}

Section "M9: WorkingDirectory"
foreach ($wd in '', 'C:\probe', 'C:\Windows') {
    $o = "$Logs\m9-wd.txt"; Remove-Item $o -ErrorAction SilentlyContinue
    $tt = Reg $P 'wd' "act self $o noenv" 'HighestAvailable' 7 $wd
    $e = Run-Wait $tt
    "WorkingDirectory=<$wd>: $(if ($e) { $e } else { 'ran' }) -> $((Get-Content $o -ErrorAction SilentlyContinue | Where-Object { $_ -like 'cwd=*' }))"
}

Section "M9: a Command path with a space, quoted or not"
New-Item -ItemType Directory -Force 'C:\probe\dir with space' | Out-Null
Copy-Item $Script:Exe 'C:\probe\dir with space\tsprobe.exe' -Force
icacls 'C:\probe\dir with space\tsprobe.exe' '/grant' 'Everyone:(RX)' | Out-Null
foreach ($cmd in 'C:\probe\dir with space\tsprobe.exe', '"C:\probe\dir with space\tsprobe.exe"') {
    $o = "$Logs\m9-space.txt"; Remove-Item $o -ErrorAction SilentlyContinue
    try {
        $tt = Reg $P 'space' "act self $o noenv" 'HighestAvailable' 7 '' $cmd
        $e = Run-Wait $tt
        "Command=<$cmd>: $(if ($e) { $e } else { 'ran' }); LastTaskResult=$(HexOf $tt.LastTaskResult) -> $((Get-Content $o -ErrorAction SilentlyContinue | Where-Object { $_ -match '^(exe=|rawcmdline=)' }) -join ' | ')"
    } catch { "Command=<$cmd>: REGISTRATION REFUSED $($_.Exception.Message.Trim())" }
}

Section "M11: Priority (the priority class of the action and of its child)"
$prios = if ($Kind -in 'system', 'user') { @(-1) + (0..10) } else { @(-1, 4, 5, 6, 7, 8) }
foreach ($pr in $prios) {
    $o = "$Logs\m11.txt"; Remove-Item $o -ErrorAction SilentlyContinue
    try {
        $tt = Reg $P 'prio' "act self $o noenv" 'HighestAvailable' $pr
        $e = Run-Wait $tt
        $l = (Get-Content $o -ErrorAction SilentlyContinue | Where-Object { $_ -match '^(priority-class|child-priority-class)' }) -join ' '
        "Priority=$(if ($pr -lt 0) { '(omitted)' } else { $pr }): $(if ($e) { $e } else { 'ran' }) -> $l   [0x20 NORMAL, 0x4000 BELOW_NORMAL, 0x8000 ABOVE_NORMAL, 0x40 IDLE, 0x80 HIGH, 0x100 REALTIME]"
    } catch { "Priority=${pr}: REGISTRATION REFUSED $($_.Exception.Message.Trim())" }
}

Remove-ProbeTasks $svc
Remove-Item "$runs\m8-$Kind" -Recurse -Force -ErrorAction SilentlyContinue
Remove-ProbeUser $user
