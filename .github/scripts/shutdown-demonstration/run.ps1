# The shutdown demonstration: goetia's own shim, in a Windows Server Core container, stops a service when the container
# shuts down, as `goetia daemon stop` does. Windows CI only (the workflow `shutdown-demonstration.yaml`), on a runner
# whose Docker engine runs Windows containers; it starts and shuts down containers, so never anywhere else.
#
#   run.ps1 -Release <cargo target\release> -Work <scratch directory>
#
# Two `protocol: plain` services are installed with the `goetia` of -Release, inside the container, and started:
#   demo-asked     a program that writes a marker when it is asked to exit with Ctrl+C, and exits (default `timeout`);
#   demo-ignoring  a program that ignores every console event, with `timeout: 60s`.
# Then the container is shut down with `docker stop` and a --time far above 60 s. It passes when:
#   - the first program's marker is there and its shim log has the lines of an asked stop, and
#   - the second was ended by the shim at its `timeout` (the shim's own warning in its log), not earlier by Windows,
#     which cuts a service whose checkpoint stays put at about 45 s.
# The logs and markers are in a directory mounted into the container, so they outlive it.
param(
    [Parameter(Mandatory)][string]$Release,
    [Parameter(Mandatory)][string]$Work
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Image = 'mcr.microsoft.com/windows/servercore:ltsc2025'
$Here = $PSScriptRoot
$Bin = Join-Path $Work 'bin'
$Out = Join-Path $Work 'out'

# Run a native command; a failing exit code is an error.
function Invoke-Native {
    param([string]$What, [scriptblock]$Command)
    & $Command
    if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
}

# The engine may not answer at the job's first step: wait on its real answer, with no cap of ours (the step's own timeout
# is the bound).
function Wait-Engine {
    while ($true) {
        $service = Get-Service docker -ErrorAction SilentlyContinue
        if ($service -and $service.Status -eq 'Stopped') { Start-Service docker }
        docker info *> $null
        if ($LASTEXITCODE -eq 0) { break }
        Start-Sleep -Milliseconds 500
    }
}

# Wait until $Path exists: a file a program of the container writes once it is ready. No cap of ours (the step's own
# timeout is the bound); a container that has gone will never write it.
function Wait-File {
    param([string]$Path, [string]$Container)
    while (-not (Test-Path -LiteralPath $Path)) {
        if (-not (docker ps -q --no-trunc --filter "id=$Container")) { throw "the container is gone, and $Path was not written" }
        Start-Sleep -Milliseconds 200
    }
}

function Write-Manifest {
    param([string]$Name, [string]$Command, [string]$StopAction)
    Set-Content -LiteralPath (Join-Path $Out "$Name.yaml") -Encoding utf8 -Value @"
id: $Name
command: $Command
protocol: plain
logs: 'C:\out\$Name.log'
stop-action: $StopAction
"@
}

# Stage ------------------------------------------------------------------------------------------------------------------

New-Item -ItemType Directory -Force $Bin, $Out | Out-Null
$csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
foreach ($program in 'idle', 'daemon') {
    Invoke-Native "compile $program.cs" { & $csc -nologo -platform:x64 "-out:$Bin\$program.exe" (Join-Path $Here "$program.cs") }
}
Copy-Item (Join-Path $Release 'goetia.exe'), (Join-Path $Release 'goetia-shim.exe') $Bin
# The container's accounts read the programs and write the logs and markers.
Invoke-Native 'grant access to the programs' { icacls $Bin /grant '*S-1-1-0:(OI)(CI)RX' }
Invoke-Native 'grant access to the output' { icacls $Out /grant '*S-1-1-0:(OI)(CI)F' }

Write-Manifest 'demo-asked' "['C:\bin\daemon.exe', marker, 'C:\out\asked.marker', 'C:\out\asked.ready']" '{ send-signal: ctrl-c }'
Write-Manifest 'demo-ignoring' "['C:\bin\daemon.exe', ignore, 'C:\out\ignoring.ready']" '{ send-signal: ctrl-c, timeout: 60s }'

$version = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
Write-Host "HOST: $($version.ProductName) build $($version.CurrentBuild).$($version.UBR)"
Wait-Engine
Invoke-Native "pull $Image" { docker pull $Image }
Invoke-Native 'inspect the image' { docker image inspect $Image --format 'IMAGE: {{index .RepoDigests 0}} osversion={{.OsVersion}}' }
Invoke-Native 'read the engine' { docker info --format 'ENGINE: {{.ServerVersion}} DefaultIsolation={{.Isolation}}' }
Get-ChildItem $Bin | Format-Table Name, Length | Out-Host

# Run --------------------------------------------------------------------------------------------------------------------

$container = $null
try {
    $container = (docker run -d --rm --network none --isolation=process -v "${Bin}:C:\bin" -v "${Out}:C:\out" $Image C:\bin\idle.exe C:\out).Trim()
    if ($LASTEXITCODE -ne 0) { throw "docker run failed with exit code $LASTEXITCODE" }
    Write-Host "CONTAINER: $container"
    Wait-File (Join-Path $Out 'ready') $container

    foreach ($name in 'demo-asked', 'demo-ignoring') {
        Invoke-Native "install $name" { docker exec $container C:\bin\goetia.exe daemon install -f "C:\out\$name.yaml" --start }
    }
    Wait-File (Join-Path $Out 'asked.ready') $container
    Wait-File (Join-Path $Out 'ignoring.ready') $container
    Invoke-Native 'list the daemons' { docker exec $container C:\bin\goetia.exe daemon list }

    Write-Host 'SHUTDOWN: docker stop --time 900'
    $elapsed = Measure-Command {
        docker stop --time 900 $container | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "docker stop failed with exit code $LASTEXITCODE" }
    }
    Write-Host ("SHUTDOWN: the container was gone after {0:N1} s" -f $elapsed.TotalSeconds)
}
finally {
    # Only the container this script made.
    if ($container) { docker rm -f $container *> $null }
}

# Judge ------------------------------------------------------------------------------------------------------------------

foreach ($log in 'demo-asked.log', 'demo-ignoring.log') {
    Write-Host "::group::$log"
    Get-Content -LiteralPath (Join-Path $Out $log)
    Write-Host '::endgroup::'
}

$failures = @()
function Expect {
    param([string]$What, [bool]$Held)
    Write-Host ("{0,-6} {1}" -f $(if ($Held) { 'PASS' } else { 'FAIL' }), $What)
    if (-not $Held) { $script:failures += $What }
}

$asked = Get-Content -Raw -LiteralPath (Join-Path $Out 'demo-asked.log')
$ignoring = Get-Content -Raw -LiteralPath (Join-Path $Out 'demo-ignoring.log')
$marker = Join-Path $Out 'asked.marker'
Expect 'the first program wrote its marker when it was asked to exit' (Test-Path -LiteralPath $marker)
if (Test-Path -LiteralPath $marker) { Write-Host "       marker: $(Get-Content -LiteralPath $marker)" }
Expect "the first program's shim log has the asked stop's lines" (
    ($asked -match 'asking the daemon to exit with Ctrl\+C') -and ($asked -match 'exited within the wait'))
Expect "the second program was ended by the shim at its timeout (the shim's own line), not earlier by Windows" (
    ($ignoring -match 'asking the daemon to exit with Ctrl\+C') -and
    ($ignoring -match 'process tree had not been seen to exit when its timeout of 1m was over'))

if ($failures.Count -gt 0) { throw "the demonstration failed: $($failures -join '; ')" }
Write-Host 'The demonstration passed.'
