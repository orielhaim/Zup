param([Parameter(Mandatory)][string]$Tools)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$utf8 = [Text.UTF8Encoding]::new($false)
if (-not ([Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run in an isolated elevated Windows guest.'
}
$tag = [Guid]::NewGuid().ToString('N')
$app = "com.example.zup-publication-$tag"
$fixture = Join-Path $env:ProgramFiles "ZupPublicationTest-$tag"
$install = Join-Path $env:ProgramFiles $app
$dist = Join-Path $fixture 'dist'
New-Item -ItemType Directory -Path $dist | Out-Null
function Invoke-Native([string]$Executable, [string[]]$Arguments, [switch]$AllowFailure) {
    $quoted = $Arguments | ForEach-Object {
        if ($_ -match '["\r\n]' -or $_.EndsWith('\')) { throw 'Unsupported fixture argument.' }
        '"' + $_ + '"'
    }
    $stdout = Join-Path $fixture 'stdout.txt'
    $stderr = Join-Path $fixture 'stderr.txt'
    $process = Start-Process -FilePath $Executable -ArgumentList $quoted -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $null = $process.Handle
    if (-not $process.WaitForExit(240000)) { $process.Kill(); throw 'Fixture command timed out.' }
    $process.WaitForExit()
    $text = [IO.File]::ReadAllText($stdout)
    if ($process.ExitCode -ne 0 -and -not $AllowFailure) { throw "Command failed: $text $([IO.File]::ReadAllText($stderr))" }
    @{ code = $process.ExitCode; output = $text }
}
function Has-UsersAccess([string]$Path) {
    foreach ($rule in (Get-Acl -LiteralPath $Path).Access) {
        if ($rule.AccessControlType -eq 'Allow' -and $rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -eq 'S-1-5-32-545') { return $true }
    }
    return $false
}
[IO.File]::WriteAllText((Join-Path $dist 'first.txt'), 'first', $utf8)
[IO.File]::WriteAllText((Join-Path $dist 'second.txt'), 'second', $utf8)
$manifest = @'
schema = 1
frontend = "headless"
[app]
id = "APP_ID"
name = "Private publication verification"
version = "1.0.0"
[build.targets.default]
target = "windows-x64"
source = { directory = "dist" }
[install]
scope = "machine"
generation_readers = "native_executables"
[install.directory]
machine = "${location.programs}/APP_ID"
[[files]]
id = "content"
source = "*.txt"
'@
$manifestPath = Join-Path $fixture 'zup.toml'
[IO.File]::WriteAllText($manifestPath, $manifest.Replace('APP_ID', $app), $utf8)
$setup = Join-Path $fixture 'setup.exe'
$null = Invoke-Native (Join-Path $Tools 'zup.exe') @('build', '--manifest', $manifestPath, '--runtime', (Join-Path $Tools 'zup-setup-headless-windows-x64.exe'), '--output', $setup)
try {
    $env:ZUP_TEST_FAULT_POINT = 'materialization_member_exposed'
    $env:ZUP_TEST_FAULT_OPERATION = 'generation:seal:'
    $failed = Invoke-Native $setup @('install', '--scope', 'machine', '--non-interactive', '--yes', '--output', 'json') -AllowFailure
} finally {
    Remove-Item Env:ZUP_TEST_FAULT_POINT -ErrorAction SilentlyContinue
    Remove-Item Env:ZUP_TEST_FAULT_OPERATION -ErrorAction SilentlyContinue
}
if ($failed.code -eq 0) { throw 'Publication fault did not interrupt installation.' }
$hash = [Security.Cryptography.SHA256]::Create()
try { $key = -join($hash.ComputeHash([Text.Encoding]::UTF8.GetBytes("zup:application-state:1`0$app")) | ForEach-Object { $_.ToString('x2') }) } finally { $hash.Dispose() }
$state = Join-Path $env:ProgramData "zup/apps/$key"
$publishing = @(Get-ChildItem -LiteralPath (Join-Path $state 'materializations') -Filter '*.json' -File -Recurse | ForEach-Object {
    $record = Get-Content -LiteralPath $_.FullName -Raw | ConvertFrom-Json
    if ($record.state.state -eq 'publishing') { @{ path = $_.FullName; record = $record } }
})
if ($publishing.Count -ne 1) { throw 'Expected one protected publication intent.' }
$before = $publishing[0].record
$preparation = Join-Path (Split-Path $before.specification.directory.path -Parent) $before.state.directory
if (Has-UsersAccess $preparation) { throw 'Preparation root was published before its members.' }
$exposed = @(Get-ChildItem -LiteralPath $preparation -File -Recurse | Where-Object { Has-UsersAccess $_.FullName })
if ($exposed.Count -eq 0) { throw 'Fault did not exercise a partially exposed native tree.' }
$refused = Invoke-Native $setup @('repair', '--scope', 'machine', '--non-interactive', '--yes', '--output', 'json') -AllowFailure
if ($refused.code -ne 7) { throw "A pending publication was not refused as recovery-required: $($refused.output)" }
$diagnostic = $refused.output | ConvertFrom-Json
if ($diagnostic.transaction -ne $before.transaction -or -not $diagnostic.message) { throw 'Worker refusal lost its protected recovery transaction or diagnostic.' }
$null = Invoke-Native $setup @('recover', '--transaction', $before.transaction, '--scope', 'machine', '--output', 'json')
$after = Get-Content -LiteralPath $publishing[0].path -Raw | ConvertFrom-Json
if ($after.state.state -ne 'sealed' -or ($before.state.tree | ConvertTo-Json -Depth 100 -Compress) -ne ($after.state.tree | ConvertTo-Json -Depth 100 -Compress)) { throw 'Recovery changed the protected native tree identity.' }
$selected = $after.specification.directory.path
if (-not (Has-UsersAccess $selected)) { throw 'Recovery did not publish final access.' }
$null = Invoke-Native $setup @('repair', '--scope', 'machine', '--non-interactive', '--yes', '--output', 'json')
$null = Invoke-Native $setup @('uninstall', '--scope', 'machine', '--non-interactive', '--yes', '--output', 'json')
$null = Invoke-Native $setup @('cleanup', '--keep', '0', '--scope', 'machine', '--non-interactive', '--yes', '--output', 'json')
@{ result = 'passed'; application = $app; checks = @('partial_native_acl_publication', 'worker_recovery_hint_preserved', 'protected_intent_recovery', 'exact_tree_identity_preserved', 'repair_uninstall_cleanup') } | ConvertTo-Json
