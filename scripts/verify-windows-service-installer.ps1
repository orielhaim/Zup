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

function Build-Installer([int]$Version, [string]$Configuration, [switch]$FailStart) {
    foreach ($name in @('storage', 'api')) {
        $bytes = [IO.File]::ReadAllBytes($Service)
        $marker = if ($FailStart -and $name -eq 'api') { 'zup-native-start-failure' } else { "fixture-version-$Version" }
        [IO.File]::WriteAllBytes((Join-Path $dist "$name.exe"), [byte[]]($bytes + [Text.Encoding]::ASCII.GetBytes($marker)))
    }
    [IO.File]::WriteAllText((Join-Path $dist 'shared.conf'), $Configuration, $utf8)
    $declarations = ''
    foreach ($name in @('storage', 'api')) {
        $dependency = if ($name -eq 'api') { "requires_services = [`"$($names.storage)`"]" } else { '' }
        $failure = if ($RecoveryPolicy) { 'failure = { type = "windows", delay_seconds = 2, restart_attempts = 2, reset_seconds = 60 }' } else { '' }
        $declarations += @"

[[services]]
id = "$($names[$name])"
name = "$($names[$name])"
binary = "`${install}/$name.exe"
arguments = ["$($names[$name])"]
requires_files = ["`${install}/shared.conf"]
$dependency
$failure
start = "manual"
execution = { type = "windows", account = "local_service" }
lifecycle = { install = "running", change = "preserve" }
"@
    }
    $manifest = Join-Path $root 'zup.toml'
    [IO.File]::WriteAllText($manifest, @"
schema = 1
frontend = "headless"
[app]
id = "$application"
name = "Native SCM installer verification"
version = "$Version.0.0"
[build.targets.default]
target = "windows-x64"
source = { directory = "dist" }
[install]
scope = "machine"
[install.directory]
machine = "`${location.programs}/$application"
[[files]]
source = "*.exe"
destination = "`${install}"
executable = true
[[files]]
source = "shared.conf"
destination = "`${install}"
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
    $first = Build-Installer 1 'initial'
    $lastSetup = $first
    $installed = $true
    $null = Deploy $first 'install'
    $before = Observe
    Assert-Running $before
    $checks.Add('install_local_service')
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
    $null = Deploy $lastSetup 'uninstall'
    $installed = $false
    foreach ($name in @('storage', 'api')) {
        if (Get-Service -Name $names[$name] -ErrorAction SilentlyContinue) { throw 'Uninstall retained a service.' }
    }
    $checks.Add('uninstall')
    @{ result = 'passed'; application = $application; native_recovery_policy = [bool]$RecoveryPolicy; checks = $checks.ToArray() } | ConvertTo-Json -Depth 4
} finally {
    if ($installed -and $lastSetup) {
        $cleanup = Deploy $lastSetup 'uninstall' -AllowFailure
        if ($cleanup.code -ne 0) { Write-Warning "Cleanup requires recovery: $($cleanup.output). Retained $root" }
    } else {
        Remove-Item -LiteralPath $root -Recurse -Force
    }
}
