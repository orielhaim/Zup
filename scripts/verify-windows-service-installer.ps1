param(
    [Parameter(Mandatory)][string]$Zup,
    [Parameter(Mandatory)][string]$Runtime,
    [Parameter(Mandatory)][string]$Service,
    [switch]$RecoveryPolicy
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding
$principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this test in an isolated administrator guest.'
}
$suffix = [Guid]::NewGuid().ToString('N')
$application = "com.example.zup-scm-$suffix"
$root = Join-Path $env:ProgramFiles "ZupInstallerTest-$suffix"
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Path $dist | Out-Null
$utf8 = [Text.UTF8Encoding]::new($false)
$names = @{ storage = "zup-$suffix-storage"; api = "zup-$suffix-api" }
$installed = $false
$verified = $false
$lastSetup = $null
$checks = [Collections.Generic.List[string]]::new()

function Invoke-Native([string]$Executable, [string[]]$Arguments, [switch]$AllowFailure) {
    $stdout = Join-Path $root 'stdout.txt'
    $stderr = Join-Path $root 'stderr.txt'
    # These test arguments contain no quotes or trailing directory separators.
    $quoted = $Arguments | ForEach-Object {
        if ($_ -match '["\r\n]' -or $_.EndsWith('\')) { throw 'Invalid fixture argument.' }
        '"' + $_ + '"'
    }
    $process = Start-Process -FilePath $Executable -ArgumentList $quoted -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $null = $process.Handle
    if (-not $process.WaitForExit(240000)) {
        $process.Kill()
        throw "Native test timed out; retain $root for recovery."
    }
    $process.WaitForExit()
    $output = [IO.File]::ReadAllText($stdout)
    if ($process.ExitCode -ne 0 -and -not $AllowFailure) {
        throw "Native command failed ($($process.ExitCode)): $output $([IO.File]::ReadAllText($stderr))"
    }
    @{ code = $process.ExitCode; output = $output }
}

function Build-Installer([int]$Version, [string]$Configuration, [switch]$FailStart, [int]$PayloadVersion = 0, [string]$Startup = 'manual', [string]$AppVersion = '', [string]$DisplaySuffix = '', [string]$InitialState = 'running') {
    if ($PayloadVersion -eq 0) { $PayloadVersion = $Version }
    if (-not $AppVersion) { $AppVersion = "$Version.0.0" }
    foreach ($name in @('storage', 'api')) {
        $bytes = [IO.File]::ReadAllBytes($Service)
        $marker = if ($FailStart -and $name -eq 'api') { 'zup-native-start-failure' } else { "fixture-version-$PayloadVersion" }
        [IO.File]::WriteAllBytes((Join-Path $dist "$name.exe"), [byte[]]($bytes + [Text.Encoding]::ASCII.GetBytes($marker)))
    }
    [IO.File]::WriteAllText((Join-Path $dist 'shared.conf'), $Configuration, $utf8)
    $declarations = ''
    foreach ($name in @('storage', 'api')) {
        $dependency = if ($name -eq 'api') { "requires_services = [`"$($names.storage)`"]" } else { '' }
        $failure = if ($RecoveryPolicy) { 'failure = { type = "windows", delay_seconds = 2, restart_attempts = 2, reset_seconds = 60 }' } else { '' }
        $jobReference = '${job.' + $names[$name] + '}'
        $declarations += @"

[[services]]
id = "$($names[$name])"
native_name = "$($names[$name])"
display_name = "$($names[$name])$DisplaySuffix"
binary = "`${install}/$name.exe"
arguments = ["$($names[$name])", "normal", "--job", "$jobReference"]
requires_files = ["`${install}/shared.conf"]
$dependency
$failure
start = "$Startup"
execution = { type = "windows_job", account = "local_service" }
lifecycle = { install = "$InitialState", change = "preserve" }
"@
    }
    $manifest = Join-Path $root 'zup.toml'
    [IO.File]::WriteAllText($manifest, @"
schema = 1
frontend = "headless"
[app]
id = "$application"
native_name = "Native SCM installer verification"
version = "$AppVersion"
[build.targets.default]
target = "windows-x64"
source = { directory = "dist" }
[install]
scope = "machine"
[install.directory]
machine = "`${location.programs}/$application"
[[files]]
id = "application"
source = "*.exe"
placement = { kind = "fixed", directory = "`${install}", replacement = "quiescent" }
executable = true
[[files]]
id = "shared"
source = "shared.conf"
placement = { kind = "fixed", directory = "`${install}", replacement = "quiescent" }
$declarations
"@, $utf8)
    $setup = Join-Path $root "setup-$Version.exe"
    $null = Invoke-Native $Zup @('build', '--manifest', $manifest, '--runtime', $Runtime, '--output', $setup)
    $setup
}

function Deploy([string]$Setup, [string]$Action, [switch]$AllowFailure) {
    Invoke-Native $Setup @($Action, '--scope', 'machine', '--non-interactive', '--yes', '--output', 'json') -AllowFailure:$AllowFailure
}

function Observe {
    $result = @{}
    foreach ($name in @('storage', 'api')) {
        $serviceState = Get-CimInstance Win32_Service -Filter "Name='$($names[$name])'"
        if (-not $serviceState) { throw "Missing native service $name" }
        $ticks = if ($serviceState.ProcessId) { (Get-Process -Id $serviceState.ProcessId).StartTime.ToUniversalTime().Ticks } else { 0 }
        $serviceState | Add-Member -NotePropertyName ExecutionStartTicks -NotePropertyValue $ticks
        $result[$name] = $serviceState
    }
    $result
}

function Assert-Running($State) {
    foreach ($name in @('storage', 'api')) {
        if ($State[$name].State -ne 'Running' -or $State[$name].ProcessId -eq 0 -or $State[$name].StartName -ne 'NT AUTHORITY\LocalService') {
            throw "Service $name did not reach Running as LocalService."
        }
    }
}

try {
    if ($RecoveryPolicy) {
        $newStopped = Build-Installer 12 'initial stopped' -PayloadVersion 1 -AppVersion '0.9.0' -InitialState 'stopped'
        $null = Deploy $newStopped 'install'
        $installed = $true
        $lastSetup = $newStopped
        $created = Observe
        if ($created.api.State -ne 'Stopped' -or $created.storage.State -ne 'Stopped') { throw 'Protected new registration started unexpectedly.' }
        $null = Deploy $newStopped 'uninstall'
        $installed = $false
        $checks.Add('new_stopped_recovery_policy_registration')
    }
    $first = Build-Installer 1 'initial'
    $lastSetup = $first
    $installed = $true
    $null = Deploy $first 'install'
    $before = Observe
    Assert-Running $before
    $checks.Add('install_local_service')
    foreach ($policy in @(@{ version = 6; startup = 'automatic' }, @{ version = 7; startup = 'manual' })) {
        $policySetup = Build-Installer $policy.version 'initial' -PayloadVersion 1 -Startup $policy.startup -AppVersion "1.$($policy.version).0"
        $null = Deploy $policySetup 'upgrade'
        $lastSetup = $policySetup
        $preserved = Observe
        Assert-Running $preserved
        foreach ($name in @('storage', 'api')) {
            if ($preserved[$name].ProcessId -ne $before[$name].ProcessId -or $preserved[$name].ExecutionStartTicks -ne $before[$name].ExecutionStartTicks) { throw 'Boot policy update replaced native execution.' }
        }
    }
    $checks.Add('running_boot_policy_preserves_generation')
    $metadataSetup = Build-Installer 13 'initial' -PayloadVersion 1 -AppVersion '1.8.0' -DisplaySuffix ' updated'
    $null = Deploy $metadataSetup 'upgrade'
    $lastSetup = $metadataSetup
    $metadataState = Observe
    Assert-Running $metadataState
    foreach ($name in @('storage', 'api')) {
        if ($metadataState[$name].DisplayName -ne "$($names[$name]) updated") { throw 'Display name update was not applied.' }
        if ($metadataState[$name].ProcessId -ne $before[$name].ProcessId -or $metadataState[$name].ExecutionStartTicks -ne $before[$name].ExecutionStartTicks) { throw 'Display name update replaced native execution.' }
    }
    $checks.Add('display_name_preserves_generation')
    $second = Build-Installer 2 'updated'
    $null = Deploy $second 'upgrade'
    $lastSetup = $second
    $after = Observe
    Assert-Running $after
    foreach ($name in @('storage', 'api')) {
        if ($after[$name].ProcessId -eq $before[$name].ProcessId) { throw 'Running upgrade did not replace service process.' }
    }
    $checks.Add('running_upgrade_and_dependencies')
    $null = Deploy $second 'repair'
    $repaired = Observe
    foreach ($name in @('storage', 'api')) {
        if ($after[$name].ProcessId -ne $repaired[$name].ProcessId) { throw 'Unchanged repair restarted service.' }
    }
    $checks.Add('unchanged_repair')
    $failed = Build-Installer 3 'failed' -FailStart
    if ((Deploy $failed 'upgrade' -AllowFailure).code -eq 0) { throw 'Failed startup was accepted.' }
    Assert-Running (Observe)
    $checks.Add('failed_start_compensation')
    foreach ($name in @('api', 'storage')) {
        Stop-Service -Name $names[$name]
        (Get-Service -Name $names[$name]).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(15))
    }
    $null = Deploy $second 'repair'
    $checks.Add('stopped_unchanged_repair')
    if ($RecoveryPolicy) {
        foreach ($policy in @(@{ version = 10; startup = 'automatic' }, @{ version = 11; startup = 'manual' })) {
            $policySetup = Build-Installer $policy.version 'updated' -PayloadVersion 2 -Startup $policy.startup -AppVersion "2.$($policy.version).0"
            $null = Deploy $policySetup 'upgrade'
            $lastSetup = $policySetup
            $preserved = Observe
            if ($preserved.api.State -ne 'Stopped' -or $preserved.storage.State -ne 'Stopped') { throw 'Boot policy update started a stopped service with recovery actions.' }
        }
        $checks.Add('stopped_recovery_policy_preserves_state')
    }
    $fourth = Build-Installer 4 'stopped'
    if ($RecoveryPolicy) {
        $refused = Deploy $fourth 'upgrade' -AllowFailure
        if ($refused.code -eq 0 -or -not $refused.output.Contains('pending recovery actions')) { throw 'Ambiguous queued SCM recovery was not refused explicitly.' }
        $checks.Add('stopped_recovery_queue_refusal')
    } else {
        $null = Deploy $fourth 'upgrade'
        $lastSetup = $fourth
        $checks.Add('stopped_upgrade')
    }
    $stopped = Observe
    if ($stopped.api.State -ne 'Stopped' -or $stopped.storage.State -ne 'Stopped') { throw 'Stopped upgrade started a service.' }
    if (-not $RecoveryPolicy) {
        foreach ($policy in @(@{ version = 8; startup = 'automatic' }, @{ version = 9; startup = 'manual' })) {
            $policySetup = Build-Installer $policy.version 'stopped' -PayloadVersion 4 -Startup $policy.startup -AppVersion "4.$($policy.version).0"
            $null = Deploy $policySetup 'upgrade'
            $lastSetup = $policySetup
            $preserved = Observe
            if ($preserved.api.State -ne 'Stopped' -or $preserved.storage.State -ne 'Stopped') { throw 'Boot policy update started a stopped service.' }
        }
        $checks.Add('stopped_boot_policy_preserves_state')
    }
    $null = Deploy $lastSetup 'uninstall'
    $installed = $false
    foreach ($name in @('storage', 'api')) {
        if (Get-Service -Name $names[$name] -ErrorAction SilentlyContinue) { throw 'Uninstall retained a service.' }
    }
    $checks.Add('uninstall')
    $historyCounts = [Collections.Generic.List[int]]::new()
    $hash = [Security.Cryptography.SHA256]::Create()
    try {
        $stateKey = -join ($hash.ComputeHash([Text.Encoding]::UTF8.GetBytes("zup:application-state:1`0$application")) | ForEach-Object { $_.ToString('x2') })
    } finally { $hash.Dispose() }
    $transactions = Join-Path $env:ProgramData "zup/apps/$stateKey/transactions"
    for ($index = 0; $index -lt 6; $index++) {
        $null = Invoke-Native $lastSetup @('cleanup', '--keep', '0', '--scope', 'machine', '--non-interactive', '--yes', '--output', 'json')
        $historyCounts.Add(@(Get-ChildItem -LiteralPath $transactions -Filter transaction.json -Recurse -File).Count)
    }
    if ($historyCounts[5] -gt $historyCounts[4] -or $historyCounts[4] -gt $historyCounts[3]) {
        throw "Settled machine cleanup accumulated journals: $historyCounts"
    }
    $maintenance = Join-Path $env:ProgramFiles "$application/.zup/maintenance"
    if ((Test-Path -LiteralPath $maintenance) -and @(Get-ChildItem -LiteralPath $maintenance -Directory).Count) {
        throw 'Removed fixed-only application retained maintenance generations after explicit cleanup.'
    }
    $checks.Add('machine_retired_payload_and_history_collection')
    $verified = $true
    @{ result = 'passed'; application = $application; native_recovery_policy = [bool]$RecoveryPolicy; history_counts = $historyCounts.ToArray(); checks = $checks.ToArray() } | ConvertTo-Json -Depth 4
} finally {
    if ($installed -and $lastSetup) {
        $cleanup = Deploy $lastSetup 'uninstall' -AllowFailure
        if ($cleanup.code -ne 0) { Write-Warning "Cleanup requires recovery: $($cleanup.output). Retained $root" }
    } elseif ($verified) {
        $fixture = Get-Item -LiteralPath $root
        $programs = (Get-Item -LiteralPath $env:ProgramFiles).FullName
        if ($fixture.Parent.FullName -ne $programs -or $fixture.Name -ne "ZupInstallerTest-$suffix" -or ($fixture.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'The disposable fixture cleanup boundary changed.'
        }
        Remove-Item -LiteralPath $fixture.FullName -Recurse -Force
    } else {
        Write-Warning "Verification failed; retained disposable fixture $root for recovery."
    }
}
