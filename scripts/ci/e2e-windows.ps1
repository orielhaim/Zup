param(
    [Parameter(Mandatory = $true)][string]$Installer
)
$ErrorActionPreference = 'Stop'

& $Installer install --scope user --non-interactive --yes
if ($LASTEXITCODE -ne 0) { throw "install failed with exit code $LASTEXITCODE" }

$payload = Join-Path $env:LOCALAPPDATA 'Cross Matrix\app.txt'
$content = (Get-Content $payload -Raw).Trim()
if ($content -ne 'hello cross matrix') { throw "payload mismatch: '$content'" }

& $Installer repair --scope user --non-interactive --yes
if ($LASTEXITCODE -ne 0) { throw "repair failed with exit code $LASTEXITCODE" }

& $Installer uninstall --scope user --non-interactive --yes
if ($LASTEXITCODE -ne 0) { throw "uninstall failed with exit code $LASTEXITCODE" }

if (Test-Path (Join-Path $env:LOCALAPPDATA 'Cross Matrix')) {
    throw 'uninstall left files behind'
}
Write-Host 'lifecycle complete: install, repair, uninstall'
