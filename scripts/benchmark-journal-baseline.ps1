param([string]$Sizes = '1,16,128,512')
$ErrorActionPreference = 'Stop'
$repository = Split-Path $PSScriptRoot -Parent
$dispatch = '1447e0e2f4e254f45e67775abde15a87ee461016'
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('zup-f13-baseline-' + [guid]::NewGuid())
$source = Join-Path $scratch 'source'
New-Item -ItemType Directory -Path $source | Out-Null
git -C $repository archive --format=tar -o (Join-Path $scratch 'source.tar') $dispatch
if ($LASTEXITCODE) { throw 'Cannot export the baseline commit' }
tar -xf (Join-Path $scratch 'source.tar') -C $source
if ($LASTEXITCODE) { throw 'Cannot extract the baseline commit' }

$crate = Join-Path $source 'crates/zup-transaction'
$metrics = Get-Content (Join-Path $repository 'crates/zup-transaction/src/journal_io.rs') -Raw
[IO.File]::WriteAllText((Join-Path $crate 'src/journal_io.rs'), $metrics)
Add-Content (Join-Path $crate 'src/lib.rs') "`nmod journal_io;`npub use journal_io::JournalIo;"
$storagePath = Join-Path $crate 'src/storage.rs'
$storage = Get-Content $storagePath -Raw
$storage = $storage.Replace('volume: Arc<dyn JournalVolume>,', "volume: Arc<dyn JournalVolume>,`n    io: Arc<crate::journal_io::Counters>,")
$storage = $storage.Replace('volume: Arc::new(PrivateJournals::new(root.into())),', "volume: Arc::new(PrivateJournals::new(root.into())),`n            io: Arc::default(),")
$storage = $storage.Replace('Self { volume }', 'Self { volume, io: Arc::default() }')
$storage = $storage.Replace('    pub fn list_ids', "    pub fn io_stats(&self) -> crate::JournalIo { self.io.snapshot() }`n`n    pub fn list_ids")
$storage = $storage.Replace('match self.volume.create_journal(&id, &bytes) {', "self.io.write(bytes.len());`n        match self.volume.create_journal(&id, &bytes) {")
$storage = $storage.Replace('let record = decode_record(&bytes)?;', "self.io.read(bytes.len());`n                let record = decode_record(&bytes)?;")
$storage = $storage.Replace('let current = decode_record(&journal.read()?)?;', "let bytes = journal.read()?;`n        self.io.read(bytes.len());`n        let current = decode_record(&bytes)?;")
$storage = $storage.Replace('journal.publish(&bytes)?;', "self.io.write(bytes.len());`n        journal.publish(&bytes)?;")
[IO.File]::WriteAllText($storagePath, $storage)
$benchmark = Get-Content (Join-Path $repository 'crates/zup-transaction/tests/journal_benchmark.rs') -Raw
$end = $benchmark.IndexOf('#[test]', $benchmark.IndexOf('fn journal_scaling'))
if ($end -lt 0) { throw 'Cannot isolate the scaling benchmark' }
[IO.File]::WriteAllText((Join-Path $crate 'tests/journal_benchmark.rs'), $benchmark.Substring(0, $end))

$previousSizes = $env:ZUP_BENCH_SIZES
$previousTarget = $env:CARGO_TARGET_DIR
try {
    $env:ZUP_BENCH_SIZES = $Sizes
    $env:CARGO_TARGET_DIR = Join-Path $repository 'target/f13-baseline'
    cargo test --release --manifest-path (Join-Path $source 'Cargo.toml') -p zup-transaction --test journal_benchmark -- --ignored --nocapture
    if ($LASTEXITCODE) { throw 'Baseline benchmark failed' }
} finally {
    $env:ZUP_BENCH_SIZES = $previousSizes
    $env:CARGO_TARGET_DIR = $previousTarget
    Write-Host "Baseline source and artifacts: $scratch"
}
