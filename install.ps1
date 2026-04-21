# Phantom Remote Desktop — Windows install script
# Usage: irm https://raw.githubusercontent.com/huaying/phantom/main/install.ps1 | iex

$ErrorActionPreference = "Stop"

$repo = "huaying/phantom"
$installDir = "$env:LOCALAPPDATA\phantom"

# Create install directory
if (!(Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
}

# Download binaries via `/releases/latest/download/NAME` redirect.
# This avoids the GitHub API rate limit (60 unauthenticated req/hr/IP)
# that `/repos/:owner/:repo/releases/latest` imposes.
$baseUrl = "https://github.com/$repo/releases/latest/download"

$files = @("phantom-server-windows-x86_64.exe", "phantom-client-windows-x86_64.exe")
foreach ($file in $files) {
    $localName = $file -replace "-windows-x86_64", ""
    $url = "$baseUrl/$file"
    $dest = Join-Path $installDir $localName
    Write-Host "Downloading $file..." -ForegroundColor Cyan
    try {
        Invoke-WebRequest -Uri $url -OutFile $dest -UseBasicParsing
        Write-Host "  -> $dest" -ForegroundColor Green
    } catch {
        Write-Host "  -> Skipped ($file not available)" -ForegroundColor Yellow
    }
}

# Add to PATH if not already there
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$installDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$installDir", "User")
    Write-Host ""
    Write-Host "Added $installDir to your PATH." -ForegroundColor Green
    Write-Host "Restart your terminal for PATH changes to take effect." -ForegroundColor Yellow
}

Write-Host ""
Write-Host "Done! Phantom (latest) installed to $installDir" -ForegroundColor Green

# Auto-register as a Windows Service when elevated. This mirrors what
# install.sh does on Linux (XDG autostart by default) and gives users a
# "one command = installed + running + auto-starts on boot" experience.
# Registering the service needs Administrator because `sc create` talks
# to the Service Control Manager. Opt out with $env:PHANTOM_NO_AUTOSTART=1.
$noAutostart = $env:PHANTOM_NO_AUTOSTART -eq "1"
$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltinRole]::Administrator)

$serverExe = Join-Path $installDir "phantom-server.exe"
if ($noAutostart) {
    Write-Host ""
    Write-Host "Skipping service registration (PHANTOM_NO_AUTOSTART=1)." -ForegroundColor Yellow
    Write-Host "Register later with: phantom-server.exe --install" -ForegroundColor Cyan
} elseif (-not (Test-Path $serverExe)) {
    Write-Host ""
    Write-Host "phantom-server.exe not installed — skipping service registration." -ForegroundColor Yellow
} elseif ($isAdmin) {
    Write-Host ""
    Write-Host "Registering Windows Service + installing Virtual Display Driver..." -ForegroundColor Cyan
    & $serverExe --install
    if ($LASTEXITCODE -eq 0) {
        Write-Host ""
        Write-Host "Phantom is registered and running as a Windows Service." -ForegroundColor Green
        Write-Host "  Status:  sc query PhantomServer" -ForegroundColor DarkGray
        Write-Host "  Remove:  phantom-server.exe --uninstall" -ForegroundColor DarkGray
    } else {
        Write-Host ""
        Write-Host "Service registration exited with code $LASTEXITCODE." -ForegroundColor Yellow
        Write-Host "Re-run manually to see the error: phantom-server.exe --install" -ForegroundColor Cyan
    }
} else {
    Write-Host ""
    Write-Host "Not running as Administrator — skipping service registration." -ForegroundColor Yellow
    Write-Host "To finish the install (auto-start on boot, pre-login access), re-run in an" -ForegroundColor Yellow
    Write-Host "elevated PowerShell, or run:" -ForegroundColor Yellow
    Write-Host "  phantom-server.exe --install" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "Or start the server ad-hoc (current user session only):" -ForegroundColor Yellow
    Write-Host "  phantom-server.exe" -ForegroundColor Cyan
}
