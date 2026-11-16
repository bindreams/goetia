# F1-F3: the run-records folders of goetia slice 5 (%ProgramData%\Goetia, Goetia\runs, runs\<id>). Throwaway Windows CI runner
# only: it creates a local account and folders under C:\ProgramData.
#   F1  does C:\ProgramData let a standard user create a folder? (which image)
#   F2  elevated admin: a folder made with the descriptor "O:BA" (no DACL) reads back as owned by BA with inherited
#       entries and an unprotected DACL; a folder made with the shim folder's descriptor reads back as that descriptor
#       (owner, protection and the set of entries: what is_as_wanted compares).
#   F3  which DACL call on runs\<id> leaves a child file's DACL, and the DACL of a hard link's target, as they were:
#       SetNamedSecurityInfoW on the path, or SetSecurityInfo on a handle opened with WRITE_DAC.
param([string]$Only = 'F12 F3')
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class F {
    [StructLayout(LayoutKind.Sequential)]
    struct SA { public int Length; public IntPtr Sd; public int Inherit; }
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(string sddl, uint rev, out IntPtr sd, out uint size);
    [DllImport("advapi32.dll")]
    static extern bool GetSecurityDescriptorDacl(IntPtr sd, out bool present, out IntPtr dacl, out bool defaulted);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateDirectoryW(string path, ref SA sa);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateHardLinkW(string link, string existing, IntPtr reserved);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern SafeFileHandle CreateFileW(string path, uint access, uint share, IntPtr sa, uint disposition, uint flags, IntPtr template);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode)]
    static extern uint SetNamedSecurityInfoW(string name, int type, uint info, IntPtr owner, IntPtr group, IntPtr dacl, IntPtr sacl);
    [DllImport("advapi32.dll")]
    static extern uint SetSecurityInfo(SafeFileHandle h, int type, uint info, IntPtr owner, IntPtr group, IntPtr dacl, IntPtr sacl);

    static IntPtr Sd(string sddl) {
        IntPtr sd; uint size;
        if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, 1, out sd, out size)) throw new Exception("sddl: " + Marshal.GetLastWin32Error());
        return sd;
    }
    static IntPtr Dacl(IntPtr sd) {
        bool present, defaulted; IntPtr dacl;
        if (!GetSecurityDescriptorDacl(sd, out present, out dacl, out defaulted)) throw new Exception("dacl: " + Marshal.GetLastWin32Error());
        return dacl;
    }
    // CreateDirectoryW with a descriptor, as goetia's Descriptor::create_dir does. 0 is success.
    public static int MakeDir(string path, string sddl) {
        var sa = new SA { Length = Marshal.SizeOf(typeof(SA)), Sd = Sd(sddl), Inherit = 0 };
        return CreateDirectoryW(path, ref sa) ? 0 : Marshal.GetLastWin32Error();
    }
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool SetKernelObjectSecurity(SafeFileHandle h, uint info, IntPtr sd);
    // The same through the kernel's own call, which has no propagation to children: the SD's protected bit is the one set.
    public static uint SetDaclKernel(string path, string sddl) {
        using (var h = CreateFileW(path, 0x40000 | 0x20000, 7, IntPtr.Zero, 3, 0x02000000 | 0x00200000, IntPtr.Zero)) {
            if (h.IsInvalid) return (uint)Marshal.GetLastWin32Error();
            return SetKernelObjectSecurity(h, DACL_PROTECTED, Sd(sddl)) ? 0u : (uint)Marshal.GetLastWin32Error();
        }
    }
    public static int HardLink(string link, string existing) { return CreateHardLinkW(link, existing, IntPtr.Zero) ? 0 : Marshal.GetLastWin32Error(); }
    const uint DACL_PROTECTED = 0x80000004; // DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION
    // The DACL of `sddl`, protected, set on the path by name.
    public static uint SetDaclNamed(string path, string sddl) { return SetNamedSecurityInfoW(path, 1, DACL_PROTECTED, IntPtr.Zero, IntPtr.Zero, Dacl(Sd(sddl)), IntPtr.Zero); }
    // The same on a handle opened for WRITE_DAC | READ_CONTROL, backup semantics, not following a reparse point.
    public static uint SetDaclHandle(string path, string sddl) {
        using (var h = CreateFileW(path, 0x40000 | 0x20000, 7, IntPtr.Zero, 3, 0x02000000 | 0x00200000, IntPtr.Zero)) {
            if (h.IsInvalid) return (uint)Marshal.GetLastWin32Error();
            return SetSecurityInfo(h, 1, DACL_PROTECTED, IntPtr.Zero, IntPtr.Zero, Dacl(Sd(sddl)), IntPtr.Zero);
        }
    }
}
'@

function Sddl([string]$Path) { (Get-Acl -LiteralPath $Path).GetSecurityDescriptorSddlForm('Owner,Group,Access') }
function Rules([string]$Path) {
    $a = Get-Acl -LiteralPath $Path
    $rules = $a.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | ForEach-Object {
        '{0}|{1}|{2:X}|{3}|{4}|{5}' -f $_.IdentityReference.Value, $_.AccessControlType, [int]$_.FileSystemRights, $_.InheritanceFlags, $_.PropagationFlags, $_.IsInherited
    }
    "owner=$($a.GetOwner([Security.Principal.SecurityIdentifier]).Value) protected=$($a.AreAccessRulesProtected)"
    $rules | Sort-Object
}
function Wanted([string]$Sddl) {
    $s = New-Object Security.AccessControl.DirectorySecurity
    $s.SetSecurityDescriptorSddlForm($Sddl)
    $rules = $s.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | ForEach-Object {
        '{0}|{1}|{2:X}|{3}|{4}|{5}' -f $_.IdentityReference.Value, $_.AccessControlType, [int]$_.FileSystemRights, $_.InheritanceFlags, $_.PropagationFlags, $_.IsInherited
    }
    "owner=$($s.GetOwner([Security.Principal.SecurityIdentifier]).Value) protected=$($s.AreAccessRulesProtected)"
    $rules | Sort-Object
}
function Remove-Tree([string]$Path) { if (Test-Path -LiteralPath $Path) { Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Continue } }

$PD = $env:ProgramData
$FOLDER = 'O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;WD)'

if ($Only -match 'F12') {
# F1 ==================================================================================================================
Section "F1 a standard user creating a folder under $PD"
icacls $PD
$u = New-ProbeUser 'gfuser' -NoBatchRight
$made = Join-Path $PD 'GoetiaProbeF1'
Remove-Tree $made
New-Item -ItemType Directory -Force C:\probe | Out-Null
icacls C:\probe '/grant' 'Everyone:(OI)(CI)M' | Out-Null
$out = 'C:\probe\f1.out'
$cred = New-Object Management.Automation.PSCredential($u.Qualified, (ConvertTo-SecureString $u.Password -AsPlainText -Force))
try {
    $p = Start-Process -FilePath cmd.exe -ArgumentList '/c', "mkdir $made > $out 2>&1" -Credential $cred -WorkingDirectory C:\probe -LoadUserProfile -Wait -PassThru
    "cmd exit code $($p.ExitCode); output: $(Get-Content $out -Raw -ErrorAction SilentlyContinue)"
} catch { "Start-Process as the standard user FAILED: $($_.Exception.Message)" }
"folder exists after: $(Test-Path $made)"
if (Test-Path $made) { "owner: $((Get-Acl $made).Owner)"; Sddl $made; Remove-Tree $made }
Remove-ProbeUser $u

# F2 ==================================================================================================================
Section "F2 Goetia with the descriptor O:BA, and runs with the shim folder's descriptor"
$g = Join-Path $PD 'GoetiaProbeF2'
Remove-Tree $g
"whoami /groups (the Administrators line): $((whoami /groups | Select-String 'Administrators') -join ' / ')"
$r = [F]::MakeDir($g, 'O:BA')
"MakeDir(O:BA) win32 result $r"
if ($r -eq 0) {
    "sddl: $(Sddl $g)"
    Rules $g
    "reference, the same parent: a folder made by New-Item"
    $ref = Join-Path $PD 'GoetiaProbeF2ref'; Remove-Tree $ref; New-Item -ItemType Directory $ref | Out-Null
    "sddl: $(Sddl $ref)"; Rules $ref; Remove-Tree $ref
    $runs = Join-Path $g 'runs'
    $r2 = [F]::MakeDir($runs, $FOLDER)
    "MakeDir(runs, the shim folder's descriptor) win32 result $r2"
    if ($r2 -eq 0) {
        "sddl: $(Sddl $runs)"
        $actual = @(Rules $runs); $wanted = @(Wanted $FOLDER)
        "actual: "; $actual
        "wanted: "; $wanted
        "ordered-set compare of the actual and the wanted (owner, protection, entries): $(($actual -join ';') -eq ($wanted -join ';'))"
    }
}
Remove-Tree $g
}

# F3 ==================================================================================================================
if ($Only -match 'F3') {
Section "F3 the DACL call for runs\<id>: what it leaves alone, and what the new principal can still do"
New-Item -ItemType Directory -Force C:\probe | Out-Null
icacls C:\probe '/grant' 'Everyone:(OI)(CI)M' | Out-Null
$root = 'C:\probe\f3'
Remove-Tree $root; New-Item -ItemType Directory -Force $root | Out-Null
$ua = New-ProbeUser 'gfa' -NoBatchRight
$ub = New-ProbeUser 'gfb' -NoBatchRight
function Runs-Sddl([string]$Principal) {
    # the principal's inheritable entry is for the files made in the folder (inherit-only); its entry for the
    # folder itself has no add-subdirectory (0x4) and has delete-child (0x40)
    "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OIIO;0x1301bf;;;$Principal)(A;;0x1301fb;;;$Principal)"
}
function As($U, [string]$Command) {
    $cred = New-Object Management.Automation.PSCredential($U.Qualified, (ConvertTo-SecureString $U.Password -AsPlainText -Force))
    $o = "C:\probe\as-$($U.Name).out"; Remove-Item $o -ErrorAction SilentlyContinue
    $p = Start-Process -FilePath cmd.exe -ArgumentList '/c', "$Command > $o 2>&1" -Credential $cred -WorkingDirectory C:\probe -LoadUserProfile -Wait -PassThru
    "    as $($U.Name): $Command -> exit $($p.ExitCode) $((Get-Content $o -Raw -ErrorAction SilentlyContinue) -replace '\s+', ' ')"
}
# A victim: a file in an unprotected folder, so a propagating call would rewrite its inherited entries.
$vd = Join-Path $root 'victims'; New-Item -ItemType Directory $vd | Out-Null
foreach ($variant in 'named', 'handle', 'kernel') {
    "--- $variant"
    $d = Join-Path $root "id-$variant"
    $victim = Join-Path $vd "victim-$variant.txt"
    "x" | Set-Content $victim
    "MakeDir(id) win32 result $([F]::MakeDir($d, (Runs-Sddl $ua.Sid)))"
    "  the folder as made: $(Sddl $d)"
    As $ua "copy NUL $d\recA.json"
    "  recA.json as made by the first principal: $(Sddl (Join-Path $d 'recA.json'))"
    $child = Join-Path $d 'rec.json'
    "x" | Set-Content $child
    $link = Join-Path $d 'link.json'
    "HardLink(link -> victim) win32 result $([F]::HardLink($link, $victim))"
    $before = @{ dir = (Sddl $d); child = (Sddl $child); victim = (Sddl $victim); recA = (Sddl (Join-Path $d 'recA.json')) }
    $new = Runs-Sddl $ub.Sid
    $status = switch ($variant) { 'named' { [F]::SetDaclNamed($d, $new) } 'handle' { [F]::SetDaclHandle($d, $new) } 'kernel' { [F]::SetDaclKernel($d, $new) } }
    "set the DACL ($variant): win32 result $status"
    $after = @{ dir = (Sddl $d); child = (Sddl $child); victim = (Sddl $victim); recA = (Sddl (Join-Path $d 'recA.json')) }
    foreach ($k in 'dir', 'child', 'recA', 'victim') {
        "$k unchanged: $($before[$k] -eq $after[$k])"
        "  before: $($before[$k])"
        "  after : $($after[$k])"
    }
    "the folder, read back: $(Sddl $d)"
    "the new principal, after the change:"
    As $ub "copy NUL $d\recB.json"
    As $ub "del $d\recA.json"
    As $ub "mkdir $d\sub"
    "the first principal, after the change:"
    As $ua "copy NUL $d\recA2.json"
    "left in the folder: $((Get-ChildItem $d -Force | ForEach-Object Name) -join ', ')"
}
Remove-ProbeUser $ua; Remove-ProbeUser $ub
Remove-Tree $root
}
