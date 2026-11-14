# Removes Session Hub for the current user, including its data in %LOCALAPPDATA%\SessionHub.
# -KeepData keeps the data. -Purge is the old spelling of the default and still accepted.
param([switch]$KeepData, [switch]$Purge)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'dist\hub.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    $exe = Join-Path $env:LOCALAPPDATA 'SessionHub\bin\hub.exe'
}
if (-not (Test-Path -LiteralPath $exe)) { throw 'hub.exe not found: nothing to uninstall' }
$hubArgs = @('uninstall')
if ($KeepData) { $hubArgs += '--keep-data' }
& $exe @hubArgs
exit $LASTEXITCODE
