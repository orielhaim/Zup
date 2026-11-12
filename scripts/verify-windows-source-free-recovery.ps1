param(
    [Parameter(Mandatory)][ValidateSet('prepare','recover')][string]$Phase,
    [Parameter(Mandatory)][string]$Fixture,
    [switch]$Acquired
)
$ErrorActionPreference = 'Stop'
$utf8 = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $utf8
if (-not ([Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run in an isolated elevated Windows guest.' }
$Fixture = (Resolve-Path -LiteralPath $Fixture).Path
$spec = Get-Content -LiteralPath (Join-Path $Fixture 'fixture.json') -Raw | ConvertFrom-Json
$install = Join-Path $env:ProgramFiles $spec.install_folder
$sha = [Security.Cryptography.SHA256]::Create()
try { $key = -join($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes("zup:application-state:1`0$($spec.app)")) | ForEach-Object { $_.ToString('x2') }) } finally { $sha.Dispose() }
$state = Join-Path $env:ProgramData "zup/apps/$key"
$context = Join-Path $Fixture 'recovery.json'
$carrier = Join-Path $Fixture 'v1-maintenance.exe'
function Invoke-Carrier([string]$Executable, [string[]]$Arguments, [switch]$AllowFailure) {
    $quoted = $Arguments | ForEach-Object {
        if ($_ -match '["\r\n]' -or $_.EndsWith('\')) { throw 'Unsupported fixture argument.' }
        '"' + $_ + '"'
    }
    $out = Join-Path $Fixture 'stdout.txt'
    $err = Join-Path $Fixture 'stderr.txt'
    $process = Start-Process -FilePath $Executable -ArgumentList $quoted -PassThru -WindowStyle Hidden -RedirectStandardOutput $out -RedirectStandardError $err
    $null = $process.Handle
    if (-not $process.WaitForExit(120000)) { $process.Kill(); throw 'Carrier timed out.' }
    $process.WaitForExit()
    $text = [IO.File]::ReadAllText($out) + [IO.File]::ReadAllText($err)
    if (-not $AllowFailure -and $process.ExitCode -ne 0) { throw "Carrier failed ($($process.ExitCode)): $text" }
    @{ code=$process.ExitCode; text=$text }
}
function Ledger {
    $files = @(Get-ChildItem -LiteralPath (Join-Path $state 'installations') -Filter '*.json')
    if ($files.Count -ne 1) { throw 'Expected one installation record.' }
    (Get-Content -LiteralPath $files[0].FullName -Raw | ConvertFrom-Json).record
}
if ($Phase -eq 'prepare') {
    if (-not (Test-Path -LiteralPath (Join-Path $state 'installations'))) {
        $null = Invoke-Carrier (Join-Path $Fixture 'setup-1.0.0.exe') @('install','--scope','machine','--non-interactive','--yes','--output','json')
    }
    $initial = Ledger
    if ($initial.current.version -ne '1.0.0') { throw 'Fresh installation did not commit.' }
    Copy-Item -LiteralPath (Join-Path $initial.current.maintenance.directory.path 'maintenance.exe') -Destination $carrier
    $env:ZUP_TEST_FAULT_POINT = 'apply_returned'
    $env:ZUP_TEST_FAULT_OPERATION = 'generation:activate:'
    $env:ZUP_TEST_FAULT_ACTION = 'crash'
    try {
        $interrupted = if ($Acquired) { Invoke-Carrier $carrier @('update','--scope','machine','--non-interactive','--yes','--output','json','--source',(Join-Path $Fixture 'repository')) -AllowFailure }
        else { Invoke-Carrier (Join-Path $Fixture 'setup-2.0.0.exe') @('upgrade','--scope','machine','--non-interactive','--yes','--output','json') -AllowFailure }
    }
    finally { Remove-Item Env:ZUP_TEST_FAULT_POINT,Env:ZUP_TEST_FAULT_OPERATION,Env:ZUP_TEST_FAULT_ACTION }
    if ($interrupted.code -eq 0) { throw 'Fault injection did not interrupt the upgrade.' }
    $pending = @(Get-ChildItem -LiteralPath (Join-Path $state 'transactions') -Directory | ForEach-Object {
        $record = Get-Content -LiteralPath (Join-Path $_.FullName 'transaction.json') -Raw | ConvertFrom-Json
        if ($record.app_version -eq '2.0.0') { $record }
    })
    if ($pending.Count -ne 1) { throw "Expected one newer transaction: $($interrupted.text)" }
    if ($pending[0].schema -ne 2) { throw 'Fixture runtime predates the retained-source journal compatibility gate.' }
    $transaction = $pending[0].transaction_id
    $journal = Join-Path $state "transactions/$transaction/transaction.json"
    $retained = Join-Path $state "transactions/$transaction/recovery"
    $source = Join-Path $retained 'source.json'
    $bytes = [IO.File]::ReadAllBytes($source)
    $manifest = [Text.Encoding]::UTF8.GetString($bytes) | ConvertFrom-Json
    $generated = @($manifest.files.PSObject.Properties | Where-Object { $_.Name.StartsWith('__zup_plugins__/') })
    if ($Acquired) {
        if ($manifest.release.trust_anchor -ne $spec.trust_anchor -or $manifest.release.release -ne $spec.release) { throw 'Protected source lacks authenticated release provenance.' }
        $generated = @($manifest.files.PSObject.Properties | Where-Object { $_.Name -eq 'payload.bin' })
    } elseif ($generated.Count -eq 0) { throw 'Generated overlay is absent from retained inventory.' }
    if ($generated.Count -eq 0) { throw 'Candidate payload is absent from the retained inventory.' }
    $blob = Join-Path $retained $generated[0].Value.sha256
    $backup = Join-Path $Fixture 'protected-blob-backup'
    $journalBytes = [Convert]::ToBase64String([IO.File]::ReadAllBytes($journal))
    Move-Item -LiteralPath $blob -Destination $backup
    try { $refused = Invoke-Carrier $carrier @('recover','--scope','machine','--transaction',$transaction,'--output','json') -AllowFailure }
    finally { Move-Item -LiteralPath $backup -Destination $blob }
    if ($refused.code -eq 0 -or -not $refused.text.Contains($transaction)) { throw 'Missing exact content did not produce actionable pending recovery.' }
    if ([Convert]::ToBase64String([IO.File]::ReadAllBytes($journal)) -ne $journalBytes) { throw 'Missing-source refusal changed journal state.' }
    $manifest.execution_format = 2
    [IO.File]::WriteAllText($source, ($manifest | ConvertTo-Json -Depth 100), $utf8)
    try { $refused = Invoke-Carrier $carrier @('recover','--scope','machine','--transaction',$transaction,'--output','json') -AllowFailure }
    finally { [IO.File]::WriteAllBytes($source, $bytes) }
    if ($refused.code -eq 0 -or -not $refused.text.Contains('format')) { throw 'Incompatible worker format was accepted.' }
    if ([Convert]::ToBase64String([IO.File]::ReadAllBytes($journal)) -ne $journalBytes) { throw 'Compatibility refusal changed journal state.' }
    $blocked = Invoke-Carrier $carrier @('cleanup','--scope','machine','--keep','0','--non-interactive','--yes','--output','json') -AllowFailure
    if ($blocked.code -eq 0 -or -not (Test-Path -LiteralPath $blob)) { throw 'Cleanup collected a live recovery root.' }
    Remove-Item -LiteralPath (Join-Path $Fixture 'setup-1.0.0.exe')
    if ($Acquired) {
        $repository = (Resolve-Path -LiteralPath (Join-Path $Fixture 'repository')).Path
        $cache = Join-Path $env:LOCALAPPDATA "zup/apps/$key/content"
        foreach ($candidate in @($repository, $cache)) {
            $resolved = (Resolve-Path -LiteralPath $candidate).Path
            if ($resolved -ne (Join-Path $Fixture 'repository') -and $resolved -ne (Join-Path $env:LOCALAPPDATA "zup/apps/$key/content")) { throw 'Unexpected source directory.' }
            Remove-Item -LiteralPath $resolved -Recurse -Force
        }
    } else { Remove-Item -LiteralPath (Join-Path $Fixture 'setup-2.0.0.exe') }
    @{ transaction=$transaction; boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().Ticks } | ConvertTo-Json | ForEach-Object { [IO.File]::WriteAllText($context, $_, $utf8) }
    $sourceCheck = if ($Acquired) { 'authenticated_release_retained' } else { 'generated_overlay_retained' }
    @{ result='prepared_for_reboot'; checks=@('machine_install',$sourceCheck,'missing_blob_refused','future_format_refused','pending_source_not_collected','original_installers_absent') } | ConvertTo-Json
} else {
    $pending = Get-Content -LiteralPath $context -Raw | ConvertFrom-Json
    if ((Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().Ticks -le $pending.boot) { throw 'A real guest reboot is required.' }
    $completion = Invoke-Carrier $carrier @('recover','--scope','machine','--transaction',$pending.transaction,'--output','json')
    if (($completion.text | ConvertFrom-Json).version -ne '2.0.0') { throw 'Recovery reported the carrier version instead of the transaction version.' }
    $settled = Ledger
    if ($settled.current.version -ne '2.0.0') { throw 'Original v1 carrier did not publish the v2 transaction.' }
    $payload = if ($Acquired) { 'payload.bin' } else { 'asset.bin' }
    if ([IO.File]::ReadAllText((Join-Path $settled.current.materialization.directory.path $payload)) -ne '2.0.0') { throw 'Recovery used different generation content.' }
    if ($Acquired) {
        if ($settled.current.release.trust_anchor -ne $spec.trust_anchor -or $settled.current.release.release -ne $spec.release) { throw 'Recovery published different release provenance.' }
    } else {
        $generated = [IO.File]::ReadAllText((Join-Path $install 'configure.txt'))
        if (-not $generated.Contains('version: 2.0.0') -or -not $generated.Contains('scope: machine')) { throw 'Recovery used a different generated overlay.' }
    }
    if (Test-Path -LiteralPath (Join-Path $state "transactions/$($pending.transaction)/recovery")) { throw 'Settled source was not collected.' }
    $currentCarrier = Join-Path $Fixture 'v2-maintenance.exe'
    Copy-Item -LiteralPath (Join-Path $settled.current.maintenance.directory.path 'maintenance.exe') -Destination $currentCarrier
    $null = Invoke-Carrier $currentCarrier @('uninstall','--scope','machine','--non-interactive','--yes','--output','json')
    foreach ($attempt in 1..4) { $null = Invoke-Carrier $currentCarrier @('cleanup','--scope','machine','--keep','0','--non-interactive','--yes','--output','json') }
    $sourceCheck = if ($Acquired) { 'exact_authenticated_release' } else { 'exact_generated_overlay' }
    @{ result='passed'; acquired=[bool]$Acquired; checks=@('reboot','v1_carrier_recovers_v2','no_installer',$sourceCheck,'retained_source_collected','uninstall_and_history_cleanup') } | ConvertTo-Json
}
