# Phantom Remote Desktop — Windows install script
# Usage: irm https://raw.githubusercontent.com/huaying/phantom/main/install.ps1 | iex

$ErrorActionPreference = "Stop"

$repo = "huaying/phantom"
$installDir = "$env:LOCALAPPDATA\phantom"

# Create install directory
if (!(Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
}

# Get latest release tag
Write-Host "Fetching latest release..." -ForegroundColor Cyan
$release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
$version = $release.tag_name
Write-Host "Latest version: $version" -ForegroundColor Green

# Download binaries
$baseUrl = "https://github.com/$repo/releases/download/$version"

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
Write-Host "Done! Phantom $version installed to $installDir" -ForegroundColor Green
Write-Host ""
Write-Host "Quick start:" -ForegroundColor Cyan
Write-Host "  phantom-server.exe --no-encrypt --transport web"
Write-Host "  # then open https://localhost:9900 in browser"
Write-Host ""
Write-Host "Note: Windows server builds without embedded web client." -ForegroundColor Yellow
Write-Host "      Use --transport tcp with native client instead," -ForegroundColor Yellow
Write-Host "      or build from source with wasm-pack for web access." -ForegroundColor Yellow
