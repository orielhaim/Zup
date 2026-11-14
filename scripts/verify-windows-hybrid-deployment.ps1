param(
    [Parameter(Mandatory)][ValidateSet('prepare', 'recover', 'verify')][string]$Phase,
    [Parameter(Mandatory)][string]$Tools,
    [Parameter(Mandatory)][string]$Context
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$utf8 = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $utf8
$OutputEncoding = [Console]::OutputEncoding
if (-not ([Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run inside an isolated elevated Windows guest.'
}
function Invoke-Native([string]$Executable, [string[]]$Arguments, [switch]$AllowFailure) {
    $stdout = Join-Path $fixture 'stdout.txt'
    $stderr = Join-Path $fixture 'stderr.txt'
    $quoted = $Arguments | ForEach-Object {
        if ($_ -match '["\r\n]' -or $_.EndsWith('\')) { throw "Unsupported fixture argument: <$_>." }
        '"' + $_ + '"'
    }
    $process = Start-Process -FilePath $Executable -ArgumentList $quoted -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $null = $process.Handle
    if (-not $process.WaitForExit(240000)) { $process.Kill(); throw 'Native fixture command timed out.' }
    $process.WaitForExit()
    $output = [IO.File]::ReadAllText($stdout)
    if ($process.ExitCode -ne 0 -and -not $AllowFailure) { throw "Command failed ($($process.ExitCode)): $output $([IO.File]::ReadAllText($stderr))" }
    @{ code = $process.ExitCode; output = $output }
}
function Deploy([string]$Setup, [string]$Action, [string[]]$Extra = @(), [switch]$AllowFailure) {
    $arguments = @($Action) + $Extra + @('--scope', 'machine', '--output', 'json')
    if ($Action -ne 'recover') { $arguments += @('--non-interactive', '--yes') }
    Invoke-Native $Setup $arguments -AllowFailure:$AllowFailure
}
function Assert-Native([string]$ExpectedImage) {
    $serviceState = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
    if ($serviceState.State -ne 'Running' -or $serviceState.ProcessId -eq 0 -or $serviceState.StartMode -ne 'Auto') { throw 'Native automatic service is not running.' }
    $image = [IO.File]::ReadAllText((Join-Path $data 'native-image.txt'))
    if ($image -ne $ExpectedImage) { throw "Service executed $image instead of $ExpectedImage" }
    if (-not $serviceState.PathName.Contains($ExpectedImage)) { throw 'SCM registration is not pinned to the selected generation.' }
    if ([IO.File]::ReadAllText((Join-Path $data 'sentinel')) -ne 'persistent') { throw 'Persistent application data changed.' }
}
if ($Phase -eq 'prepare') {
    $suffix = [Guid]::NewGuid().ToString('N')
    $app = "com.example.zup-hybrid-$suffix"
    $serviceDisplay = "zup-hybrid-$suffix"
    $fixture = Join-Path $env:ProgramFiles "ZupHybridTest-$suffix"
    $dist = Join-Path $fixture 'dist'
    $install = Join-Path $env:ProgramFiles $app
    $data = Join-Path $env:ProgramData "$app-data"
    New-Item -ItemType Directory -Path $dist | Out-Null
    $setups = @()
    foreach ($version in 1..3) {
        $bytes = [IO.File]::ReadAllBytes((Join-Path $Tools 'zup-test-service.exe'))
        $marker = if ($version -eq 3) { 'zup-native-start-failure' } else { "generation-$version" }
        [IO.File]::WriteAllBytes((Join-Path $dist 'worker.exe'), [byte[]]($bytes + [Text.Encoding]::ASCII.GetBytes($marker)))
        [IO.File]::WriteAllText((Join-Path $dist 'fixed.conf'), "configuration-$version", $utf8)
        $manifest = Join-Path $fixture 'zup.toml'
        [IO.File]::WriteAllText($manifest, @"
schema = 1
frontend = "headless"
[app]
id = "$app"
name = "Hybrid native service verification"
version = "$version.0.0"
[build.targets.default]
target = "windows-x64"
source = { directory = "dist" }
[install]
scope = "machine"
generation_readers = "native_executables"
[install.directory]
machine = "`${location.programs}/$app"
[[install.data]]
id = "state"
path = "`${location.shared_data}/$app-data"
access = { kind = "services", services = ["worker"] }
[[files]]
id = "worker"
source = "worker.exe"
executable = true
[[files]]
id = "configuration"
source = "fixed.conf"
placement = { kind = "fixed", directory = "`${install}", replacement = "quiescent" }
[[services]]
id = "worker"
display_name = "$serviceDisplay"
binary = "`${payload}/worker.exe"
arguments = ["`${service.worker}", "data", "`${data.state}", "--job", "`${job.worker}"]
requires_files = ["`${install}/fixed.conf"]
start = "automatic"
execution = { type = "windows_job", account = "local_service" }
lifecycle = { install = "running", change = "preserve" }
"@, $utf8)
        $setup = Join-Path $fixture "setup-$version.exe"
        $null = Invoke-Native (Join-Path $Tools 'zup.exe') @('build', '--manifest', $manifest, '--runtime', (Join-Path $Tools 'zup-setup-headless-windows-x64.exe'), '--output', $setup)
        $setups += $setup
    }
    $null = Deploy $setups[0] 'install'
    $registrations = @(Get-CimInstance Win32_Service -Filter "DisplayName='$serviceDisplay'")
    if ($registrations.Count -ne 1) { throw 'Expected one native registration for the default service name.' }
    $serviceName = $registrations[0].Name
    [IO.File]::WriteAllText((Join-Path $data 'sentinel'), 'persistent', $utf8)
    [IO.File]::WriteAllText((Join-Path $install 'foreign.txt'), 'unrelated', $utf8)
    $firstImage = [IO.File]::ReadAllText((Join-Path $data 'native-image.txt'))
    if (-not $firstImage.Contains('\.zup\deployments\')) { throw 'Service does not use generation placement.' }
    Assert-Native $firstImage
    $null = Deploy $setups[1] 'upgrade'
    $secondImage = [IO.File]::ReadAllText((Join-Path $data 'native-image.txt'))
    if ($secondImage -eq $firstImage) { throw 'Upgrade did not activate a new generation.' }
    Assert-Native $secondImage
    $failed = Deploy $setups[2] 'upgrade' -AllowFailure
    if ($failed.code -eq 0) { throw 'Failed service startup was accepted.' }
    $diagnostic = $failed.output | ConvertFrom-Json
    $hash = [Security.Cryptography.SHA256]::Create()
    try { $key = -join($hash.ComputeHash([Text.Encoding]::UTF8.GetBytes("zup:application-state:1`0$app")) | ForEach-Object { $_.ToString('x2') }) } finally { $hash.Dispose() }
    $state = Join-Path $env:ProgramData "zup/apps/$key"
    $pending = @(Get-ChildItem -LiteralPath (Join-Path $state 'transactions') -Directory | ForEach-Object {
        $journal = Get-Content -LiteralPath (Join-Path $_.FullName 'transaction.json') -Raw | ConvertFrom-Json
        if ($journal.app_id -eq $app -and $journal.phase.recovery_required -ne $null) { $journal }
    })
    if ($pending.Count -ne 1) { throw 'Failed start must retain one explicit recovery transaction.' }
    $native = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
    if ($native.State -ne 'Stopped' -or $native.StartMode -ne 'Disabled') { throw 'Uncertain startup did not close native admission.' }
    @{ fixture=$fixture; application=$app; service=$serviceName; data=$data; install=$install; image=$firstImage; second_image=$secondImage; setup=$setups[1]; recovery_setup=$setups[2]; transaction=$pending[0].transaction_id; boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().Ticks; state=$state } | ConvertTo-Json | ForEach-Object { [IO.File]::WriteAllText($Context, $_, $utf8) }
    if ($diagnostic.transaction -ne $pending[0].transaction_id -or [string]::IsNullOrWhiteSpace($diagnostic.message)) { throw 'Recovery result omitted the protected transaction identity or diagnostic.' }
    $failure = if ($pending[0].recovery_failure) { $pending[0].recovery_failure } elseif ($pending[0].rollback_failure) { $pending[0].rollback_failure } else { $pending[0].failure }
    if ($failure -and -not $diagnostic.message.Contains($failure.message)) { throw 'Recovery result lost the native failure diagnostic.' }
    @{ result='recovery_requires_reboot'; checks=@('hybrid_install', 'generation_upgrade', 'uncertain_start_closes_admission', 'persistent_service_access') } | ConvertTo-Json
} elseif ($Phase -eq 'recover') {
    $record = Get-Content -LiteralPath $Context -Raw | ConvertFrom-Json
    $fixture=$record.fixture; $serviceName=$record.service; $data=$record.data; $install=$record.install
    if ((Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().Ticks -le $record.boot) { throw 'A real guest reboot is required.' }
    $null = Deploy $record.recovery_setup 'recover' @('--transaction', $record.transaction)
    Assert-Native $record.second_image
    Move-Item -LiteralPath (Join-Path $fixture 'dist') -Destination (Join-Path $fixture 'source-offline')
    Move-Item -LiteralPath (Join-Path $fixture 'zup.toml') -Destination (Join-Path $fixture 'source-offline.toml')
    $null = Deploy $record.setup 'rollback' @('--revision', '1')
    Assert-Native $record.image
    if ([IO.File]::ReadAllText((Join-Path $install 'fixed.conf')) -ne 'configuration-1') { throw 'Rollback did not restore the fixed instance.' }
    $record.boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().Ticks
    [IO.File]::WriteAllText($Context, ($record | ConvertTo-Json), $utf8)
    @{ result='prepared_for_native_boot'; checks=@('failed_start_recovery_after_boot', 'source_free_rollback', 'fixed_resource_restoration') } | ConvertTo-Json
} else {
    $record = Get-Content -LiteralPath $Context -Raw | ConvertFrom-Json
    $fixture=$record.fixture; $serviceName=$record.service; $data=$record.data; $install=$record.install
    if ((Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().Ticks -le $record.boot) { throw 'A real guest reboot is required.' }
    Assert-Native $record.image
    $offline = "$($record.state).offline-verification"
    Move-Item -LiteralPath $record.state -Destination $offline
    try {
        Stop-Service -Name $serviceName
        (Get-Service -Name $serviceName).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(20))
        Start-Service -Name $serviceName
        (Get-Service -Name $serviceName).WaitForStatus('Running', [TimeSpan]::FromSeconds(20))
        Assert-Native $record.image
    } finally { Move-Item -LiteralPath $offline -Destination $record.state }
    $null = Deploy $record.setup 'uninstall'
    if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) { throw 'Uninstall retained the native service.' }
    foreach ($index in 1..6) { $null = Deploy $record.setup 'cleanup' @('--keep', '0') }
    if ([IO.File]::ReadAllText((Join-Path $data 'sentinel')) -ne 'persistent' -or [IO.File]::ReadAllText((Join-Path $install 'foreign.txt')) -ne 'unrelated') { throw 'Uninstall changed preserved data or foreign content.' }
    if (Test-Path -LiteralPath (Join-Path $install 'fixed.conf')) { throw 'Uninstall retained the fixed resource.' }
    @{ result='passed'; application=$record.application; checks=@('native_boot_pinned_to_rollback_generation', 'service_restart_without_zup_metadata', 'uninstall_preserves_data_and_foreign_files', 'source_free_retired_generation_cleanup') } | ConvertTo-Json
}
