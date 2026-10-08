$ErrorActionPreference = 'Continue'
$src = (Resolve-Path 'probe/svc/target/debug/probe-svc.exe').Path
$p4 = (Resolve-Path 'probe/svc/target/debug/probe4.exe').Path
$dir = Join-Path $env:ProgramData 'goetiaprobe'
New-Item -ItemType Directory -Force $dir | Out-Null
$exe = Join-Path $dir 'probe-svc.exe'
Copy-Item $src $exe
"=== ACL of the copy (inherited from %ProgramData%, no icacls grant)"
(icacls $exe) | ForEach-Object { "  $_" }

"=== race: shim-like exit (service thread exit(6) vs main exit(0)), 40 stops"
sc.exe create p4-race binPath= "`"$exe`" p4-race specific6" | Out-Null
& $p4 race p4-race 40

"=== linger: STOPPED(6) reported, process never exits"
sc.exe create p4-linger binPath= "`"$exe`" p4-linger linger" | Out-Null
& $p4 linger p4-linger

net user goetiaprobeuser 'Pr0be-Passw0rd!x' /add | Out-Null
& $p4 grant "$env:COMPUTERNAME\goetiaprobeuser"
$accounts = [ordered]@{
  'p4-localsystem' = @();
  'p4-virtual'     = @('obj=', 'NT SERVICE\p4-virtual');
  'p4-localsvc'    = @('obj=', 'NT AUTHORITY\LocalService');
  'p4-netsvc'      = @('obj=', 'NT AUTHORITY\NetworkService');
  'p4-user'        = @('obj=', ".\goetiaprobeuser", 'password=', 'Pr0be-Passw0rd!x')
}
foreach ($name in $accounts.Keys) {
  "=== pin under $name $($accounts[$name] -join ' ')"
  $args = @('create', $name, 'binPath=', "`"$exe`" $name specific6") + $accounts[$name]
  (sc.exe @args) | Select-Object -Last 1 | ForEach-Object { "  sc create: $_" }
  & $p4 pin $name
}
foreach ($name in @('p4-race', 'p4-linger') + $accounts.Keys) { sc.exe stop $name | Out-Null; sc.exe delete $name | Out-Null }
