# End-to-end latency benchmark: hook process -> Named Pipe -> daemon -> BLE -> ESP32
#
# Usage:
#   .\tools\bench-latency.ps1                                # installed daemon
#   .\tools\bench-latency.ps1 -LogPath <path> -PipeClient <path>
#   .\tools\bench-latency.ps1 -GapMs 120                     # dense switching (debounce pressure)
#
# NOTE: intentionally ASCII-only. Windows PowerShell 5.1 decodes scripts as ANSI
# unless a BOM is present, so non-ASCII bytes here would break parsing.
param(
    [int]$Rounds = 8,
    [int]$GapMs = 1200,
    [string]$LogPath = (Join-Path $env:USERPROFILE '.cursorlight\bin\cursorlight_daemon.log'),
    [string]$PipeClient = (Join-Path $env:USERPROFILE '.cursorlight\bin\pipe-client.exe')
)

$ErrorActionPreference = 'Continue'

function Get-Median([double[]]$values) {
    if ($values.Count -eq 0) { return [double]::NaN }
    $s = $values | Sort-Object
    $n = $s.Count
    if ($n % 2 -eq 1) { return $s[[int](($n - 1) / 2)] }
    return ($s[$n / 2 - 1] + $s[$n / 2]) / 2
}

function Show-Stats([string]$name, $list) {
    if ($list.Count -eq 0) { Write-Host ("  {0,-22} (no samples)" -f $name); return }
    $a = $list.ToArray()
    $min = ($a | Measure-Object -Minimum).Minimum
    $max = ($a | Measure-Object -Maximum).Maximum
    $med = Get-Median $a
    Write-Host ("  {0,-22} n={1,-3} min={2,6:N1}  median={3,6:N1}  max={4,6:N1}" -f $name, $a.Count, $min, $med, $max)
}

if (-not (Test-Path $LogPath)) { Write-Host "log not found: $LogPath" -ForegroundColor Red; exit 1 }
if (-not (Test-Path $PipeClient)) { Write-Host "pipe-client not found: $PipeClient" -ForegroundColor Red; exit 1 }

$before = @(Get-Content $LogPath).Count

& $PipeClient --ping | Out-Null
if ($LASTEXITCODE -ne 0) { Write-Host 'daemon pipe not reachable' -ForegroundColor Red; exit 1 }

# Alternate between two modes so every message is a real change
# (identical modes are short-circuited by the debounce).
$modes = @('thinking', 'busy')
$json = Join-Path $env:TEMP 'bench-hook.json'
$clientMs = New-Object System.Collections.Generic.List[double]
$wallMs = New-Object System.Collections.Generic.List[double]

Write-Host ("bench: rounds={0} gap={1}ms" -f $Rounds, $GapMs)
Write-Host ("log:  {0}" -f $LogPath)

for ($i = 0; $i -lt $Rounds * 2; $i++) {
    $action = $modes[$i % 2]
    [System.IO.File]::WriteAllText($json, '{"session_id":"bench","tool_name":"Bash","permission_mode":"default"}', [System.Text.UTF8Encoding]::new($false))
    $line = '"' + $PipeClient + '" --stdin --action ' + $action + ' --timing < "' + $json + '"'
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $out = & cmd /c $line
    $sw.Stop()
    $wallMs.Add($sw.Elapsed.TotalMilliseconds)
    $m = [regex]::Match(($out -join ' '), 'sent in ([0-9.]+)ms')
    if ($m.Success) { $clientMs.Add([double]$m.Groups[1].Value) }
    if ($i -lt ($Rounds * 2 - 1)) { Start-Sleep -Milliseconds $GapMs }
}

Start-Sleep -Milliseconds 600
$new = @(Get-Content $LogPath | Select-Object -Skip $before)
$lat = New-Object System.Collections.Generic.List[double]
$gatt = New-Object System.Collections.Generic.List[double]
foreach ($l in $new) {
    $m = [regex]::Match($l, 'Latency: mode=(\w+) hook_to_ble=(\d+)ms gatt=(\d+)ms')
    if ($m.Success) {
        $lat.Add([double]$m.Groups[2].Value)
        $gatt.Add([double]$m.Groups[3].Value)
    }
}

Write-Host '--- results (ms) ---'
Show-Stats 'pipe-client in-process' $clientMs
Show-Stats 'spawn+pipe wall' $wallMs
Show-Stats 'daemon hook->BLE' $lat
Show-Stats 'gatt write only' $gatt
if ($lat.Count -eq 0) {
    Write-Host 'WARNING: no Latency samples - is the ESP32 connected? check the log for "fully connected".' -ForegroundColor Yellow
}
