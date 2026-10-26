param([Parameter(Mandatory = $true)][string]$OutDir)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Setup = @(Get-ChildItem "$OutDir/*-setup.exe")
if ($Setup.Count -ne 1) { throw 'Expected exactly one Windows installer' }
$Destination = Join-Path $env:RUNNER_TEMP 'NeuroWatch install test'
$InstallLog = Join-Path $env:RUNNER_TEMP 'neurowatch-install.log'
$Process = Start-Process $Setup[0].FullName -Wait -PassThru -ArgumentList @(
    '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-',
    "/DIR=`"$Destination`"", "/LOG=`"$InstallLog`""
)
if ($Process.ExitCode -ne 0) { Get-Content $InstallLog; throw "Installer failed: $($Process.ExitCode)" }
if (-not (Test-Path (Join-Path $Destination 'THIRD_PARTY_NOTICES.md'))) { throw 'Installer omitted third-party notices' }

# An update says so and removes what the old version left behind (here the
# same installer again over the installation it made).
$StaleMeta = Join-Path $Destination 'app\neurowatch-ui\_internal\neurowatch-0.0.1.dist-info'
$StalePlugin = Join-Path $Destination 'gstreamer\lib\gstreamer-1.0\gstneurowatchold.dll'
New-Item -ItemType Directory -Force -Path $StaleMeta | Out-Null
Set-Content -Path (Join-Path $StaleMeta 'METADATA') -Value "Name: neurowatch`nVersion: 0.0.1" -Encoding ascii
Set-Content -Path $StalePlugin -Value '' -Encoding ascii
$UpdateLog = Join-Path $env:RUNNER_TEMP 'neurowatch-update.log'
$Process = Start-Process $Setup[0].FullName -Wait -PassThru -ArgumentList @(
    '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-',
    "/DIR=`"$Destination`"", "/LOG=`"$UpdateLog`""
)
if ($Process.ExitCode -ne 0) { Get-Content $UpdateLog; throw "Update failed: $($Process.ExitCode)" }
if ((Test-Path $StaleMeta) -or (Test-Path $StalePlugin)) { Get-Content $UpdateLog; throw 'The update left old files behind' }
$Updated = Get-Content $UpdateLog -Raw
if ($Updated -match 'Not only NeuroWatch in ') { throw 'The installer mistook its own bundle files for unrelated content' }
if ($Updated -notmatch 'Updating NeuroWatch .+ to ') { throw 'The update did not notice the installed version' }
if ($Updated -notmatch 'Removed [1-9][0-9]* file\(s\) left over') { throw 'The update did not report the removed files' }
if (-not (Test-Path (Join-Path $Destination 'bin/nw-backend.exe'))) { throw 'The update removed the backend' }
if (-not (Test-Path (Join-Path $Destination 'THIRD_PARTY_NOTICES.md'))) { throw 'The update removed third-party notices' }
# The build SDK must not satisfy missing runtime DLLs or plugins during testing.
$env:PATH = ($env:PATH -split ';' | Where-Object { $_ -notmatch 'gstreamer' }) -join ';'
$Sdk = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64
$HiddenSdk = if ($Sdk) { "$Sdk.hidden-for-neurowatch-test" } else { $null }
$SdkMoved = $false
Remove-Item Env:GSTREAMER_1_0_ROOT_MSVC_X86_64 -ErrorAction SilentlyContinue
Remove-Item Env:GST_PLUGIN_PATH -ErrorAction SilentlyContinue
try {
    # Only on disposable Actions runners: also hide hard-coded SDK paths used
    # by dynamically loaded modules. PATH removal alone cannot catch those.
    if ($env:GITHUB_ACTIONS -eq 'true' -and $Sdk -and (Test-Path $Sdk)) {
        Move-Item $Sdk $HiddenSdk
        $SdkMoved = $true
    }
    & python (Join-Path $PSScriptRoot 'smoke_release.py') $Destination
    if ($LASTEXITCODE -ne 0) { throw 'Installed bundle smoke check failed' }
} finally {
    if ($SdkMoved) { Move-Item $HiddenSdk $Sdk }
    $Uninstall = Start-Process (Join-Path $Destination 'unins000.exe') -Wait -PassThru -ArgumentList @(
        '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
    )
    if ($Uninstall.ExitCode -ne 0) { throw "Uninstall failed: $($Uninstall.ExitCode)" }
}
if (Test-Path (Join-Path $Destination 'bin/nw-backend.exe')) { throw 'Uninstaller left the backend installed' }
