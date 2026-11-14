# F13 journal measurements and validation

The baseline is PR #17 dispatch `1447e0e2f4e254f45e67775abde15a87ee461016`.
Its store loaded and validated the complete snapshot on update, read it again
under CAS, then pretty-serialized and republished the complete plan and accumulated
receipts. The coordinator did this several times per node. Metadata transfers were
quadratic even though each native effect concerned a single file.

The replacement keeps immutable intent once and commits typed deltas through a
small atomic head. Its [durability and authority contract](../reference/durable-journal.md)
describes checkpoint selection, corruption refusal and bounded retention.

## Reproduce

Run from the repository root on Windows:

```powershell
./scripts/benchmark-journal-baseline.ps1 -Sizes '1,16,128,512'
cargo test --release -p zup-transaction --test journal_benchmark -- --ignored --nocapture --test-threads=1
```

The baseline script exports the exact dispatch source to a temporary directory,
adds counters and the same scaling test, and builds it without switching the
current branch or modifying the index. `ZUP_BENCH_SIZES` selects replacement sizes.
The baseline script was also rerun at size 1 to verify the historical reproducer.

Both runs use Rust 1.100.0 release builds, fresh NTFS directories on the same
Windows x64 host, and realistic coordinator transitions over the native private
filesystem store. File effects use the existing contract-valid fake executor.
Paths include long names; receipts include their destinations and native identity
fields. Native effect integration is covered separately by the platform suites.

Counters measure store-level logical transfers, including the initial intent
write. They exclude F11 initialization/reservation transfers, lock-seal reads,
filesystem metadata and physical device writes. They count an attempted write
before publication. The benchmark is a successful run, so all counted writes
complete. Wall times are individual observations on a developer host, with other
validation/build activity during portions of the session. Treat them as indicative,
not an isolated disk throughput comparison. Peak resident memory was not measured.

Raw counts, peak live metadata file sizes, largest individual writes, final disk footprints, timings and
the repeated-recovery series are in [the measurement data](f13-journal-measurements.json).
`peak_file_bytes` measures the largest live journal file observed after a commit.
`largest_write` measures a publication unit. Neither measures temporary atomic-publication
overlap, total peak disk usage or OS cache memory. MB below means 1,000,000 bytes.

## Results

| Files / nodes | Before writes MB | After writes MB | Before reads MB | After reads MB | Before / after wall seconds |
| ------------- | ---------------: | --------------: | --------------: | -------------: | --------------------------: |
| 1 / 8         |            0.292 |           0.026 |           0.579 |          0.021 |               0.434 / 0.773 |
| 16 / 38       |           12.764 |           0.210 |          25.478 |          0.159 |               2.141 / 3.104 |
| 128 / 262     |          675.153 |           1.583 |       1,349.917 |          1.187 |             25.832 / 18.984 |
| 512 / 1,030   |       10,583.943 |           6.302 |      21,166.339 |          4.720 |            300.894 / 74.426 |

At 128 files, writes fell **426.4x** and reads **1,137.0x**. At 512 files,
the reductions were **1,679.4x** and **4,484.4x**. Multiplying files by four
from 128 to 512 multiplied replacement writes by 3.98, versus 15.68 before.
The small transaction still pays for two durable publications per ordinary commit.
Metadata size improvement does not imply the same wall-time improvement.

| Files | Before reads / writes | After reads / writes | Before / after disk MB | Before / after cold load ms |
| ----- | --------------------: | -------------------: | ---------------------: | --------------------------: |
| 1     |               40 / 20 |              22 / 39 |          0.016 / 0.020 |               3.000 / 5.956 |
| 16    |              190 / 95 |             97 / 189 |          0.147 / 0.174 |              4.362 / 12.853 |
| 128   |           1,310 / 655 |          657 / 1,309 |          1.126 / 1.329 |             11.849 / 76.753 |
| 512   |         5,150 / 2,575 |        2,577 / 5,149 |          4.490 / 5.300 |           69.349 / 1113.134 |

Cold load creates a fresh store, reconstructs and validates the exact final record,
and completes its durability barrier. It is **slower** than decoding the old single
snapshot because each committed transition receives state validation. Replay bytes
are bounded by the checkpoint budget, but mutable-state validation still scans the
record. This implementation makes no linear-CPU claim. Large plans can justify a
later independently verified incremental validator; durability has not been weakened
to improve this number.

The repeated-recovery benchmark preserves a committed record and its receipts while
writing 1,024 sizable retry diagnostics. At 256, 512, 768 and 1,024 attempts, disk
usage was 188,888, 198,047, 207,208 and 216,419 bytes. Final cumulative writes were
10,070,305 bytes; cold load took 7.700 ms. The peak live metadata file was 128,294 bytes. Both checkpoint slots were reused under
the existing lease. The format test independently verifies seven-file boundedness,
all verified receipts, and refusal after the active checkpoint disappears.

## Validation

The final Windows/shared run passed **723 tests** and the final native Linux/shared
run passed **819 tests**. Each run skipped 23 ignored or platform-excluded tests,
including the two opt-in benchmarks. These overlapping counts must not be added.

Executed checks include:

- Windows `cargo nextest` for `zup-transaction`, `zup-windows`, `zup-plan`,
  `zup-storage`, `zup-protocol` and `zup-runtime`, with `zup-transaction/test-faults`.
- Native Ubuntu WSL `cargo nextest` for `zup-linux` and those shared crates, with
  the same fault feature. WSL identity and mounted-source read-only health probes
  succeeded. Build artifacts used `/home/oriel/zup-f13-target`; no host or VM
  settings were changed. Final logs were written on the host because `/tmp` is
  ephemeral across WSL sessions.
- Windows-to-Linux `cargo check -p zup-transaction --target x86_64-unknown-linux-gnu`.
- Workspace, all-target, all-feature Clippy with warnings denied; rustfmt and
  `git diff --check`.
- Doctest commands for the six Windows/shared crates above, plus native Linux
  `zup-linux` and `zup-transaction`. All selected crates currently have zero doctests.
- Release-mode native scaling, repeated-recovery and historical size-1 reproducer.

The native suites include F08 explicit forward authorization, F09 lost publication
acknowledgement and native-object drift, F10 preparation retirement, F11 process
creation/CAS races and identity substitution, and F12 cleanup DAGs and deferred
receipts. Linux interruption tests terminate a process around real filesystem
effects and recover with a new executor. Windows retained-source tests remove the
original overlay; the new test compacts and cold-loads that journal before opening
the retained source and verifies that pending work still prevents collection.

New format tests exercise partial headers/payloads, uncommitted complete frames,
checksum damage, committed truncation, missing head/checkpoint, changed intent,
native log substitution, stale CAS, invalid state transitions, and discontinuous or
replayed revisions even with a recomputed checksum. Process-abort sweeps cover
initial log/head publication, frame header/payload/flush, head selection, inactive
checkpoint publication and both sides of checkpoint selection. A durable native
effect marker must retain its Running intent after restart. A 128-file failed
installation preserves compensation evidence through cold replay.

Not executed: elevated Windows machine/SCM configurations, Linux root-only tests,
real systemd/OpenRC VM lifecycle suites, other architectures/filesystems, reboot or
power-cut testing. The Windows token is not elevated; `sudo -n` requires interactive
authentication. Those configurations remain skipped, not counted as passing.
Process termination does not prove sudden-power-loss safety. Native flush and
identity semantics remain subject to the platform limits documented in the contract.
