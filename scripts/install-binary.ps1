$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
$BinDir = Join-Path $Root "bin"
$Bin = Join-Path $BinDir "herdr-tab-title.exe"
$Repo = if ($env:HERDR_TAB_TITLE_REPO) { $env:HERDR_TAB_TITLE_REPO } else { "daanzu/herdr-tab-title" }
$Manifest = Join-Path $Root "herdr-plugin.toml"
$VersionMatch = Select-String -Path $Manifest -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
$Version = $VersionMatch.Matches[0].Groups[1].Value

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

$asset = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "herdr-tab-title-x86_64-pc-windows-msvc.exe"; break }
    # ARM64 falls back to a local build until a matching release asset is published.
    "ARM64" { ""; break }
    default { "" }
}

if ($asset -and (Get-Command Invoke-WebRequest -ErrorAction SilentlyContinue)) {
    $url = "https://github.com/$Repo/releases/download/v$Version/$asset"
    $tmp = "$Bin.download"
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $tmp
        Move-Item -Force $tmp $Bin
        Write-Host "installed $asset"
        exit 0
    } catch {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmp
        Write-Warning "could not download $asset; building locally instead"
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "no release binary for this platform and cargo is not available"
}

Push-Location $Root
try {
    cargo build --locked --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
    Copy-Item -Force "target\release\herdr-tab-title.exe" $Bin
    Write-Host "built $Bin"
} finally {
    Pop-Location
}
