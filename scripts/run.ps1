$ErrorActionPreference = "Stop"

$Root = if ($env:HERDR_PLUGIN_ROOT) {
    $env:HERDR_PLUGIN_ROOT
} else {
    Split-Path -Parent $PSScriptRoot
}
$Bin = Join-Path $Root "bin\herdr-tab-title.exe"

if (Test-Path $Bin) {
    & $Bin @args
    exit $LASTEXITCODE
}

if (Get-Command cargo -ErrorAction SilentlyContinue) {
    Push-Location $Root
    try {
        & cargo run --quiet --release -- @args
        exit $LASTEXITCODE
    } finally {
        Pop-Location
    }
}

Write-Error "herdr-tab-title.exe is missing and cargo is not available."
Write-Error "Run scripts\install-binary.ps1 from the plugin directory."
exit 127
