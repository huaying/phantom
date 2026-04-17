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
Write-Host ""
Write-Host "Quick start:" -ForegroundColor Cyan
Write-Host "  phantom-server.exe"
Write-Host "  # TCP:9900 (native client) + Web:9901 (browser: https://localhost:9901)"
Write-Host ""
Write-Host "Install as Windows Service (auto-start, pre-login access):" -ForegroundColor Cyan
Write-Host "  phantom-server.exe --install"
