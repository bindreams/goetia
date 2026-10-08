$ErrorActionPreference = 'Continue'
$exe = (Resolve-Path 'probe/svc/target/debug/probe-svc.exe').Path
$notify = (Resolve-Path 'probe/svc/target/debug/notify-probe.exe').Path
function Show($name) {
  "  sc query:"; (sc.exe query $name) | Where-Object { $_ -match 'STATE|EXIT_CODE' } | ForEach-Object { "    $_" }
  $w = Get-CimInstance Win32_Service -Filter "Name='$name'"
  "  Win32_Service: State=$($w.State) Status=$($w.Status) ExitCode=$($w.ExitCode) ServiceSpecificExitCode=$($w.ServiceSpecificExitCode)"
  "  Get-Service Status=$((Get-Service $name).Status)"
}
function StopWith($tool, $name) {
  switch ($tool) {
    'sc' { $o = (sc.exe stop $name 2>&1) -join ' | '; $rc = $LASTEXITCODE; (Get-Service $name).WaitForStatus('Stopped', '00:00:30') }
    'net' { $o = (net stop $name 2>&1) -join ' | '; $rc = $LASTEXITCODE }
    'Stop-Service' { try { $o = (Stop-Service $name -ErrorAction Stop 2>&1) -join ' | '; $rc = 'no-error' } catch { $o = $_.ToString(); $rc = 'ERROR' } }
  }
  "  rc=$rc"; "  out=$o"
}
foreach ($mode in 'clean', 'specific6', 'crash') {
  foreach ($tool in 'sc', 'net', 'Stop-Service') {
    $name = "goetiaprobe-$mode-$($tool -replace '-', '')"
    sc.exe create $name binPath= "`"$exe`" $name $mode" | Out-Null
    "=== mode=$mode tool=$tool"
    "  fresh, never started:"; Show $name
    Start-Service $name
    "  -- first stop"; StopWith $tool $name; Show $name
    "  -- second stop (already stopped)"; StopWith $tool $name; Show $name
    if ($tool -eq 'sc') { "  -- notify on an already-STOPPED service"; & $notify $name }
    sc.exe delete $name | Out-Null
  }
}
