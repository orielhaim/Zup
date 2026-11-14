$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Native SCM verification requires an elevated PowerShell session.'
}
Push-Location (Join-Path $PSScriptRoot '..')
$previousFixture = $env:ZUP_TEST_SCM_SERVICE_EXE
try {
    cargo build -p zup-windows --features test-launcher --bin zup-test-service
    if ($LASTEXITCODE -ne 0) { throw 'SCM fixture build failed.' }
    $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
    $env:ZUP_TEST_SCM_SERVICE_EXE = Join-Path $metadata.target_directory 'debug/zup-test-service.exe'
    cargo nextest run --profile scm -p zup-windows --features test-launcher --lib --run-ignored all -E 'test(service_control::tests::native::) or test(services::tests::uniquely_named_service_create_upgrade_reconcile_and_drift) or test(service_control::tests::scm_generation_requires_machine_authority)' --no-fail-fast
    if ($LASTEXITCODE -ne 0) { throw 'Native SCM verification failed.' }
} finally {
    $env:ZUP_TEST_SCM_SERVICE_EXE = $previousFixture
    Pop-Location
}
