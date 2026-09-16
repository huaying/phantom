param([string]$InstallerPath = (Join-Path $PSScriptRoot '..\..\install.ps1'))
$ErrorActionPreference = 'Stop'

# Load only the pure evidence predicates, never the installer's top-level
# downloads, service setup, registry changes or process handling.
$tokens = $null; $parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    (Resolve-Path $InstallerPath), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw ($parseErrors | Out-String) }
foreach ($name in @('Test-PhantomWinlogonLogReady', 'Test-PhantomSecureDesktopReady', 'Test-VddPresent', 'Test-VddEnabled')) {
    $fn = $ast.Find({ param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
    }, $true)
    if ($null -eq $fn) { throw "Missing installer function: $name" }
    . ([scriptblock]::Create($fn.Extent.Text))
}

$script:checks = 0
function Assert-Result([string]$Name, [bool]$Actual, [bool]$Expected) {
    if ($Actual -ne $Expected) { throw "$Name expected $Expected, got $Actual" }
    $script:checks++
}

$ready = @(
    '[1.0s] Session/desktop changed: 0/None -> 2/Winlogon',
    '[1.1s] Candidate agent generation 1 launched PID=900',
    '[1.2s] Tier 3: GDI+OpenH264 1920x1080',
    '[1.3s] Candidate generation 1 capture ready: 1920x1080 bytes=25000',
    '[1.3s] Committed agent generation 1 for session 2/Winlogon; retiring generation 0'
)
Assert-Result 'first keyframe committed on current Winlogon' (Test-PhantomWinlogonLogReady $ready) $true
Assert-Result 'launch without first frame is insufficient' (Test-PhantomWinlogonLogReady $ready[0..2]) $false
Assert-Result 'candidate ready without commit is insufficient' (Test-PhantomWinlogonLogReady $ready[0..3]) $false
Assert-Result 'Default transition supersedes secure desktop' (Test-PhantomWinlogonLogReady ($ready + '[2.0s] Session/desktop changed: 2/Winlogon -> 2/Default')) $false
Assert-Result 'new console needs its own first frame' (Test-PhantomWinlogonLogReady ($ready + '[2.0s] Session/desktop changed: 2/Winlogon -> 3/Winlogon')) $false
Assert-Result 'later Default commit supersedes old Winlogon' (Test-PhantomWinlogonLogReady ($ready + '[2.0s] Committed agent generation 2 for session 2/Default; retiring generation 1')) $false
Assert-Result 'empty log is not ready' (Test-PhantomWinlogonLogReady @()) $false

$evidence = [pscustomobject]@{ ServiceLogRecent=$true; AgentConnected=$true; CaptureEvidence=$true; WinlogonCaptureReady=$true }
Assert-Result 'live secure desktop may defer provisioning' (Test-PhantomSecureDesktopReady $evidence $true $true $true) $true
foreach ($field in @('ServiceLogRecent', 'AgentConnected', 'CaptureEvidence', 'WinlogonCaptureReady')) {
    $evidence.$field = $false
    Assert-Result "missing $field fails closed" (Test-PhantomSecureDesktopReady $evidence $true $true $true) $false
    $evidence.$field = $true
}
Assert-Result 'missing console fails closed' (Test-PhantomSecureDesktopReady $evidence $false $true $true) $false
Assert-Result 'stopped service fails closed' (Test-PhantomSecureDesktopReady $evidence $true $false $true) $false
Assert-Result 'missing browser listener fails closed' (Test-PhantomSecureDesktopReady $evidence $true $true $false) $false

# Exercise device observations without changing the test host's PnP state.
function Get-PnpDevice {
    [CmdletBinding()] param([string]$Class)
    $script:displayDevices
}
function Get-PnpDeviceProperty {
    [CmdletBinding()] param([string]$InstanceId, [string]$KeyName)
    if ($script:problemQueryFails) { throw 'device state unavailable' }
    [pscustomobject]@{ Data=$script:problemCodes[$InstanceId] }
}
$script:problemQueryFails=$false
$script:displayDevices=@([pscustomobject]@{FriendlyName='AWS Indirect Display Device';InstanceId='aws'})
$script:problemCodes=@{mtt=0;otherMtt=0}
Assert-Result 'AWS display alone needs no Phantom VDD' (Test-VddEnabled) $false
$script:displayDevices+=([pscustomobject]@{FriendlyName='Virtual Display Driver';InstanceId='mtt'})
Assert-Result 'enabled MTT must be reported beside AWS' (Test-VddEnabled) $true
$script:problemCodes.mtt=22
Assert-Result 'retained disabled MTT does not own a display' (Test-VddEnabled) $false
$script:problemQueryFails=$true
Assert-Result 'unknown device state preserves coexistence warning' (Test-VddEnabled) $true
$script:problemQueryFails=$false
$script:displayDevices+=([pscustomobject]@{FriendlyName='Virtual Display Driver';InstanceId='otherMtt'})
Assert-Result 'one disabled node must not hide a second enabled VDD' (Test-VddEnabled) $true
Write-Output "Windows doctor regression: $script:checks checks passed"
