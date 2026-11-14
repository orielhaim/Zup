# F20 recovery verification

Baseline: PR #17, `rewrite/hybrid-deployment-engine`,
`6fb67aab06ca31dec46124cf053781e7bc962330` (F19).

## Reproduction and contract

The F20 review in `Zup_PR17_Deep_Review.html` identifies a post-commit
housekeeping failure becoming `RecoveryRequired` without a transaction. The GUI
then refuses Retry, Repair and Recover. Reopening can hide that ephemeral state;
there is no journal for the cache failure itself.

Before changing production code, the strengthened session regression reproduced
that state. A separate regression used a real filesystem journal with two dependent
post-commit cleanup effects, interrupted collection, reopened the preset with no
installed ledger or desired target, and failed because recovery discovery returned
no transaction. Its executor simulates effects; its journal and preset IPC are real.

The fix authenticates recovery through the journal independently of presentation
or acquisition. A small typed projection carries the original identity and pending
cleanup locations to the preset. Native execution reloads authority under the
installation lock. A committed transaction can only finish cleanup and reconcile
ownership. Presentation failures retain verified settlement and offer Refresh.
See [the session contract](../reference/deployment-contract.md#session-recovery-and-presentation).

## Focused coverage

- `reopened_gui_finds_committed_cleanup_without_a_desired_target`: install,
  upgrade and uninstall, with missing or unreadable presentation ledger. Exercises
  close/reopen, stale UI action, a second deferral, exact recovery ID, different
  carrier version, settled journal and no pre-commit replay through real preset IPC.
- `post_commit_cleanup_failure_is_one_reconciliation_result`: post-commit cache
  failure has an enabled refresh action without inventing journal authority.
- `a_rollback_refresh_failure_preserves_settlement_and_has_a_safe_refresh_action`:
  rollback remains rollback when presentation fails; Refresh does not repeat recovery.
- `a_committed_warning_offers_only_the_owned_active_launcher`: the original
  installed launcher remains available even when absent from the current carrier;
  a removed deployment offers none. Launch execution rechecks protected journal
  evidence and does not rebuild a desired deployment.
- Shared `cleanup_lifecycle` fixtures perform native filesystem collection on
  Windows and Linux. A live executable reader defers cleanup twice; after it exits,
  recovery works with the original source directory deleted. Process-crash cases
  cover incomplete/lost cleanup acknowledgements and preserve the active generation.
- Windows runtime and multiprocess tests cover original journal identity, changed
  carrier version, missing payload, foreign app/target rejection, retained source,
  authenticated worker inspection and lifecycle serialization. Existing journal
  tamper, reader, sidecar and rollback suites remain enabled.
- Identical generated-input preparations use distinct physical staging directories.
  Cleaning one preserves the other's files. Unjournaled abandoned input caches are
  preserved because content identity alone does not authorize deleting them.

## Commands and results

Windows commands use `CARGO_BUILD_JOBS=2`:

```text
cargo fmt --all --check
cargo clippy -j 2 --workspace --all-targets --all-features -- -D warnings
cargo test -j 2 --workspace --all-features --doc
cargo xtask toolchain build --profile dev
cargo xtask fixtures build
cargo nextest run -j 2 --workspace --all-features --profile fast --no-fail-fast
cargo nextest run -j 2 -p zup-installer -p zup-preview --all-features --profile integration --no-fail-fast
```

The fast Windows workspace run passed 2,032 tests, with 51 skipped by platform,
privilege or explicit ignore gates. Formatting, workspace Clippy, doctests and
staged toolchain/fixture builds passed. The final staged installer/preview
integration run passed all 44 tests, with one skipped.

After extending authenticated launch inspection, the focused Windows run completed
614 tests: 612 passed and two failed. One journal-write failure reported Windows
error 1450 (insufficient system resources). The other exposed a test helper that
planned another install during read-only inspection. That helper now reads the
existing protected transaction. The final serial rerun passed all 60 selected
tests, covering both failures and all preset-host/session regressions. No native
validation or durability requirement was relaxed.

```text
cargo nextest run --test-threads 1 -p zup-session -p zup-preset-host -p zup-transaction -p zup-windows --all-features --profile fast -E 'package(zup-session) | package(zup-preset-host) | test(a_large_failed_install_preserves_compensation_evidence_after_restart) | test(a_worker_executes_only_within_its_authority_domain)'
```

The renewed standard Windows UAC request was approved. All three isolated elevated
tests passed: machine lock contention, protected journal CAS/reopen and
`machine_worker_discovers_and_recovers_the_original_journal_without_payload`.
The latter deletes the source payload, supplies an untrusted parent root and
verifies that the worker independently finds the protected original transaction.
It exercises prepared-transaction rollback and independently authenticated active
commit inspection, not a machine GUI upgrade with a live reader. The latter uses
an empty deployment and a test executor with a real protected journal, native
ownership publisher and worker; it does not prove native application effects.
No security policy or unrelated installation was changed.

```text
cargo nextest run -j 2 -p zup-windows --all-features --run-ignored only -E 'test(machine_worker_discovers_and_recovers_the_original_journal_without_payload) | test(machine_journals_use_shared_revision_cas_and_reopen) | test(a_second_process_sees_the_machine_lock_as_busy)' --no-fail-fast
```

The acquired-carrier crash recovery test passed using the instrumented test
console. A staged production console does not contain fault injection; using it
had incorrectly prevented the test's requested process crash. The fixture now
selects the instrumented console only for this fault test.

WSL Ubuntu was probed read-only before native execution; it was not repaired or
reconfigured. Linux workspace all-target/all-feature Clippy passed. The broader
Linux run completed 787 tests: 758 passed, 29 failed, 29 skipped. Failures include
machine fixture private-root mode mismatches (0755 versus required 0700) and
interrupted-service `Corrupt(InvalidPhase)` cases. A separate archive of the F19
baseline reproduced ten of these failures; the remaining nineteen were not all
individually rerun against baseline. The production ACL and journal validation
remain strict. The final focused Linux run passed all 314 tests (538 excluded by
the package/test filter), including native cleanup, session IPC and journal security.
Linux workspace doctests passed as well.

```text
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/home/oriel/.cache/zup-f19-target cargo clippy --workspace --all-targets --all-features -- -D warnings
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/home/oriel/.cache/zup-f19-target cargo nextest run -j 2 -p zup-session -p zup-preset-host -p zup-transaction -p zup-linux --all-features --profile fast -E 'package(zup-session) | package(zup-preset-host) | package(zup-transaction) | test(cleanup_lifecycle)'
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/home/oriel/.cache/zup-f19-target cargo test --workspace --all-features --doc
```

## Limits

Preset/controller coverage uses real child-process IPC and real journal storage,
with simulated native effects. Separate native tests exercise actual files,
readers, deletion and process crashes. These are not interactive GPUI click or
screenshot tests. No real reboot or sudden-power-loss experiment was performed.
Process-kill coverage does not establish power-loss durability. WSL passwordless
sudo was unavailable; no root-system service or Linux GUI proof is claimed.
