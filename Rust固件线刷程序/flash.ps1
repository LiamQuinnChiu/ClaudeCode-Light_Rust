$ErrorActionPreference = 'Stop'

# 固定路径：flash/ 的上一级就是项目根
$root = Split-Path -Parent $PSScriptRoot
if (-not $root) { $root = (Get-Location).Path }

# 固件：../Firmware/*.bin
$image = Get-ChildItem -Path (Join-Path $root 'Firmware') -Filter '*.bin' -File -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $image) {
    Write-Host "[ERROR] No .bin in Firmware/" -ForegroundColor Red
    exit 1
}
$image = $image.FullName

# espflash：../bin/espflash.exe 或 PATH
$espflash = Join-Path $root 'bin\espflash.exe'
if (-not (Test-Path $espflash)) {
    $found = Get-Command espflash -ErrorAction SilentlyContinue
    if ($found) { $espflash = $found.Source }
    else {
        Write-Host '[INFO] Downloading espflash...' -ForegroundColor Yellow
        $binDir = Join-Path $root 'bin'
        New-Item -Path $binDir -ItemType Directory -Force | Out-Null
        $zip = Join-Path $binDir 'espflash.zip'
        Invoke-WebRequest -Uri 'https://github.com/esp-rs/espflash/releases/latest/download/espflash-x86_64-pc-windows-msvc.zip' -OutFile $zip -UseBasicParsing
        Expand-Archive -Path $zip -DestinationPath $binDir -Force
        Remove-Item $zip -Force
    }
}

Write-Host ''
Write-Host '=== CursorLight flash ===' -ForegroundColor Cyan
Write-Host "Firmware: $image"
Write-Host "espflash: $espflash"
Write-Host ''

$espArgs = @('write-bin', '-S', '-B', '460800', '0x10000', $image)
& $espflash @espArgs

if ($LASTEXITCODE -ne 0) {
    Write-Host '[FAILED] Hold BOOT + replug USB, retry.' -ForegroundColor Red
    exit $LASTEXITCODE
}

Write-Host '[OK] Flashed. LED should start demo mode.' -ForegroundColor Green
Write-Host 'OTA rollback preserved (ota_1 untouched).' -ForegroundColor DarkGray
