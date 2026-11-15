# Shared helpers of the Task Scheduler probes (dot-sourced). Runs on a throwaway Windows CI runner only:
# it creates local accounts, ACLs and scheduled tasks.
$ErrorActionPreference = 'Continue'
$Script:Probe = 'C:\probe'
$Script:Exe = 'C:\probe\bin\tsprobe.exe'
$Script:Logs = 'C:\probe\logs'
$Script:Folder = '\GoetiaProbe'

function Show-Platform {
    "ImageOS=$env:ImageOS ImageVersion=$env:ImageVersion"
    (cmd /c ver) -join ' '
    $cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
    "ProductName=$($cv.ProductName) DisplayVersion=$($cv.DisplayVersion) CurrentBuild=$($cv.CurrentBuild) UBR=$($cv.UBR) BuildLabEx=$($cv.BuildLabEx)"
    "Caption=$((Get-CimInstance Win32_OperatingSystem).Caption) Version=$((Get-CimInstance Win32_OperatingSystem).Version)"
    "whoami=$(whoami) timezone=$((Get-TimeZone).Id) now=$((Get-Date).ToString('o'))"
    "PowerShell=$($PSVersionTable.PSVersion)"
}

function Initialize-Probe {
    New-Item -ItemType Directory -Force $Script:Probe, "$Script:Probe\bin", $Script:Logs | Out-Null
    icacls $Script:Probe '/grant' 'Everyone:(OI)(CI)M' | Out-Null
    $built = (Resolve-Path "$PSScriptRoot\target\release\tsprobe.exe" -ErrorAction SilentlyContinue)
    if ($built) { Copy-Item $built $Script:Exe -Force }
    if (-not (Test-Path $Script:Exe)) { throw "tsprobe.exe is not built" }
    icacls $Script:Exe '/grant' 'Everyone:(RX)' | Out-Null
}

function Grant-Right([string]$Account, [string]$Right) {
    if (-not ('Lsa' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Security.Principal;
public static class Lsa {
    [StructLayout(LayoutKind.Sequential)]
    struct OBJATTR { public int Length; public IntPtr Root; public IntPtr Name; public int Attr; public IntPtr Sd; public IntPtr Qos; }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct USTR { public ushort Length; public ushort Max; public string Buffer; }
    [DllImport("advapi32.dll")] static extern uint LsaOpenPolicy(IntPtr sys, ref OBJATTR a, uint access, out IntPtr h);
    [DllImport("advapi32.dll")] static extern uint LsaAddAccountRights(IntPtr h, byte[] sid, ref USTR rights, uint count);
    [DllImport("advapi32.dll")] static extern uint LsaClose(IntPtr h);
    [DllImport("advapi32.dll")] static extern int LsaNtStatusToWinError(uint s);
    public static int Grant(string account, string right) {
        var sid = (SecurityIdentifier)new NTAccount(account).Translate(typeof(SecurityIdentifier));
        var bytes = new byte[sid.BinaryLength]; sid.GetBinaryForm(bytes, 0);
        var oa = new OBJATTR(); oa.Length = Marshal.SizeOf(typeof(OBJATTR));
        IntPtr h;
        uint s = LsaOpenPolicy(IntPtr.Zero, ref oa, 0x810, out h);
        if (s != 0) return LsaNtStatusToWinError(s);
        var u = new USTR(); u.Buffer = right; u.Length = (ushort)(right.Length * 2); u.Max = (ushort)(u.Length + 2);
        s = LsaAddAccountRights(h, bytes, ref u, 1);
        LsaClose(h);
        return s == 0 ? 0 : LsaNtStatusToWinError(s);
    }
}
'@
    }
    $r = [Lsa]::Grant($Account, $Right)
    "granted $Right to $Account via LsaAddAccountRights: win32 result $r"
}

function Get-Sid([string]$Account) {
    (New-Object Security.Principal.NTAccount($Account)).Translate([Security.Principal.SecurityIdentifier]).Value
}

function New-ProbeUser([string]$Name, [switch]$Admin, [switch]$NoBatchRight) {
    # `net user` prompts for passwords over 14 characters.
    $pw = 'Pb1!' + [guid]::NewGuid().ToString('N').Substring(0, 10)
    net user $Name $pw /add | Out-Null
    if ($Admin) { net localgroup Administrators $Name /add | Out-Null }
    if (-not $NoBatchRight) { Grant-Right $Name 'SeBatchLogonRight' | Out-Host }
    @{ Kind = 'user'; Name = $Name; Qualified = "$env:COMPUTERNAME\$Name"; UserId = "$env:COMPUTERNAME\$Name"; Password = $pw; Sid = (Get-Sid $Name) }
}

function Remove-ProbeUser($U) { if ($U) { net user $U.Name /delete 2>&1 | Out-Null } }

function Get-Principal([string]$Kind, $User) {
    switch ($Kind) {
        'system' { @{ Kind = 'system'; Name = 'SYSTEM'; UserId = 'S-1-5-18'; Sid = 'S-1-5-18' } }
        'localservice' { @{ Kind = 'localservice'; Name = 'LOCAL SERVICE'; UserId = 'S-1-5-19'; Sid = 'S-1-5-19' } }
        'networkservice' { @{ Kind = 'networkservice'; Name = 'NETWORK SERVICE'; UserId = 'S-1-5-20'; Sid = 'S-1-5-20' } }
        'user' { $User }
    }
}

function Esc([string]$s) { [System.Security.SecurityElement]::Escape($s) }

function New-TaskXml {
    param(
        [Parameter(Mandatory)][hashtable]$P,
        [Parameter(Mandatory)][string]$Command,
        [string]$Arguments = '',
        [string]$WorkingDirectory = '',
        [string]$Triggers = '',
        [string]$Policy = 'IgnoreNew',
        [int]$Priority = 7,
        [string]$RunLevel = 'LeastPrivilege',
        [string]$DataXml = '',
        [bool]$Enabled = $true,
        [bool]$StartWhenAvailable = $false,
        [bool]$Hidden = $false,
        [string]$Description = 'goetia probe',
        [string]$RegistrationExtra = ''
    )
    # `ServiceAccount` is not a value of the XML LogonType (measured: (8,35):LogonType:ServiceAccount); a service
    # account's principal names the account and no LogonType.
    $logon = if ($P.Kind -eq 'user') { '<LogonType>Password</LogonType>' } else { '' }
    $trig = if ($Triggers) { "<Triggers>$Triggers</Triggers>" } else { '' }
    $args_ = if ($Arguments) { "<Arguments>$(Esc $Arguments)</Arguments>" } else { '' }
    $prio = if ($Priority -ge 0) { "<Priority>$Priority</Priority>" } else { '' }
    $wd = if ($WorkingDirectory) { "<WorkingDirectory>$(Esc $WorkingDirectory)</WorkingDirectory>" } else { '' }
    @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>$(Esc $Description)</Description>$RegistrationExtra</RegistrationInfo>
  $trig
  <Principals>
    <Principal id="Author">
      <UserId>$(Esc $P.UserId)</UserId>
      $logon
      <RunLevel>$RunLevel</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>$Policy</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>$("$StartWhenAvailable".ToLower())</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>$("$Enabled".ToLower())</Enabled>
    <Hidden>$("$Hidden".ToLower())</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    $prio
  </Settings>
  <Actions Context="Author">
    <Exec><Command>$(Esc $Command)</Command>$args_$wd</Exec>
  </Actions>
  $DataXml
</Task>
"@
}

function Get-ProbeService { $s = New-Object -ComObject Schedule.Service; $s.Connect(); $s }

function Get-ProbeFolder($Svc, [string]$Path = $Script:Folder) {
    try { return $Svc.GetFolder($Path) } catch {
        $root = $Svc.GetFolder('\')
        $null = $root.CreateFolder($Path.TrimStart('\'))
        return $Svc.GetFolder($Path)
    }
}

# RegisterTask the way the XML of a principal needs it; tries the argument sets in turn and says which worked.
function Register-ProbeTask($Folder, [string]$Name, [string]$Xml, [hashtable]$P, [int]$Flags = 6) {
    if ($P.Kind -eq 'user') { $attempts = @(, @($P.UserId, $P.Password, 1)) }
    else { $attempts = @(@($null, $null, 0), @($P.UserId, $null, 5)) }
    $last = $null
    foreach ($a in $attempts) {
        try {
            $t = $Folder.RegisterTask($Name, $Xml, $Flags, $a[0], $a[1], $a[2], $null)
            return $t
        } catch { $last = $_; Write-Host "  RegisterTask($Name) with (user=$($a[0]), logon=$($a[2])) failed: $($_.Exception.Message) [0x$('{0:X8}' -f $_.Exception.HResult)]" }
    }
    throw $last
}

function Remove-ProbeTasks($Svc) {
    try {
        $f = $Svc.GetFolder($Script:Folder)
        foreach ($t in @($f.GetTasks(1))) {
            try { foreach ($i in @($t.GetInstances(0))) { try { $i.Stop() } catch {} }; } catch {}
            try { $f.DeleteTask($t.Name, 0) } catch {}
        }
        $Svc.GetFolder('\').DeleteFolder($Script:Folder.TrimStart('\'), 0)
    } catch {}
}

function HexOf([int]$v) { '0x{0:X8}' -f ([uint32]([long]$v -band 0xFFFFFFFFL)) }

# A wait on the real condition, no cap: the job's timeout is the failure bound.
function Wait-NoInstances($Task) { while (@($Task.GetInstances(0)).Count -gt 0) { } }
# Run, wait for the action's process by its handle, then for the listing to drop; the Run's error text if it failed.
function Run-Wait($Task) {
    try {
        $rt = $Task.Run($null)
        $pid_ = [int]$rt.EnginePID
        if ($pid_ -ne 0) { try { $p = [Diagnostics.Process]::GetProcessById($pid_); $null = $p.Handle; $p.WaitForExit() } catch {} }
        Wait-NoInstances $Task
        return $null
    } catch { return "Run FAILED: $($_.Exception.Message.Trim()) [$(HexOf $_.Exception.HResult)]" }
}
function Wait-Match([string]$Path, [string]$Pattern) { while (-not (Select-String -Path $Path -Pattern $Pattern -Quiet -ErrorAction SilentlyContinue)) { } }
function Wait-Lines([string]$Path, [int]$N) { while (@(Get-Content $Path -ErrorAction SilentlyContinue).Count -lt $N) { } }

function Initialize-Native {
    if ('Native' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Native {
    [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr h, uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    // TerminateProcess(pid, code); the Win32 error, 0 on success.
    public static int Terminate(uint pid, uint code) {
        IntPtr h = OpenProcess(0x0001, false, pid);
        if (h == IntPtr.Zero) return Marshal.GetLastWin32Error();
        bool ok = TerminateProcess(h, code);
        int e = ok ? 0 : Marshal.GetLastWin32Error();
        CloseHandle(h);
        return e;
    }
}
'@
}

# One task's status values, raw.
function Info($Task, [string]$Label) {
    $r = $Task.LastTaskResult
    $lr = $Task.LastRunTime
    $nr = $Task.NextRunTime
    $sched = if ((([long]$r) -band 0xFFFFFF00L) -eq 0x41300L) { 'SCHED_S_*' } else { '' }
    "{0}: LastTaskResult={1} ({2}) {3} LastRunTime={4} (oa {5}) NextRunTime={6} (oa {7}) State={8} Enabled={9}" -f $Label, $r, (HexOf $r), $sched, $lr.ToString('o'), $lr.ToOADate(), $nr.ToString('o'), $nr.ToOADate(), $Task.State, $Task.Enabled
}

function Section([string]$Title) { Write-Host ""; Write-Host "===== $Title =====" }
