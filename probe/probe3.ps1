$ErrorActionPreference = 'Continue'
$exe = (Resolve-Path 'probe/svc/target/debug/probe-svc.exe').Path
$p3 = (Resolve-Path 'probe/svc/target/debug/probe3.exe').Path
$svcs = @{ 'goetiap3-h1' = 'specific6'; 'goetiap3-h2' = 'specific6'; 'goetiap3-m1' = 'pendstop'; 'goetiap3-m1s' = 'pendstart'; 'goetiap3-m3' = 'clean' }
foreach ($k in $svcs.Keys) { sc.exe create $k binPath= "`"$exe`" $k $($svcs[$k])" | Out-Null }
$t0 = Get-Date
& $p3
"=== H2: System event log entries since the probe began (7036, 7040, 7042) for goetiap3-h2"
Get-WinEvent -FilterHashtable @{ LogName = 'System'; StartTime = $t0 } -ErrorAction SilentlyContinue |
  Where-Object { $_.Message -match 'goetiap3-h2' } |
  ForEach-Object { "  id=$($_.Id) $($_.TimeCreated.ToString('HH:mm:ss.fff')) $($_.Message -replace '\s+', ' ')" }
foreach ($k in $svcs.Keys) { sc.exe stop $k | Out-Null; sc.exe delete $k | Out-Null }
