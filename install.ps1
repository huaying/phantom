# Phantom Remote Desktop — Windows install script
# Usage: irm https://raw.githubusercontent.com/huaying/phantom/main/install.ps1 | iex

$ErrorActionPreference = "Stop"

$repo = "huaying/phantom"
$installDir = if ($env:PHANTOM_INSTALL_DIR) {
    $env:PHANTOM_INSTALL_DIR
} else {
    "$env:LOCALAPPDATA\phantom"
}

# Create install directory
if (!(Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
}

# Download binaries via `/releases/latest/download/NAME` redirect by default.
# Test/CI can override the source without changing installer behavior:
#   $env:PHANTOM_SERVER_EXE="C:\tmp\phantom-server.exe"
#   $env:PHANTOM_CLIENT_EXE="C:\tmp\phantom-client.exe"
#   $env:PHANTOM_LOCAL_ASSET_DIR="C:\tmp\phantom-assets"
#   $env:PHANTOM_ASSET_BASE_URL="http://10.0.0.1:8000"
#   $env:PHANTOM_INSTALL_DIR="C:\tmp\phantom-bin"
#   $env:PHANTOM_NO_PATH=1
$baseUrl = if ($env:PHANTOM_ASSET_BASE_URL) {
    $env:PHANTOM_ASSET_BASE_URL.TrimEnd("/")
} else {
    "https://github.com/$repo/releases/latest/download"
}

function Copy-PhantomAsset {
    param(
        [string]$Source,
        [string]$Destination,
        [string]$Label
    )

    if (!(Test-Path $Source)) {
        throw "local Phantom asset not found: $Source"
    }

    Write-Host "Using local $Label..." -ForegroundColor Cyan
    Copy-Item -Path $Source -Destination $Destination -Force
    Write-Host "  -> $Destination" -ForegroundColor Green
}

function Copy-PhantomAssetOverride {
    param(
        [string]$File,
        [string]$LocalName,
        [string]$Destination,
        [bool]$Required
    )

    $direct = $null
    if ($LocalName -eq "phantom-server.exe") {
        $direct = $env:PHANTOM_SERVER_EXE
    } elseif ($LocalName -eq "phantom-client.exe") {
        $direct = $env:PHANTOM_CLIENT_EXE
    }

    if ($direct) {
        Copy-PhantomAsset -Source $direct -Destination $Destination -Label $LocalName
        return $true
    }

    if ($env:PHANTOM_LOCAL_ASSET_DIR) {
        $candidates = @(
            (Join-Path $env:PHANTOM_LOCAL_ASSET_DIR $File),
            (Join-Path $env:PHANTOM_LOCAL_ASSET_DIR $LocalName)
        )
        foreach ($candidate in $candidates) {
            if (Test-Path $candidate) {
                Copy-PhantomAsset -Source $candidate -Destination $Destination -Label $File
                return $true
            }
        }

        if ($Required) {
            throw "required Phantom asset not found in PHANTOM_LOCAL_ASSET_DIR: $File"
        }
        Write-Host "  -> Skipped optional local asset ($File not found in PHANTOM_LOCAL_ASSET_DIR)" -ForegroundColor Yellow
        return $true
    }

    return $false
}

function Invoke-DownloadPhantomAsset {
    param(
        [string]$File,
        [bool]$Required
    )

    $localName = $File -replace "-windows-x86_64", ""
    $dest = Join-Path $installDir $localName

    if (Copy-PhantomAssetOverride -File $File -LocalName $localName -Destination $dest -Required $Required) {
        return $true
    }

    $url = "$baseUrl/$File"
    Write-Host "Downloading $File..." -ForegroundColor Cyan
    try {
        Invoke-WebRequest -Uri $url -OutFile $dest -UseBasicParsing
        Write-Host "  -> $dest" -ForegroundColor Green
        return $true
    } catch {
        if ($Required) {
            Write-Host "  -> Failed to download required asset: $File" -ForegroundColor Red
            throw
        }
        Write-Host "  -> Skipped optional asset ($File not available)" -ForegroundColor Yellow
        return $false
    }
}

Invoke-DownloadPhantomAsset -File "phantom-server-windows-x86_64.exe" -Required $true | Out-Null
Invoke-DownloadPhantomAsset -File "phantom-client-windows-x86_64.exe" -Required $false | Out-Null

$noPath = $env:PHANTOM_NO_PATH -eq "1"

if ($noPath) {
    Write-Host ""
    Write-Host "Skipping PATH update (PHANTOM_NO_PATH=1)." -ForegroundColor Yellow
} else {
    # Add to PATH if not already there
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -notlike "*$installDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$installDir", "User")
        Write-Host ""
        Write-Host "Added $installDir to your PATH." -ForegroundColor Green
        Write-Host "Restart your terminal for PATH changes to take effect." -ForegroundColor Yellow
    }
}

Write-Host ""
Write-Host "Done! Phantom (latest) installed to $installDir" -ForegroundColor Green

# Auto-register as a Windows Service when elevated. This mirrors what
# install.sh does on Linux (XDG autostart by default) and gives users a
# "one command = installed + running + auto-starts on boot" experience.
# Registering the service needs Administrator because `sc create` talks
# to the Service Control Manager. Opt out with $env:PHANTOM_NO_AUTOSTART=1.
$noAutostart = $env:PHANTOM_NO_AUTOSTART -eq "1"
$noDoctor = $env:PHANTOM_NO_DOCTOR -eq "1"
$doctorStrict = $env:PHANTOM_DOCTOR_STRICT -eq "1"
$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltinRole]::Administrator)

function Add-DoctorResult {
    param(
        [string]$Level,
        [string]$Message
    )

    switch ($Level) {
        "OK" {
            Write-Host "  OK: $Message" -ForegroundColor Green
        }
        "WARN" {
            $script:doctorWarns += 1
            Write-Host "  WARN: $Message" -ForegroundColor Yellow
        }
        "FAIL" {
            $script:doctorFails += 1
            Write-Host "  FAIL: $Message" -ForegroundColor Red
        }
    }
}

function Test-ListeningPort {
    param([int]$Port)

    try {
        $conn = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction Stop |
            Select-Object -First 1
        return $null -ne $conn
    } catch {
        try {
            $lines = netstat -ano -p tcp 2>$null
            return [bool]($lines | Select-String -Pattern "[:.]$Port\s+.*LISTENING")
        } catch {
            return $false
        }
    }
}

function Test-VddPresent {
    try {
        $devices = Get-PnpDevice -Class Display -ErrorAction Stop
        foreach ($device in $devices) {
            if (($device.FriendlyName -eq "Virtual Display Driver") -or
                ($device.InstanceId -like "*MttVDD*")) {
                return $true
            }
        }
    } catch {
        try {
            $text = pnputil /enum-devices /class Display 2>$null
            if ($LASTEXITCODE -eq 0 -and ($text -match "MttVDD|Virtual Display Driver")) {
                return $true
            }
        } catch {}
    }
    return $false
}

function Test-ActiveUserSession {
    try {
        $text = @(query user 2>&1)
        # On some OpenSSH/Windows builds, `query user` prints valid session
        # rows but still exits non-zero. Treat the output as authoritative.
        if ($text -match "\s+Active\s+") {
            return $true
        }
    } catch {}
    return $false
}

function Test-ConsoleSessionAvailable {
    try {
        $text = @(qwinsta 2>&1)
        foreach ($line in $text) {
            if (($line -match "^\s*>?\s*console\s+") -and ($line -match "\s+(Active|Conn)\s+")) {
                return $true
            }
        }
    } catch {}
    return $false
}

function Get-PhantomRuntimeEvidence {
    param([int]$FreshMinutes = 15)

    $cutoff = (Get-Date).AddMinutes(-1 * $FreshMinutes)
    $svcLog = Join-Path $env:WINDIR "Temp\phantom-debug.log"
    $agentLog = Join-Path $env:WINDIR "Temp\phantom-agent.log"
    $evidence = [ordered]@{
        ServiceLogPath = $svcLog
        AgentLogPath = $agentLog
        ServiceLogExists = $false
        AgentLogExists = $false
        ServiceLogRecent = $false
        AgentLogRecent = $false
        AgentConnected = $false
        CaptureEvidence = $false
    }

    if (Test-Path $svcLog) {
        $evidence.ServiceLogExists = $true
        $evidence.ServiceLogRecent = (Get-Item $svcLog).LastWriteTime -gt $cutoff
        $tail = @(Get-Content $svcLog -Tail 250 -ErrorAction SilentlyContinue)
        $evidence.AgentConnected = [bool]($tail -match "IPC: agent connected")
        $evidence.CaptureEvidence = [bool]($tail -match "capture=|Tier 1:|Tier 2:|Tier 3:|dxgi_nvenc|gdi")
    }

    if (Test-Path $agentLog) {
        $evidence.AgentLogExists = $true
        $evidence.AgentLogRecent = (Get-Item $agentLog).LastWriteTime -gt $cutoff
        $tail = @(Get-Content $agentLog -Tail 250 -ErrorAction SilentlyContinue)
        if ($tail -match "Tier 1:|Tier 2:|Tier 3:|capture=|dxgi_nvenc|gdi") {
            $evidence.CaptureEvidence = $true
        }
    }

    return [pscustomobject]$evidence
}

function Test-ExpectedLocalProbeIsolationFailure {
    param([object[]]$ProbeOutput)

    $text = ($ProbeOutput | Out-String)
    return [bool]($text -match "no displays found|No displays found|Session 0|cannot capture screen from Session 0")
}

function Enable-PhantomServiceRecovery {
    $service = Get-Service -Name "PhantomServer" -ErrorAction SilentlyContinue
    if ($null -eq $service) {
        return
    }

    Write-Host "Configuring Phantom service crash recovery..." -ForegroundColor Cyan
    & sc.exe failure PhantomServer reset= 86400 actions= restart/5000/restart/5000/restart/30000 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "sc failure PhantomServer failed with exit code $LASTEXITCODE"
    }
    & sc.exe failureflag PhantomServer 1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "sc failureflag PhantomServer failed with exit code $LASTEXITCODE"
    }
}

function Test-PhantomServiceRecovery {
    try {
        $text = & sc.exe qfailure PhantomServer 2>$null
        if ($LASTEXITCODE -ne 0) {
            return $false
        }
        return [bool]($text | Select-String -Pattern "RESTART")
    } catch {
        return $false
    }
}

function Enable-PhantomCrashDumps {
    $dumpDir = Join-Path $env:ProgramData "Phantom\Dumps"
    New-Item -ItemType Directory -Path $dumpDir -Force | Out-Null

    $dumpKey = "HKLM:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\phantom-server.exe"
    New-Item -Path $dumpKey -Force | Out-Null
    New-ItemProperty -Path $dumpKey -Name "DumpFolder" -Value $dumpDir -PropertyType ExpandString -Force | Out-Null
    New-ItemProperty -Path $dumpKey -Name "DumpType" -Value 2 -PropertyType DWord -Force | Out-Null
    New-ItemProperty -Path $dumpKey -Name "DumpCount" -Value 5 -PropertyType DWord -Force | Out-Null
}

function Test-PhantomCrashDumps {
    try {
        $dumpKey = "HKLM:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\phantom-server.exe"
        $props = Get-ItemProperty -Path $dumpKey -ErrorAction Stop
        return (($props.DumpType -eq 2) -and ($props.DumpCount -ge 1) -and (Test-Path $props.DumpFolder))
    } catch {
        return $false
    }
}

function Stop-ExistingPhantomProcessesForInstall {
    param([string]$ProgramFilesServer)

    $service = Get-Service -Name "PhantomServer" -ErrorAction SilentlyContinue
    if (($null -ne $service) -and ($service.Status -eq "Running")) {
        Write-Host "Stopping existing PhantomServer before update..." -ForegroundColor Cyan
        & sc.exe stop PhantomServer | Out-Null
        for ($i = 0; $i -lt 30; $i++) {
            Start-Sleep -Milliseconds 500
            $service = Get-Service -Name "PhantomServer" -ErrorAction SilentlyContinue
            if (($null -eq $service) -or ($service.Status -ne "Running")) {
                break
            }
        }
    }

    if (-not (Test-Path $ProgramFilesServer)) {
        return
    }

    $procs = @()
    try {
        $procs = @(Get-CimInstance Win32_Process -Filter "Name='phantom-server.exe'" -ErrorAction Stop |
            Where-Object {
                $_.ExecutablePath -and
                $_.ExecutablePath.Equals($ProgramFilesServer, [StringComparison]::OrdinalIgnoreCase)
            })
    } catch {}

    if ($procs.Count -eq 0) {
        return
    }

    Write-Host "Stopping stale Phantom service/agent processes before update..." -ForegroundColor Cyan
    foreach ($proc in $procs) {
        try {
            Stop-Process -Id $proc.ProcessId -Force -ErrorAction Stop
        } catch {}
    }

    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 500
        try {
            $remaining = @(Get-CimInstance Win32_Process -Filter "Name='phantom-server.exe'" -ErrorAction Stop |
                Where-Object {
                    $_.ExecutablePath -and
                    $_.ExecutablePath.Equals($ProgramFilesServer, [StringComparison]::OrdinalIgnoreCase)
                })
            if ($remaining.Count -eq 0) {
                return
            }
        } catch {
            return
        }
    }

    Write-Host "  Warning: stale Phantom process may still be running; continuing install attempt." -ForegroundColor Yellow
}

function Invoke-PhantomDoctor {
    param(
        [string]$ServerExe,
        [bool]$IsAdmin
    )

    $script:doctorFails = 0
    $script:doctorWarns = 0

    Write-Host ""
    Write-Host "Running Phantom Windows doctor..." -ForegroundColor Cyan
    $runtimeEvidence = Get-PhantomRuntimeEvidence

    if (Test-Path $ServerExe) {
        Add-DoctorResult "OK" "phantom-server.exe found at $ServerExe"
    } else {
        Add-DoctorResult "FAIL" "phantom-server.exe not found at $ServerExe"
    }

    if ($IsAdmin) {
        Add-DoctorResult "OK" "installer is running elevated"
    } else {
        Add-DoctorResult "WARN" "installer is not elevated; service/VDD setup was skipped"
    }

    $programFilesServer = Join-Path $env:ProgramFiles "Phantom\phantom-server.exe"
    if (Test-Path $programFilesServer) {
        Add-DoctorResult "OK" "service binary exists at $programFilesServer"
    } elseif ($IsAdmin -and -not $noAutostart) {
        Add-DoctorResult "FAIL" "service binary missing at $programFilesServer"
    } else {
        Add-DoctorResult "WARN" "service binary not installed; ad-hoc current-user mode only"
    }

    $service = Get-Service -Name "PhantomServer" -ErrorAction SilentlyContinue
    if ($null -ne $service) {
        Add-DoctorResult "OK" "Windows Service registered (status=$($service.Status))"
        try {
            $serviceInfo = Get-CimInstance -ClassName Win32_Service -Filter "Name='PhantomServer'" -ErrorAction Stop
            if ($null -ne $serviceInfo) {
                $expectedPath = [regex]::Escape($programFilesServer)
                if ($serviceInfo.PathName -match $expectedPath) {
                    Add-DoctorResult "OK" "Windows Service points at Program Files binary"
                } else {
                    Add-DoctorResult "FAIL" "Windows Service points at unexpected binary: $($serviceInfo.PathName)"
                }
            }
        } catch {
            Add-DoctorResult "WARN" "could not verify Windows Service binary path"
        }
        if ($service.Status -eq "Running") {
            Add-DoctorResult "OK" "Windows Service is running"
        } elseif ($IsAdmin -and -not $noAutostart) {
            Add-DoctorResult "FAIL" "Windows Service is not running; run: sc start PhantomServer"
        } else {
            Add-DoctorResult "WARN" "Windows Service is not running; reboot or run: sc start PhantomServer"
        }

        if (Test-PhantomServiceRecovery) {
            Add-DoctorResult "OK" "Windows Service crash recovery is configured"
        } elseif ($IsAdmin -and -not $noAutostart) {
            Add-DoctorResult "FAIL" "Windows Service crash recovery is not configured"
        } else {
            Add-DoctorResult "WARN" "Windows Service crash recovery is not configured"
        }
    } elseif ($IsAdmin -and -not $noAutostart) {
        Add-DoctorResult "FAIL" "Windows Service is not registered"
    } else {
        Add-DoctorResult "WARN" "Windows Service is not registered"
    }

    if ($IsAdmin -and -not $noAutostart) {
        if (Test-PhantomCrashDumps) {
            Add-DoctorResult "OK" "Windows Error Reporting crash dumps are enabled"
        } else {
            Add-DoctorResult "WARN" "Windows Error Reporting crash dumps are not enabled"
        }
    }

    if (Test-VddPresent) {
        Add-DoctorResult "OK" "Virtual Display Driver is present"
    } else {
        Add-DoctorResult "WARN" "Virtual Display Driver was not detected; headless Windows may black-screen until VDD installs/reboot completes"
    }

    $activeUserSession = Test-ActiveUserSession
    $consoleSessionAvailable = Test-ConsoleSessionAvailable
    if ($activeUserSession) {
        Add-DoctorResult "OK" "active interactive user session detected"
    } elseif ($consoleSessionAvailable) {
        Add-DoctorResult "OK" "console session detected; service agent can capture pre-login/Winlogon desktop"
    } else {
        Add-DoctorResult "WARN" "no console session detected; service agent cannot capture until Windows creates a console session"
    }

    $browserPortListening = Test-ListeningPort 9901
    if ($browserPortListening) {
        Add-DoctorResult "OK" "browser port 9901 is listening"
    } elseif ($null -ne $service -and $service.Status -eq "Running") {
        Add-DoctorResult "WARN" "service is running but browser port 9901 is not listening yet"
    } else {
        Add-DoctorResult "WARN" "browser port 9901 is not listening"
    }

    $serviceAgentUsable = (
        ($null -ne $service) -and
        ($service.Status -eq "Running") -and
        $browserPortListening -and
        $runtimeEvidence.ServiceLogRecent -and
        $runtimeEvidence.AgentConnected
    )

    $probeExe = $ServerExe
    if (Test-Path $programFilesServer) {
        $probeExe = $programFilesServer
    }

    if (Test-Path $probeExe) {
        $probeHelp = @()
        try {
            $probeHelp = & $probeExe --help 2>&1
        } catch {
            $probeHelp = @($_.Exception.Message)
        }

        if ($probeHelp -match "--probe-capture") {
            Write-Host "  Probe: $probeExe --probe-capture" -ForegroundColor DarkGray
            $probeOutput = @()
            try {
                $probeOutput = & $probeExe --probe-capture --fps 5 --bitrate 1000 2>&1
                $probeCode = $LASTEXITCODE
            } catch {
                $probeOutput = @($_.Exception.Message)
                $probeCode = 1
            }

            $interesting = $probeOutput | Select-String -Pattern "Phantom capture probe:|resolved:|gpu_probe:|windows:|ccd:|display\[|zero_copy:|dxgi_nvenc:|fallback:|frame:|encode:|Capture probe result:|Error:|Caused by:"
            foreach ($line in $interesting) {
                Write-Host "    $line" -ForegroundColor DarkGray
            }

            if (($probeCode -eq 0) -and ($probeOutput -match "Capture probe result: pass")) {
                Add-DoctorResult "OK" "capture probe produced an encoded frame"
            } elseif ($probeOutput -match "Capture probe result: mostly-black") {
                Add-DoctorResult "FAIL" "capture probe produced a mostly black frame"
            } elseif ($serviceAgentUsable -and (Test-ExpectedLocalProbeIsolationFailure -ProbeOutput $probeOutput)) {
                Add-DoctorResult "WARN" "local capture probe cannot see displays from this installer/SSH context; service agent is connected and browser port is listening"
            } else {
                $firstError = ($probeOutput | Select-String -Pattern "Error:|Caused by:" | Select-Object -First 1)
                if ($null -ne $firstError) {
                    Add-DoctorResult "FAIL" "capture probe failed: $firstError"
                } else {
                    Add-DoctorResult "FAIL" "capture probe failed with exit code $probeCode"
                }
            }
        } else {
            Add-DoctorResult "WARN" "installed phantom-server.exe does not support --probe-capture; skipping capture probe"
        }
    }

    if ($null -ne $service -and $service.Status -eq "Running") {
        if ($runtimeEvidence.ServiceLogExists) {
            if ($runtimeEvidence.ServiceLogRecent -and $runtimeEvidence.AgentConnected) {
                Add-DoctorResult "OK" "service recently connected to a session agent"
            } else {
                Add-DoctorResult "WARN" "service log does not show a recent agent IPC connection"
            }
        } else {
            Add-DoctorResult "WARN" "service debug log not found at $($runtimeEvidence.ServiceLogPath)"
        }
        if ($runtimeEvidence.CaptureEvidence) {
            if ($runtimeEvidence.AgentLogExists) {
                Add-DoctorResult "OK" "agent recently selected a capture tier"
            } else {
                Add-DoctorResult "OK" "service log shows recent agent capture activity"
            }
        } elseif ($runtimeEvidence.AgentLogExists -or $runtimeEvidence.ServiceLogExists) {
            Add-DoctorResult "WARN" "recent logs do not show agent capture activity"
        } else {
            Add-DoctorResult "WARN" "agent log not found at $($runtimeEvidence.AgentLogPath)"
        }
    }

    Write-Host ""
    Write-Host "Doctor summary: failures=$script:doctorFails warnings=$script:doctorWarns" -ForegroundColor Cyan
    if ($script:doctorFails -gt 0) {
        Write-Host "Doctor result: failed" -ForegroundColor Red
        return $false
    }

    Write-Host "Doctor result: pass" -ForegroundColor Green
    return $true
}

$serverExe = Join-Path $installDir "phantom-server.exe"
$serverExeExists = Test-Path $serverExe
if ($noAutostart) {
    Write-Host ""
    Write-Host "Skipping service registration (PHANTOM_NO_AUTOSTART=1)." -ForegroundColor Yellow
    Write-Host "Register later with: phantom-server.exe --install" -ForegroundColor Cyan
} elseif (-not $serverExeExists) {
    Write-Host ""
    Write-Host "phantom-server.exe not installed - skipping service registration." -ForegroundColor Yellow
} elseif (-not $isAdmin) {
    Write-Host ""
    Write-Host "Not running as Administrator - skipping service registration." -ForegroundColor Yellow
    Write-Host "To finish the install (auto-start on boot, pre-login access), re-run in an" -ForegroundColor Yellow
    Write-Host "elevated PowerShell, or run:" -ForegroundColor Yellow
    Write-Host "  phantom-server.exe --install" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "Or start the server ad-hoc (current user session only):" -ForegroundColor Yellow
    Write-Host "  phantom-server.exe" -ForegroundColor Cyan
} else {
    Write-Host ""
    Write-Host "Registering Windows Service + installing Virtual Display Driver..." -ForegroundColor Cyan
    $programFilesServerForInstall = Join-Path $env:ProgramFiles "Phantom\phantom-server.exe"
    Stop-ExistingPhantomProcessesForInstall -ProgramFilesServer $programFilesServerForInstall
    & $serverExe --install
    if ($LASTEXITCODE -eq 0) {
        Enable-PhantomServiceRecovery
        Enable-PhantomCrashDumps
        Write-Host ""
        Write-Host "Phantom is registered and running as a Windows Service." -ForegroundColor Green
        Write-Host "  Status:  sc query PhantomServer" -ForegroundColor DarkGray
        Write-Host "  Remove:  phantom-server.exe --uninstall" -ForegroundColor DarkGray
    } else {
        Write-Host ""
        Write-Host "Service registration exited with code $LASTEXITCODE." -ForegroundColor Yellow
        Write-Host "Re-run manually to see the error: phantom-server.exe --install" -ForegroundColor Cyan
        throw "phantom-server.exe --install failed with exit code $LASTEXITCODE"
    }
}

if ($noDoctor) {
    Write-Host ""
    Write-Host "Skipping Windows doctor (PHANTOM_NO_DOCTOR=1)." -ForegroundColor Yellow
} elseif (Test-Path $serverExe) {
    $doctorOk = Invoke-PhantomDoctor -ServerExe $serverExe -IsAdmin $isAdmin
    if ((-not $doctorOk) -and $doctorStrict) {
        exit 1
    }
} else {
    Write-Host ""
    Write-Host "Skipping Windows doctor because phantom-server.exe is not installed." -ForegroundColor Yellow
}
