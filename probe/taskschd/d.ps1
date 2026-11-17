# D: after a task is registered again in place as another account (RegisterTask with TASK_UPDATE, as goetia's update of
# a task does), does IRegisteredTask.GetSecurityDescriptor name the first account or the second? Also the same after a
# delete and a create as the second.
#
# Claim (note: "Windows - RegisterTask with TASK_UPDATE to another account changes the principal and keeps the task's
# security descriptor, which names the first account only"): the XML principal changes with an update, the descriptor
# keeps the first account's `(A;;FR;;;LS)` entry and has no `NS` one; a delete and a create gives `(A;;FR;;;NS)`.
# Platform: GitHub windows-latest runner (Windows Server 2025, image windows-2025-vs2026 20261103.298.1), elevated.
# Throwaway Windows CI runner only: it registers a task at \GoetiaProbeD and deletes it.
#
# A correct run prints, in this order:
#   created as LocalService               SD ... (A;;FR;;;LS)   XML principal: S-1-5-19
#   updated in place to NetworkService    SD ... (A;;FR;;;LS)   XML principal: S-1-5-20
#   deleted, then created as NetworkService  SD ... (A;;FR;;;NS)   XML principal: S-1-5-20
$ErrorActionPreference = 'Stop'
$svc = New-Object -ComObject Schedule.Service
$svc.Connect()
$root = $svc.GetFolder('\')
$missing = [System.Reflection.Missing]::Value

function Xml($sid) {
    @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers />
  <Principals><Principal id="Author"><UserId>$sid</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings><Enabled>true</Enabled></Settings>
  <Actions Context="Author"><Exec><Command>C:\Windows\System32\cmd.exe</Command><Arguments>/c exit 0</Arguments></Exec></Actions>
</Task>
"@
}
function Show($label, $name) {
    $task = $root.GetTask($name)
    "--- $label"
    "SD    : " + $task.GetSecurityDescriptor(0xF)
    $xml = [xml]$task.Xml
    "XML principal: " + $xml.Task.Principals.Principal.UserId
}

$name = '\GoetiaProbeD'
try {
    "LS = S-1-5-19 (SDDL LS), NS = S-1-5-20 (SDDL NS)"
    [void]$root.RegisterTask($name, (Xml 'S-1-5-19'), 2, 'S-1-5-19', $missing, 5, $missing)
    Show 'created as LocalService' $name
    [void]$root.RegisterTask($name, (Xml 'S-1-5-20'), 4, 'S-1-5-20', $missing, 5, $missing)
    Show 'updated in place to NetworkService' $name
    $root.DeleteTask($name, 0)
    [void]$root.RegisterTask($name, (Xml 'S-1-5-20'), 2, 'S-1-5-20', $missing, 5, $missing)
    Show 'deleted, then created as NetworkService' $name
}
finally {
    try { $root.DeleteTask($name, 0) } catch { }
}
