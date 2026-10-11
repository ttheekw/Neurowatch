# Build the Neurowatch Windows x86_64 installer and portable release zip.
#
# Requires the GStreamer MSVC runtime + development MSIs; the runtime is copied
# into the bundle so end users do not need a separate GStreamer install.
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$OutDir = '',
    [string]$GStreamerRoot = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64,
    [string]$ISCC = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Native {
    param([string]$FilePath, [string[]]$Arguments)
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath exited with code $LASTEXITCODE"
    }
}

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not (Test-Path (Join-Path $Root 'pyproject.toml'))) {
    throw "pyproject.toml not found at repository root: $Root"
}
if (-not $OutDir) { $OutDir = Join-Path $Root 'dist/release' }
$Version = $Version -replace '^v', ''
if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?$') {
    throw "invalid version: $Version"
}
if (-not (Test-Path $ISCC)) {
    throw 'Inno Setup 6 is required; install it or pass -ISCC path/to/ISCC.exe'
}
if (-not $GStreamerRoot -or -not (Test-Path (Join-Path $GStreamerRoot 'bin'))) {
    throw 'GStreamer MSVC runtime not found; set GSTREAMER_1_0_ROOT_MSVC_X86_64 or pass -GStreamerRoot'
}

$WorkDir = Join-Path $Root 'build/release-windows-x86_64'
$BundleDir = Join-Path $WorkDir 'bundle/neurowatch'
$Venv = Join-Path $WorkDir 'venv'
if (Test-Path $WorkDir) { Remove-Item -Recurse -Force $WorkDir }
New-Item -ItemType Directory -Force -Path (Join-Path $BundleDir 'bin') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $BundleDir 'app') | Out-Null
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path

Write-Host '[1/6] Building backend (release)'
# NW_BACKEND_MANIFEST / NW_BACKEND_PACKAGE: build the `nw-backend` binary from
# another Cargo workspace (one that adds a backend extension to this one).
$BackendManifest = if ($env:NW_BACKEND_MANIFEST) { $env:NW_BACKEND_MANIFEST } else { Join-Path $Root 'Cargo.toml' }
$BackendPackage = if ($env:NW_BACKEND_PACKAGE) { $env:NW_BACKEND_PACKAGE } else { 'nw-backend' }
$BackendTarget = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path (Split-Path -Parent $BackendManifest) 'target' }
Invoke-Native 'cargo' @('build', '--manifest-path', $BackendManifest, '--release', '--locked', '-p', $BackendPackage)
Copy-Item (Join-Path $BackendTarget 'release/nw-backend.exe') (Join-Path $BundleDir 'bin/nw-backend.exe') -Force
# The licence and notice ship with every artifact, and the terms of any
# extensions built in (NW_EXTRA_LICENSE_FILE).
Copy-Item (Join-Path $Root 'LICENSE') (Join-Path $BundleDir 'LICENSE') -Force
Copy-Item (Join-Path $Root 'NOTICE') (Join-Path $BundleDir 'NOTICE') -Force
if ($env:NW_EXTRA_LICENSE_FILE) {
    Copy-Item $env:NW_EXTRA_LICENSE_FILE (Join-Path $BundleDir 'LICENSE-EXTENSIONS') -Force
}

Write-Host '[2/6] Packaging UI from pyproject.toml with PyInstaller'
$Python = if ($env:PYTHON) { $env:PYTHON } else { 'python' }
Invoke-Native $Python @('-m', 'venv', $Venv)
$VenvPython = Join-Path $Venv 'Scripts/python.exe'
Invoke-Native $VenvPython @('-m', 'pip', 'install', '--upgrade', 'pip')
Invoke-Native $VenvPython @('-m', 'pip', 'install', 'pyinstaller==6.16.0', 'uv==0.11.6')
$Requirements = Join-Path $WorkDir 'requirements.txt'
Invoke-Native (Join-Path $Venv 'Scripts/uv.exe') @(
    'export', '--project', $Root, '--locked', '--no-dev', '--no-emit-project',
    '--format', 'requirements-txt', '--output-file', $Requirements
)
Invoke-Native $VenvPython @('-m', 'pip', 'install', '--require-hashes', '-r', $Requirements)
Invoke-Native $VenvPython @('-m', 'pip', 'install', '--no-deps', $Root)
# NW_UI_EXTENSION_DIR / _DIST / _MODULE: bundle a UI extension (see
# ui/neurowatch/extensions.py) with its entry-point metadata, so it is found.
$ExtensionArgs = @()
if ($env:NW_UI_EXTENSION_DIR) {
    if (-not $env:NW_UI_EXTENSION_DIST -or -not $env:NW_UI_EXTENSION_MODULE) {
        throw 'NW_UI_EXTENSION_DIR needs NW_UI_EXTENSION_DIST and NW_UI_EXTENSION_MODULE'
    }
    Invoke-Native $VenvPython @('-m', 'pip', 'install', '--no-deps', $env:NW_UI_EXTENSION_DIR)
    $ExtensionArgs = @('--copy-metadata', $env:NW_UI_EXTENSION_DIST, '--collect-submodules', $env:NW_UI_EXTENSION_MODULE)
}
Invoke-Native (Join-Path $Venv 'Scripts/pyinstaller.exe') (@(
    '--noconfirm', '--clean', '--windowed',
    '--name', 'neurowatch-ui',
    '--distpath', (Join-Path $WorkDir 'pyinstaller-dist'),
    '--workpath', (Join-Path $WorkDir 'pyinstaller-build'),
    '--specpath', $WorkDir,
    '--collect-data', 'neurowatch',
    '--copy-metadata', 'neurowatch'
) + $ExtensionArgs + @(
    (Join-Path $Root 'scripts/entrypoints/neurowatch_ui.py')
))
Copy-Item -Recurse -Force (Join-Path $WorkDir 'pyinstaller-dist/neurowatch-ui') (Join-Path $BundleDir 'app/neurowatch-ui')

Write-Host '[3/6] Bundling GStreamer runtime'
$GstDir = Join-Path $BundleDir 'gstreamer'
New-Item -ItemType Directory -Force -Path (Join-Path $GstDir 'bin') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $GstDir 'lib/gstreamer-1.0') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $GstDir 'libexec/gstreamer-1.0') | Out-Null
Copy-Item (Join-Path $GStreamerRoot 'bin/*.dll') (Join-Path $GstDir 'bin') -Force
foreach ($tool in @('gst-inspect-1.0.exe', 'gst-launch-1.0.exe')) {
    Copy-Item (Join-Path $GStreamerRoot "bin/$tool") (Join-Path $GstDir 'bin') -Force
}
Copy-Item (Join-Path $GStreamerRoot 'lib/gstreamer-1.0/*.dll') (Join-Path $GstDir 'lib/gstreamer-1.0') -Force
$Scanner = Join-Path $GStreamerRoot 'libexec/gstreamer-1.0/gst-plugin-scanner.exe'
Copy-Item $Scanner (Join-Path $GstDir 'libexec/gstreamer-1.0') -Force
# GIO's TLS modules are loaded dynamically, so copying bin/*.dll alone does
# not support encrypted camera connections on machines without the SDK.
Copy-Item -Recurse -Force (Join-Path $GStreamerRoot 'lib/gio') (Join-Path $GstDir 'lib/gio')
if (Test-Path (Join-Path $GStreamerRoot 'etc')) {
    Copy-Item -Recurse -Force (Join-Path $GStreamerRoot 'etc') (Join-Path $GstDir 'etc')
}
foreach ($license in @('share/licenses', 'share/gstreamer-1.0/licenses')) {
    $src = Join-Path $GStreamerRoot $license
    if (Test-Path $src) {
        Copy-Item -Recurse -Force $src (Join-Path $GstDir 'licenses')
        break
    }
}

# App-local MSVC runtime: no system-wide installer or administrator rights.
if (-not $env:VCToolsRedistDir) { throw 'Run from an MSVC developer shell (VCToolsRedistDir missing)' }
$Crt = Get-ChildItem (Join-Path $env:VCToolsRedistDir 'x64/Microsoft.VC*.CRT') -Directory | Select-Object -First 1
if (-not $Crt) { throw 'MSVC x64 redistributable runtime missing' }
Copy-Item (Join-Path $Crt.FullName '*.dll') (Join-Path $BundleDir 'bin') -Force

Write-Host '[4/6] Writing launcher'
$Launcher = @'
# Neurowatch launcher: opens the desktop UI. The backend (bin\nw-backend.exe)
# runs as a per-user background service that keeps recording after the UI
# closes: the UI starts it (with the bundled GStreamer runtime) if it is not
# running, and registers the tray icon to start at login, which starts the
# service then. `neurowatch --no-autostart` registers nothing;
# `neurowatch service status|start|stop|restart|install|uninstall` controls it.
$ErrorActionPreference = 'Stop'
$Base = $PSScriptRoot
if ($env:NW_IPC_PORT) { $env:NW_IPC_ENDPOINT = "tcp:127.0.0.1:$([int]$env:NW_IPC_PORT)" }

$uiStart = @{ FilePath = (Join-Path $Base 'app\neurowatch-ui\neurowatch-ui.exe'); PassThru = $true; Wait = $true }
if ($args.Count -gt 0) { $uiStart.ArgumentList = $args }
$ui = Start-Process @uiStart
exit $ui.ExitCode
'@
Set-Content -Path (Join-Path $BundleDir 'neurowatch.ps1') -Value $Launcher -Encoding UTF8

$Cmd = @'
@echo off
powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "%~dp0neurowatch.ps1" %*
exit /b %ERRORLEVEL%
'@
Set-Content -Path (Join-Path $BundleDir 'neurowatch.cmd') -Value $Cmd -Encoding ASCII

Write-Host '[5/6] Creating zip'
$ZipPath = Join-Path $OutDir "neurowatch-$Version-windows-x86_64.zip"
if (Test-Path $ZipPath) { Remove-Item -Force $ZipPath }
Compress-Archive -Path (Join-Path $WorkDir 'bundle/neurowatch') -DestinationPath $ZipPath
Write-Host "Zip ready: $ZipPath"

Write-Host '[6/6] Creating Windows installer'
Invoke-Native $ISCC @(
    "/DAppVersion=$Version",
    "/DBundleDir=$BundleDir",
    "/DOutputDir=$OutDir",
    (Join-Path $Root 'scripts/installer/windows.iss')
)
$SetupPath = Join-Path $OutDir "neurowatch-$Version-windows-x86_64-setup.exe"
if (-not (Test-Path $SetupPath)) { throw "Installer was not produced: $SetupPath" }
Write-Host "Installer ready: $SetupPath"
