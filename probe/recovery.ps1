# Does SCM run failure actions after a REQUESTED stop that ends non-zero?
# 20s WaitForStatus is this probe's own failure bound.
$ErrorActionPreference = 'Continue'
$exe = (Resolve-Path 'probe/svc/target/debug/probe-svc.exe').Path
foreach ($mode in 'clean', 'specific6', 'crash') {
  $name = "goetiarecov-$mode"
  sc.exe create $name binPath= "`"$exe`" $name $mode" | Out-Null
  sc.exe failure $name reset= 86400 actions= restart/1000 | Out-Null
  sc.exe failureflag $name 1 | Out-Null
  "=== mode=$mode (failure actions restart/1000ms, failureflag 1)"
  Start-Service $name
  $pid1 = (Get-CimInstance Win32_Service -Filter "Name='$name'").ProcessId
  "  running pid=$pid1"
  sc.exe stop $name | Out-Null
  (Get-Service $name).WaitForStatus('Stopped', '00:00:20')
  $w = Get-CimInstance Win32_Service -Filter "Name='$name'"
  "  after stop: State=$($w.State) ExitCode=$($w.ExitCode) Specific=$($w.ServiceSpecificExitCode)"
  try { (Get-Service $name).WaitForStatus('Running', '00:00:20'); $restarted = $true } catch { $restarted = $false }
  $w = Get-CimInstance Win32_Service -Filter "Name='$name'"
  "  restarted by SCM within 20s: $restarted (State=$($w.State) pid=$($w.ProcessId))"
  if ($restarted) { sc.exe failure $name reset= 0 actions= '""' | Out-Null; sc.exe stop $name | Out-Null; (Get-Service $name).WaitForStatus('Stopped', '00:00:20') }
  sc.exe delete $name | Out-Null
}
