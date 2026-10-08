$ErrorActionPreference = 'Continue'
$exe = (Resolve-Path 'probe/svc/target/debug/probe-svc.exe').Path
$n2 = (Resolve-Path 'probe/svc/target/debug/notify2.exe').Path
$name = 'goetianotify2'
sc.exe create $name binPath= "`"$exe`" $name specific6" | Out-Null
& $n2 $name
sc.exe stop $name | Out-Null
sc.exe delete $name | Out-Null
