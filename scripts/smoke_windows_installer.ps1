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
