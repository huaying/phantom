<#
.SYNOPSIS
Opt one diagnosed AWS speaker endpoint into polling, or restore its backup.
.DESCRIPTION
Administrative compatibility repair for a verified driver event-mode defect.
This is not run by Phantom or the installer. It restarts Windows Audio and
interrupts current audio streams; reconnect clients after applying it.
Use the testing runbook's independent audio probe before and after this change.
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory = $true)]
    [guid]$EndpointId,
    [switch]$Restore,
    [string]$BackupDirectory = (Join-Path $env:ProgramData 'Phantom\audio')
)

$ErrorActionPreference = 'Stop'
$endpoint = $EndpointId.ToString('B')
$registryPath = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\$endpoint\Properties"
$eventProperty = '{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},7'
$nameProperty = '{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
$instanceProperty = '{b3f8fa53-0004-438e-9003-51a46e139bfc},2'
$backupPath = Join-Path $BackupDirectory ($EndpointId.ToString() + '.json')

$readKey = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($registryPath)
if ($null -eq $readKey) { throw "Render endpoint $endpoint does not exist" }
try {
    $name = $readKey.GetValue($nameProperty)
    $instance = $readKey.GetValue($instanceProperty)
    $original = $readKey.GetValue($eventProperty)
} finally { $readKey.Dispose() }

if ($name -ne 'AWS Virtual Speakers 7.1 Device' -or $instance -notmatch '^\{1\}\.ROOT\\MEDIA\\[0-9]+$') {
    throw 'This repair is restricted to an explicitly selected AWS virtual speaker endpoint'
}
if ($original -notin @(0, 1)) { throw 'Unexpected event-mode property value' }
$deviceInstanceId = $instance.Substring(4)
if ((Get-PnpDevice -InstanceId $deviceInstanceId).Status -ne 'OK') {
    throw 'Repair device readiness before changing its audio mode'
}

$backup = $null
if (Test-Path -LiteralPath $backupPath) {
    $backup = Get-Content -LiteralPath $backupPath -Raw | ConvertFrom-Json
    if ($backup.schemaVersion -ne 1 -or $backup.endpointId -ne $endpoint -or
        $backup.deviceInstanceId -ne $deviceInstanceId -or $backup.originalEventMode -ne 1) {
        throw 'Existing backup does not match this endpoint and its original event mode'
    }
}
if ($Restore -and $null -eq $backup) { throw "No verified backup at $backupPath" }
$targetMode = if ($Restore) { [int]$backup.originalEventMode } else { 0 }
if ($original -eq $targetMode) {
    [pscustomobject]@{ endpointId = $endpoint; eventMode = $original; changed = $false }
    return
}

$audio = Get-Service Audiosrv
if ($audio.Status -ne 'Running') { throw 'Windows Audio must be running before this repair' }
if (@($audio.DependentServices | Where-Object { $_.Status -eq 'Running' }).Count -ne 0) {
    throw 'Windows Audio has active dependent services; refusing a broader restart'
}
if (-not $PSCmdlet.ShouldProcess($endpoint, "Set audio event mode to $targetMode and restart Windows Audio")) {
    return
}

if ($null -eq $backup) {
    New-Item -ItemType Directory -Path $BackupDirectory -Force | Out-Null
    [ordered]@{
        schemaVersion = 1
        endpointId = $endpoint
        deviceInstanceId = $deviceInstanceId
        originalEventMode = $original
        createdUtc = [DateTime]::UtcNow.ToString('o')
    } | ConvertTo-Json | Set-Content -LiteralPath $backupPath -Encoding UTF8
}

# PowerShell's registry provider asks for broader write rights than this key
# grants. Request only the existing administrator QueryValues/SetValue rights.
# ReadWriteSubTree also marks the .NET handle writable; no ACL is changed.
$rights = [Security.AccessControl.RegistryRights]::QueryValues -bor [Security.AccessControl.RegistryRights]::SetValue
$writeKey = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey(
    $registryPath, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
if ($null -eq $writeKey) { throw 'Cannot open the endpoint for the requested repair' }
$changed = $false
try {
    $writeKey.SetValue($eventProperty, $targetMode, [Microsoft.Win32.RegistryValueKind]::DWord)
    $changed = $true
    Restart-Service Audiosrv
    (Get-Service Audiosrv).WaitForStatus('Running', [TimeSpan]::FromSeconds(15))
    if ($writeKey.GetValue($eventProperty) -ne $targetMode) { throw 'Audio mode verification failed' }
} catch {
    if ($changed) {
        $writeKey.SetValue($eventProperty, $original, [Microsoft.Win32.RegistryValueKind]::DWord)
        # Recreate the engine after restoring the old value, even if the first
        # restart completed before a later verification failure.
        Restart-Service Audiosrv
        (Get-Service Audiosrv).WaitForStatus('Running', [TimeSpan]::FromSeconds(15))
    }
    throw
} finally { $writeKey.Dispose() }

[pscustomobject]@{
    endpointId = $endpoint
    deviceInstanceId = $deviceInstanceId
    eventMode = $targetMode
    changed = $true
    backupPath = $backupPath
    audioService = (Get-Service Audiosrv).Status.ToString()
}
