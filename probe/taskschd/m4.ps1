# M4: what IRegisteredTask.Xml returns of what was registered; whether it is a fixed point; Data byte for byte;
# Date, Author, URI; task path case; GetTasks with the hidden flag. Throwaway Windows CI runner only.
. "$PSScriptRoot\common.ps1"
Section "platform"; Show-Platform
Initialize-Probe
$user = New-ProbeUser 'gprobem4'
$svc = Get-ProbeService
Remove-ProbeTasks $svc
$folder = Get-ProbeFolder $svc

# Flatten an XML document to ordered `path = value` lines (attributes too), for a structural comparison.
function Flatten([string]$xml) {
    $d = New-Object System.Xml.XmlDocument
    $d.PreserveWhitespace = $true
    $d.LoadXml($xml)
    $out = New-Object System.Collections.Generic.List[string]
    function Walk($n, $path) {
        foreach ($c in $n.ChildNodes) {
            if ($c.NodeType -eq 'Element') {
                $p = "$path/$($c.LocalName)"
                foreach ($a in $c.Attributes) { if ($a.Name -notlike 'xmlns*') { $out.Add("$p@$($a.Name) = $($a.Value)") } }
                $text = ($c.ChildNodes | Where-Object { $_.NodeType -in 'Text', 'CDATA' } | ForEach-Object { $_.Value }) -join ''
                $hasEl = [bool]($c.ChildNodes | Where-Object { $_.NodeType -eq 'Element' })
                $out.Add($(if ($hasEl) { "$p" } else { "$p = [$text]" }))
                Walk $c $p
            }
        }
    }
    Walk $d ''
    , $out.ToArray()
}

# The text of /Task/Data as an XML parser reads it out of the readback (CDATA and entities resolved).
function XmlData([string]$xml) {
    $d = New-Object System.Xml.XmlDocument; $d.PreserveWhitespace = $true; $d.LoadXml($xml)
    $n = $d.GetElementsByTagName('Data')
    if ($n.Count -gt 0) { $n[0].InnerText } else { $null }
}

$P = Get-Principal 'system'
$submitted = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Date>2020-02-02T02:02:02</Date>
    <Author>someone-else</Author>
    <Version>9.9</Version>
    <Description>desc</Description>
    <Documentation>doc</Documentation>
    <Source>src</Source>
    <URI>\Wrong\Path</URI>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>2030-01-01T03:30:00</StartBoundary>
      <Enabled>true</Enabled>
      <Repetition><Interval>PT15M</Interval><Duration>PT8H</Duration><StopAtDurationEnd>false</StopAtDurationEnd></Repetition>
      <ScheduleByMonthDayOfWeek><Weeks><Week>1</Week><Week>Last</Week></Weeks><DaysOfWeek><Monday/><Friday/></DaysOfWeek><Months><June/><March/></Months></ScheduleByMonthDayOfWeek>
    </CalendarTrigger>
    <CalendarTrigger>
      <StartBoundary>2030-01-01T04:00:00</StartBoundary>
      <Enabled>false</Enabled>
      <ScheduleByMonth><DaysOfMonth><Day>31</Day><Day>Last</Day></DaysOfMonth><Months><January/><December/></Months></ScheduleByMonth>
    </CalendarTrigger>
    <TimeTrigger><StartBoundary>2030-06-15T12:00:00</StartBoundary></TimeTrigger>
  </Triggers>
  <Principals><Principal id="Author"><UserId>S-1-5-19</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <Enabled>true</Enabled>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>C:\probe\bin\tsprobe.exe</Command><Arguments>act noop --goetia-task my-id</Arguments><WorkingDirectory>C:\probe</WorkingDirectory></Exec></Actions>
  <Data>GOETIA-BLOB line one
line two</Data>
</Task>
"@

Section "M4.1 what is registered, and what Xml returns (full text of both)"
$t = Register-ProbeTask $folder 'rich' $submitted $P
$read1 = $t.Xml
"----- submitted"; $submitted
"----- Xml (readback 1)"; $read1

Section "M4.2 structural diff, submitted -> readback (<= is only in the submission, => only in the readback)"
$a = Flatten $submitted; $b = Flatten $read1
Compare-Object -ReferenceObject $a -DifferenceObject $b | ForEach-Object { "$($_.SideIndicator) $($_.InputObject)" }
"element order: the readback's paths, in order:"
$b -join "`n"

Section "M4.3 is the readback a fixed point? register the readback, read again"
$t2 = Register-ProbeTask $folder 'rich' $read1 $P 4
$read2 = $t2.Xml
"readback 2 equals readback 1 byte for byte: $($read2 -ceq $read1)"
if ($read2 -cne $read1) { Compare-Object (Flatten $read1) (Flatten $read2) | ForEach-Object { "$($_.SideIndicator) $($_.InputObject)" } }
"Date / Author / URI of readback 1: " + (($read1 -split "`n" | Where-Object { $_ -match '<(Date|Author|URI|Source|Version)>' }) -join ' ')
"Date / Author / URI of readback 2: " + (($read2 -split "`n" | Where-Object { $_ -match '<(Date|Author|URI|Source|Version)>' }) -join ' ')

Section "M4.4 Data, byte for byte"
$cases = [ordered]@{
    'lf'        = "a`nb`nc"
    'crlf-as-char-refs' = $null   # filled below: submitted as &#13;&#10;
    'literal-crlf-in-xml' = "a`r`nb"
    'big-64k'   = ('GOETIA-BLOB-' + ('x' * 65536))
    'spaces-tabs' = "  lead`tTab  trail  "
    'unicode'   = "é 日本語 😀"
    'xml-specials' = 'a & b < c > d ]]> e " f'' g'
    'json'      = '{"marker":"goetia","spec":"a\\nb"}'
    'empty'     = ''
}
foreach ($k in @($cases.Keys)) {
    if ($k -eq 'crlf-as-char-refs') { $dataXml = "<Data>a&#13;&#10;b&#13;&#10;c</Data>"; $want = "a`r`nb`r`nc" }
    elseif ($k -eq 'literal-crlf-in-xml') { $dataXml = "<Data>a`r`nb</Data>"; $want = $cases[$k] }
    else { $want = $cases[$k]; $dataXml = "<Data>$(Esc $want)</Data>" }
    try {
        $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop' -DataXml $dataXml
        $t = Register-ProbeTask $folder "data-$k" $xml $P
        $viaDef = $t.Definition.Data
        $m = [regex]::Match($t.Xml, '(?s)<Data>(.*?)</Data>')
        $viaXml = XmlData $t.Xml
        $wantBytes = [Text.Encoding]::UTF8.GetBytes($want)
        $defBytes = if ($null -ne $viaDef) { [Text.Encoding]::UTF8.GetBytes($viaDef) } else { @() }
        "data[$k]: Definition.Data equals what XML parsing gives: $($viaDef -ceq $want) (len want $($want.Length), got $($viaDef.Length)); Xml has a <Data> element: $($m.Success); its decoded text equals: $($viaXml -ceq $want)"
        if ($viaDef -cne $want) { "   want bytes: $([BitConverter]::ToString($wantBytes[0..([math]::Min(40, $wantBytes.Length - 1))])) got bytes: $([BitConverter]::ToString($defBytes[0..([math]::Min(40, [math]::Max(0, $defBytes.Length - 1)))]))" }
    } catch { "data[$k]: REFUSED $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
}
"--- Definition.Data set through COM (the other way in):"
foreach ($k in 'lf', 'crlf-as-char-refs', 'big-64k', 'unicode') {
    $want = if ($k -eq 'crlf-as-char-refs') { "a`r`nb`r`nc" } else { $cases[$k] }
    try {
        $xml = New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop'
        $t = Register-ProbeTask $folder "datacom-$k" $xml $P
        $d = $t.Definition; $d.Data = $want
        $t = $folder.RegisterTaskDefinition("datacom-$k", $d, 4, $null, $null, 0)
        $m = [regex]::Match($t.Xml, '(?s)<Data>(.*?)</Data>')
        "datacom[$k]: Definition.Data back equals: $($t.Definition.Data -ceq $want); Xml carries it as an element: $($m.Success); decoded equal: $((XmlData $t.Xml) -ceq $want)"
        if ($k -eq 'crlf-as-char-refs') { "   Xml's Data element text: $($m.Groups[1].Value)" }
    } catch { "datacom[$k]: FAILED $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
}
"--- Data in RegistrationInfo (the design's first guess):"
try {
    $xml = (New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop' -RegistrationExtra '<Data>inside-registrationinfo</Data>')
    $null = Register-ProbeTask $folder 'data-in-reginfo' $xml $P
    "registered"
} catch { "REFUSED: $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }

Section "M4.4b a minimal registration: what Task Scheduler adds (Date, Author, URI, defaults)"
$xml = New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop' -Triggers '<TimeTrigger><StartBoundary>2030-06-15T12:00:00</StartBoundary></TimeTrigger>'
$t = Register-ProbeTask $folder 'minimal' $xml $P
"----- submitted"; $xml
"----- readback"; $t.Xml
"----- readback as a user task (password logon)"
$PU = Get-Principal 'user' $user
$xml = New-TaskXml -P $PU -Command $Script:Exe -Arguments 'act noop'
$t = Register-ProbeTask $folder 'minimal-user' $xml $PU
$t.Xml

Section "M4.5 task path case"
$xml = New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop'
$t = Register-ProbeTask $folder 'MiXeD-Case' $xml $P
"registered as MiXeD-Case: Name=$($t.Name) Path=$($t.Path)"
foreach ($q in '\goetiaprobe\mixed-case', '\GOETIAPROBE\MIXED-CASE') {
    try { $g = $svc.GetFolder((Split-Path $q -Parent)).GetTask((Split-Path $q -Leaf)); "GetTask via $q : found, Name=$($g.Name) Path=$($g.Path)" } catch { "GetTask via $q : FAILED $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
}
try { $null = $folder.RegisterTask('mixed-case', $xml, 2, $null, $null, 0, $null); "TASK_CREATE of 'mixed-case' over MiXeD-Case: ok (distinct)" } catch { "TASK_CREATE of 'mixed-case' over MiXeD-Case: FAILED $($_.Exception.Message) [$(HexOf $_.Exception.HResult)]" }
$names = @($folder.GetTasks(1) | ForEach-Object { $_.Name })
"names in the folder now: $($names -join ', ')"

Section "M4.6 GetTasks with the hidden flag"
$xml = New-TaskXml -P $P -Command $Script:Exe -Arguments 'act noop' -Hidden $true
$null = Register-ProbeTask $folder 'hid' $xml $P
"GetTasks(0): $(@($folder.GetTasks(0) | ForEach-Object { $_.Name }) -join ', ')"
"GetTasks(1): $(@($folder.GetTasks(1) | ForEach-Object { $_.Name }) -join ', ')"
$h = $folder.GetTask('hid'); "GetTask('hid') direct: found, Hidden in Xml: $($h.Xml -match '<Hidden>true</Hidden>'), Definition.Settings.Hidden=$($h.Definition.Settings.Hidden)"

Remove-ProbeTasks $svc
Remove-ProbeUser $user
