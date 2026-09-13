# CursorLight V.rs0.4.1 Setup
#
# One-click installer. The user needs NO developer environment:
# no Rust, no cargo, no espflash, no network, no admin rights.
# Everything it needs is inside this package (bin\ and Firmware\).
#
# NOTE: this file is intentionally ASCII-only.
# Windows PowerShell 5.1 (used by setup.bat) decodes scripts as ANSI unless a
# BOM is present, so any non-ASCII byte here would corrupt the parser.
$ErrorActionPreference = "Stop"

$PreBuilt     = '.\bin\cursorlight.exe'
$PipeClient   = '.\bin\pipe-client.exe'
$DotsScript   = '.\traffic_light_desktop.py'
$Settings     = Join-Path $env:USERPROFILE '.claude\settings.json'
$ClaudeDir    = Join-Path $env:USERPROFILE '.claude'
$InstallDir   = Join-Path $env:USERPROFILE '.cursorlight'
$InstallBin   = Join-Path $InstallDir 'bin'
$InstalledExe = Join-Path $InstallBin 'cursorlight.exe'
$StateDir     = Join-Path $env:LOCALAPPDATA 'CursorLight'

$LF = [string][char]10
$Q  = [string][char]34

# Run a native command without letting its stderr abort the script.
# In Windows PowerShell 5.1 a native command writing to stderr raises
# NativeCommandError, which becomes terminating under ErrorActionPreference=Stop.
function Invoke-Native([scriptblock]$Block) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    # catch matters: when PowerShell cannot even *launch* a program it raises a
    # terminating error (antivirus block, missing file, policy), which would
    # otherwise abort the whole setup. Setup must survive that and continue.
    try {
        & $Block
    } catch {
    } finally {
        $ErrorActionPreference = $prev
    }
}

function Show-Ok([string]$text)   { Write-Host ("        " + $text) }
function Show-Warn([string]$text) { Write-Host ("        [WARN] " + $text) -ForegroundColor Yellow }

# Find a usable Python 3 *with tkinter* for the optional taskbar dots.
# Returns a command line fragment ("py -3" / "python") or $null.
# Some machines have python.exe but no "py" launcher, so all are probed.
function Resolve-DotsPython {
    foreach ($cand in @(@('py', '-3'), @('python'), @('python3'))) {
        $exe = $cand[0]
        if (-not (Get-Command $exe -ErrorAction SilentlyContinue)) { continue }
        $all = @()
        if ($cand.Count -gt 1) { $all += $cand[1..($cand.Count - 1)] }
        $all += @('-c', 'import sys, tkinter; print(sys.version_info[0])')
        $out = Invoke-Native { & $exe @all 2>$null }
        if ($LASTEXITCODE -eq 0 -and (($out -join '') -match '3')) {
            $cmdline = $exe
            if ($cand.Count -gt 1) { $cmdline += ' ' + ($cand[1..($cand.Count - 1)] -join ' ') }
            return $cmdline
        }
    }
    return $null
}

Write-Host ""
Write-Host "  ============================================="
Write-Host "    CursorLight V.rs0.4.1 Setup"
Write-Host "  ============================================="
Write-Host ""

# ---------------------------------------------------------------------------
# Step 1: environment self-check (nothing to install, all optional except Windows)
# ---------------------------------------------------------------------------
Write-Host "  [1/6] Checking environment..." -ForegroundColor Cyan
Show-Ok ("PowerShell      : " + $PSVersionTable.PSVersion.ToString())

$envOk = $true
try {
    New-Item -Path $InstallDir -ItemType Directory -Force | Out-Null
    New-Item -Path $StateDir -ItemType Directory -Force | Out-Null
    Show-Ok ("Install dir     : " + $InstallDir)
} catch {
    Show-Warn ("cannot write " + $InstallDir + " : " + $_.Exception.Message)
    $envOk = $false
}

$bt = Get-Service -Name 'bthserv' -ErrorAction SilentlyContinue
if ($bt) {
    Show-Ok "Bluetooth       : available"
} else {
    Show-Warn "no Bluetooth support service found - the light cannot connect until an adapter is available"
}

if (Test-Path $ClaudeDir) {
    Show-Ok ("Claude Code     : " + $ClaudeDir)
} else {
    Show-Warn "Claude Code config dir not found; hooks are written anyway and take effect once Claude Code runs"
}

$PyCmd = Resolve-DotsPython
if ($PyCmd) {
    Show-Ok ("Desktop dots    : " + $PyCmd)
} else {
    Show-Warn "Python 3 (with tkinter) not found - taskbar dots disabled, the physical light still works"
}

if (-not $envOk) {
    Write-Host ""
    Write-Host "  [ERROR] Cannot write to your user profile. Run setup again as the logged-in user." -ForegroundColor Red
    exit 1
}
Write-Host ""

# ---------------------------------------------------------------------------
# Step 2: package integrity - everything must ship prebuilt
# ---------------------------------------------------------------------------
Write-Host "  [2/6] Checking package..." -ForegroundColor Cyan
if ((Test-Path $PreBuilt) -and (Test-Path $PipeClient)) {
    Show-Ok ("daemon      : " + $PreBuilt)
    Show-Ok ("pipe client : " + $PipeClient)
} else {
    Write-Host ""
    Write-Host "  [ERROR] This package is incomplete: bin\cursorlight.exe or bin\pipe-client.exe" -ForegroundColor Red
    Write-Host "         is missing. Please use the full release package (the zip that contains" -ForegroundColor Red
    Write-Host "         bin\ and Firmware\). A source-only checkout has no prebuilt binaries." -ForegroundColor Red
    exit 1
}
if (Test-Path $DotsScript) {
    Show-Ok "assets      : traffic_light_desktop.py"
} else {
    Show-Warn "traffic_light_desktop.py missing - taskbar dots will be skipped"
}
Write-Host ""

# ---------------------------------------------------------------------------
# Step 3: stop any running daemon BEFORE overwriting the exe
# (copying over a running exe fails and used to abort the whole install)
# ---------------------------------------------------------------------------
Write-Host "  [3/6] Stopping old daemon..." -ForegroundColor Cyan
$running = Get-Process -Name 'cursorlight' -ErrorAction SilentlyContinue
if ($running) {
    $stopper = $null
    if (Test-Path $PipeClient) {
        $stopper = (Resolve-Path $PipeClient).Path
    } elseif (Test-Path (Join-Path $InstallBin 'pipe-client.exe')) {
        $stopper = Join-Path $InstallBin 'pipe-client.exe'
    }
    if ($stopper) {
        Invoke-Native { & $stopper '{"action":"stop","status":"stop"}' 2>$null | Out-Null } | Out-Null
    }
    Start-Sleep -Milliseconds 800
    Stop-Process -Name 'cursorlight' -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 400
    Show-Ok ("stopped " + $running.Count + " process(es)")
} else {
    Show-Ok "no daemon running"
}
Write-Host ""

# ---------------------------------------------------------------------------
# Step 4: install files
# ---------------------------------------------------------------------------
Write-Host "  [4/6] Installing files..." -ForegroundColor Cyan
New-Item -Path $InstallBin -ItemType Directory -Force | Out-Null
Copy-Item $PreBuilt $InstalledExe -Force
Copy-Item $PipeClient (Join-Path $InstallBin 'pipe-client.exe') -Force
if (Test-Path $DotsScript) {
    Copy-Item $DotsScript (Join-Path $InstallDir 'traffic_light_desktop.py') -Force
}

# Prove the installed binary actually runs (antivirus / blocked exe shows up here)
$installedVersion = 'unknown'
try {
    $installedVersion = (Invoke-Native { & $InstalledExe --version 2>$null } | Select-Object -First 1)
} catch {
    $installedVersion = 'unknown'
}
Show-Ok ("installed to    : " + $InstallDir)
Show-Ok ("daemon version  : " + $installedVersion)
if ($installedVersion -eq 'unknown') {
    Show-Warn "the daemon did not report a version - antivirus may have blocked it; check $InstallBin"
}
Write-Host ""

# ---------------------------------------------------------------------------
# Step 5: generate hook.bat + write the Claude Code hooks config
# ---------------------------------------------------------------------------
Write-Host "  [5/6] Configuring hooks..." -ForegroundColor Cyan

# hook.bat notes (latency tuned, no probing process):
# - No chcp line: ASCII-only script, and chcp costs a process spawn per event.
# - No liveness probe: pipe-client starts the daemon itself when the pipe is
#   missing and then delivers the message (one process per hook event).
# - One message per invocation: stdin can only be read once.
# - Stop events carry --status completed, otherwise the daemon only sees
#   "unknown" and the light can never show success/error correctly.
# - Desktop dots lines are generated only when a usable Python was found.
if ($PyCmd) {
    $dotsStart = '    start "" /b ' + $PyCmd + ' "%~dp0traffic_light_desktop.py" start'
    $dotsStop  = '    ' + $PyCmd + ' "%~dp0traffic_light_desktop.py" stop 2>nul'
} else {
    $dotsStart = '    REM desktop dots disabled: no usable Python 3'
    $dotsStop  = '    REM desktop dots disabled: no usable Python 3'
}

$lines = @(
    '@echo off'
    'setlocal enabledelayedexpansion'
    'set ACT=%1'
    'if "!ACT!"=="" set ACT=thinking'
    'set PIPE_CLIENT=%~dp0bin\pipe-client.exe'
    ''
    'REM === Session lifecycle ==='
    'if "!ACT!"=="session_start" ('
    '    "%PIPE_CLIENT%" --stdin --action session_start 2>nul'
    $dotsStart
    '    exit /b 0'
    ')'
    'if "!ACT!"=="idle" ('
    $dotsStop
    '    "%PIPE_CLIENT%" --stdin --action session_end 2>nul'
    '    exit /b 0'
    ')'
    ''
    'REM === State commands ==='
    'if "!ACT!"=="thinking" ('
    '    "%PIPE_CLIENT%" "{\"action\":\"thinking\"}" 2>nul'
    '    exit /b 0'
    ')'
    'if "!ACT!"=="build" ('
    '    "%PIPE_CLIENT%" "{\"action\":\"build\"}" 2>nul'
    '    exit /b 0'
    ')'
    'if "!ACT!"=="alarm" ('
    '    "%PIPE_CLIENT%" "{\"action\":\"alarm\"}" 2>nul'
    '    exit /b 0'
    ')'
    'if "!ACT!"=="busy" ('
    '    "%PIPE_CLIENT%" "{\"action\":\"busy\"}" 2>nul'
    '    exit /b 0'
    ')'
    'if "!ACT!"=="error" ('
    '    "%PIPE_CLIENT%" "{\"action\":\"error\"}" 2>nul'
    '    exit /b 0'
    ')'
    'REM Commands that need stdin: pipe-client reads hook JSON, injects action/status'
    'if "!ACT!"=="pre_tool" ('
    '    "%PIPE_CLIENT%" --stdin --action pre_tool 2>nul'
    '    exit /b 0'
    ')'
    'if "!ACT!"=="post_tool" ('
    '    "%PIPE_CLIENT%" --stdin --action post_tool 2>nul'
    '    exit /b 0'
    ')'
    'if "!ACT!"=="stop" ('
    '    "%PIPE_CLIENT%" --stdin --action stop --status completed 2>nul'
    '    exit /b 0'
    ')'
    'REM Unknown action: return without writing garbage into the pipe'
    'exit /b 0'
)
$lines | Out-File -FilePath (Join-Path $InstallDir 'hook.bat') -Encoding ASCII -Force
Show-Ok "hook.bat generated"

# Claude Code expects this shape:
#   "EventName": [ { "hooks": [ { "type":"command", "command":"...", "shell":"powershell" } ] } ]
# PowerShell 5.x ConvertTo-Json turns nested arrays into {"value":[...],"Count":1},
# so the JSON is built by hand here.

function Escape-JsonString([string]$s) {
    $s = $s.Replace('\', '\\')
    $s = $s.Replace($Q, $Q + '\' + $Q)
    $s = $s.Replace([string][char]10, '\n')
    $s = $s.Replace([string][char]13, '\r')
    $s = $s.Replace([string][char]9, '\t')
    return $s
}

function New-HookEntryJson([string]$cmd) {
    $escaped = Escape-JsonString $cmd
    $entry = '[' + '{' + $Q + 'hooks' + $Q + ':' + '[' + '{'
    $entry += $Q + 'type' + $Q + ':' + $Q + 'command' + $Q + ','
    $entry += $Q + 'command' + $Q + ':' + $Q + $escaped + $Q + ','
    $entry += $Q + 'shell' + $Q + ':' + $Q + 'powershell' + $Q
    $entry += '}' + ']' + '}' + ']'
    return $entry
}

$HookCmd = "& '$InstallDir\hook.bat'"

$hookEvents = [ordered]@{
    'SessionStart'       = New-HookEntryJson "$HookCmd session_start"
    'UserPromptSubmit'   = New-HookEntryJson "$HookCmd thinking"
    'PreToolUse'         = New-HookEntryJson "$HookCmd pre_tool"
    'PostToolUse'        = New-HookEntryJson "$HookCmd post_tool"
    'PostToolUseFailure' = New-HookEntryJson "$HookCmd error"
    'Stop'               = New-HookEntryJson "$HookCmd stop"
    'SessionEnd'         = New-HookEntryJson "$HookCmd idle"
}

$hooksJsonParts = @()
foreach ($k in $hookEvents.Keys) {
    $hooksJsonParts += '    ' + $Q + $k + $Q + ': ' + $hookEvents[$k]
}
$hooksJson = '{' + $LF + ($hooksJsonParts -join (',' + $LF)) + $LF + '  }'

# Merge the existing settings (env / model / ...) with the new hooks.
function Merge-HooksIntoSettings {
    param([string]$settingsPath, [string]$hooksJson)

    if (Test-Path $settingsPath) {
        $existing = Get-Content $settingsPath -Raw -Encoding UTF8 | ConvertFrom-Json

        $parts = @('{')
        foreach ($prop in $existing.PSObject.Properties) {
            if ($prop.Name -eq 'hooks') { continue }

            $key = $prop.Name
            $val = $prop.Value
            $valJson = ($val | ConvertTo-Json -Depth 10 -Compress)

            if ($parts.Count -gt 1) { $parts[$parts.Count - 1] += ',' }
            $parts += '  ' + $Q + $key + $Q + ': ' + $valJson
        }

        if ($parts.Count -gt 1) { $parts[$parts.Count - 1] += ',' }
        $parts += '  ' + $Q + 'hooks' + $Q + ': ' + $hooksJson
        $parts += '}'
        return $parts -join $LF
    } else {
        return '{' + $LF + '  ' + $Q + 'hooks' + $Q + ': ' + $hooksJson + $LF + '}'
    }
}

$finalJson = Merge-HooksIntoSettings -settingsPath $Settings -hooksJson $hooksJson

New-Item -Path $ClaudeDir -ItemType Directory -Force | Out-Null
if (Test-Path $Settings) {
    Copy-Item $Settings "$Settings.bak" -Force
    Show-Ok ("existing settings backed up to " + (Split-Path $Settings -Leaf) + ".bak")
}
[System.IO.File]::WriteAllText($Settings, $finalJson, [System.Text.UTF8Encoding]::new($false))
Show-Ok "Claude Code hooks written"
Write-Host ""

# ---------------------------------------------------------------------------
# Step 6: start daemon + optional taskbar dots
# ---------------------------------------------------------------------------
Write-Host "  [6/6] Starting daemon..." -ForegroundColor Cyan
try {
    Start-Process -FilePath $InstalledExe -WindowStyle Hidden
} catch {
    Show-Warn ("could not launch the daemon: " + $_.Exception.Message)
}
Start-Sleep -Milliseconds 1200
$proc = Get-Process -Name 'cursorlight' -ErrorAction SilentlyContinue
if ($proc) {
    Show-Ok ("running (PID " + $proc[0].Id + ")")
} else {
    Show-Warn ("daemon did not stay up; see " + (Join-Path $InstallBin 'cursorlight_daemon.log'))
}
Write-Host ""

if ($PyCmd -and (Test-Path $DotsScript)) {
    Write-Host "        Starting desktop dots..." -ForegroundColor Cyan
    $dotsPath = Join-Path $InstallDir 'traffic_light_desktop.py'
    $dotsProc = Get-Process -Name "python*" -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowTitle -eq 'CL' }
    if ($dotsProc) {
        Show-Ok "already running, skipped"
    } else {
        $parts = $PyCmd.Split(' ')
        $exe = $parts[0]
        $argList = @()
        if ($parts.Count -gt 1) { $argList += $parts[1..($parts.Count - 1)] }
        $argList += @($dotsPath, 'start')
        try {
            Start-Process -FilePath $exe -ArgumentList $argList -WindowStyle Hidden
            Show-Ok "taskbar indicator started"
        } catch {
            Show-Warn "could not start desktop dots"
        }
    }
    Write-Host ""
}

Write-Host "  =============================================" -ForegroundColor Green
Write-Host "    Done!" -ForegroundColor Green
Write-Host "  =============================================" -ForegroundColor Green
Write-Host ""
Write-Host "    Daemon         : running in background ($installedVersion)"
if ($PyCmd) {
    Write-Host "    Desktop dots   : taskbar indicator active"
} else {
    Write-Host "    Desktop dots   : disabled (no Python 3)"
}
Write-Host "    Hooks          : $Settings"
Write-Host "    Firmware       : flash Firmware\*.bin (USB: Rust firmware flasher, or web-OTA\index.html)"
Write-Host ""
Write-Host "    Restart Claude Code once so the hooks take effect."
Write-Host "    Stop daemon    : taskkill /im cursorlight.exe"
Write-Host "    Stop dots      : close the CL window (right-click > Exit)"
Write-Host ""
