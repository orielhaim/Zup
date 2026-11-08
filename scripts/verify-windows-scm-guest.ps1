param(
    [string]$Controller = 'http://10.0.2.2:18491',
    [switch]$Once,
    [string]$LocalPayload,
    [string]$ResultPath
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'The disposable SCM guest must run with administrator privileges.'
}
$directory = 'C:\ZupScmVerification'
New-Item -ItemType Directory -Path $directory -Force | Out-Null

function Invoke-Verification([string]$PayloadZip, [string]$ReportPath, [string]$JobId, [bool]$InstallerMatrix) {
    Expand-Archive -LiteralPath $PayloadZip -DestinationPath $directory -Force
    $env:ZUP_TEST_SCM_SERVICE_EXE = "$directory\zup-test-service.exe"
    $results = @()
    foreach ($filter in @('service_control::tests::native::', 'services::tests::uniquely_named_service_create_upgrade_reconcile_and_drift', 'service_control::tests::scm_generation_requires_machine_authority')) {
        $ErrorActionPreference = 'Continue'
        $output = & "$directory\scm-tests.exe" --include-ignored --nocapture --test-threads=1 $filter 2>&1 | Out-String
        $exitCode = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        $results += @{ filter = $filter; exit_code = $exitCode; output = $output }
    }
    if ($InstallerMatrix) {
        $ErrorActionPreference = 'Continue'
        $output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$directory\verify-windows-service-installer.ps1" -Zup "$directory\zup.exe" -Runtime "$directory\zup-setup-headless-windows-x64.exe" -Service "$directory\zup-test-service.exe" 2>&1 | Out-String
        $exitCode = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        $results += @{ filter = 'generated_installer_lifecycle'; exit_code = $exitCode; output = $output }
        $ErrorActionPreference = 'Continue'
        $output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$directory\verify-windows-service-installer.ps1" -Zup "$directory\zup.exe" -Runtime "$directory\zup-setup-headless-windows-x64.exe" -Service "$directory\zup-test-service.exe" -RecoveryPolicy 2>&1 | Out-String
        $exitCode = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        $results += @{ filter = 'generated_installer_recovery_policy'; exit_code = $exitCode; output = $output }
    }
    $remaining = @(Get-Service | Where-Object Name -Like 'ZupNativeTest-*' | Select-Object -ExpandProperty Name)
    $report = @{ job = $JobId; os = [Environment]::OSVersion.VersionString; administrator = $true; results = $results; remaining_services = $remaining }
    $json = $report | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText($ReportPath, $json, [Text.UTF8Encoding]::new($false))
    return @{ report = $report; json = $json }
}

if ($LocalPayload) {
    if (-not $ResultPath) { throw 'Local verification requires an explicit result path.' }
    $verification = Invoke-Verification $LocalPayload $ResultPath 'local-native-verification' $true
    if (@($verification.report.results | Where-Object exit_code -NE 0).Count -or $verification.report.remaining_services.Count) {
        throw "Native SCM verification failed; see $ResultPath."
    }
    exit 0
}

$lastJob = $null
while ($true) {
    try {
        $job = Invoke-RestMethod "$Controller/job.json"
        if ($job.id -and $job.id -ne $lastJob) {
            if ($job.action -eq 'shutdown') { Stop-Computer -Force; break }
            if ($job.action -ne 'verify') { throw 'Unsupported SCM verification job.' }
            Invoke-WebRequest "$Controller/payload.zip" -OutFile "$directory\payload.zip"
            $verification = Invoke-Verification "$directory\payload.zip" "$directory\result.json" $job.id ([bool]$job.installer_matrix)
            Invoke-RestMethod "$Controller/result" -Method Post -ContentType 'application/json; charset=utf-8' -Body ([Text.Encoding]::UTF8.GetBytes($verification.json)) | Out-Null
            $lastJob = $job.id
            if (@($verification.report.results | Where-Object exit_code -NE 0).Count -or $verification.report.remaining_services.Count) {
                throw 'Native SCM verification failed; see result.json for test and cleanup diagnostics.'
            }
            if ($Once) { break }
        }
    } catch {
        Add-Content -LiteralPath "$directory\controller.log" -Value $_.ToString() -Encoding UTF8
        if ($Once) { throw }
    }
    Start-Sleep -Seconds 2
}
