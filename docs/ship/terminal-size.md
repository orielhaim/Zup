# Terminal installer measurements

Console uses [cliclack](https://docs.rs/cliclack), with `console` for terminal capabilities and `indicatif` for progress. Its inline prompts already cover filtering, keyboard selection, and cancellation. Keeping the existing library avoids a second prompt implementation. [Dialoguer](https://github.com/console-rs/dialoguer) and [inquire](https://github.com/mikaelmello/inquire) offer similar prompts; neither offered a demonstrated size advantage that justified replacing it. [Ratatui](https://ratatui.rs/) would require a full-screen event and rendering loop that this installer does not need.

Plain numbered prompts provide an alternative for screen readers, narrow terminals, and `TERM=dumb`. Progress redraws only when events arrive, at most ten times a second. Headless has no `cliclack`, `console`, or `indicatif` dependency. Neither terminal frontend links GPUI. Dependency checks cover Windows and Linux, x64 and ARM64.

## Distribution size

The baseline is merged PR #7. Both revisions use Rust 1.99 and the existing `installer` profile. Each complete installer contains one 22-byte application file, no preset, and no custom assets. These figures include the native runtime, package metadata, compression, and executable container. Runtime capabilities, including plugins and recovery, remain available.

| Platform | Frontend | Before | After | Change |
| --- | --- | ---: | ---: | ---: |
| Linux x64 | Headless | 6.30 MiB | 7.17 MiB | +0.86 MiB |
| Linux x64 | Console | 6.30 MiB | 7.42 MiB | +1.11 MiB |
| Linux ARM64 | Headless | 5.63 MiB | 6.45 MiB | +0.81 MiB |
| Linux ARM64 | Console | 5.63 MiB | 6.63 MiB | +1.00 MiB |
| Windows x64 | Headless | 12.00 MiB | 12.14 MiB | +0.14 MiB |
| Windows x64 | Console | 12.30 MiB | 12.40 MiB | +0.10 MiB |
| Windows ARM64 | Headless | 11.08 MiB | 11.21 MiB | +0.13 MiB |
| Windows ARM64 | Console | 11.36 MiB | 11.46 MiB | +0.10 MiB |

The terminal UI's incremental cost is the Console minus Headless difference. It ranges from 0.19 to 0.26 MiB across these targets. The earlier Linux Console had no interactive installer flow; Windows already carried `cliclack`. Linux also gains working embedded plugin planning and durable generated-content replay. That correction accounts for most of its size increase.

These measurements use prebuilt distribution templates. Ordinary `check`, preview, development builds, and tests retain their existing profiles. No new LTO or linker settings were introduced.

## Runtime checks

On this Linux x64 workspace, seven warm `--help` trials gave median startup times of 2.28 ms for Headless and 2.24 ms for Console. The baseline trials were 2.46 ms and 2.30 ms respectively. These short timings vary with the host and do not establish a startup speedup. A Console prompt consumed zero scheduler CPU ticks during a three-second idle sample.

The optimized Linux binaries passed real install, modify, repair, upgrade, and uninstall operations. Repair preserved edited managed files unless forced; uninstall preserved an unowned file. A SIGINT during execution rolled back safely, and subsequent install/uninstall succeeded. Repeated attempts against a corrupt journal returned recovery-required code 7 without mutation in JSON and JSONL. Console also completed a real PTY installation, while Headless ran under a PTY without prompting. Both frontends executed an embedded SDK plugin, repaired its deleted generated file, and removed its files and a native launcher on uninstall. Killing a process after file mutation produced recovery-required with a state path and transaction ID on restart; repeated restart attempts preserved the installed files.

The permanent terminal tests compose real runtimes and exercise PTY interaction and redirected output on native Windows and Linux, including ARM64. Focused session, planning, generated-content retention, and process-interruption tests cover cancellation and recovery. Full Verification was not rerun. Interactive UAC/PolicyKit approval and legacy Windows consoles were not manually exercised.

Recovery-required currently stops with transaction details; the terminal does not offer a recovery wizard. Thin launchers retain their initial bootstrap scope prompt and acquisition event protocol. Acquired Windows plugin planning remains unsupported; embedded plugin planning is covered. Windows installed self-uninstall still hands off to a staged process, so automation needing the final exit status should run a downloaded installer.

## Reproduce

```sh
cargo xtask toolchain build --profile installer
cargo build -p zup --bin zup
python scripts/ci/installer-size.py --cli target/debug/zup --toolchain target/installer/toolchain/0.0.1 --output target/sizes
cargo test -p zup-installer --no-default-features --features console,headless --test terminal
```

Build Console and Headless templates independently to avoid Cargo feature unification. On Windows, use `zup.exe`. The size-only workflow builds each frontend separately on native runners; it does not run Full Verification.

Sources: [Linux x64](https://github.com/orielhaim/Zup/actions/runs/40763185588), [Linux ARM64](https://github.com/orielhaim/Zup/actions/runs/40763191321), [Windows x64](https://github.com/orielhaim/Zup/actions/runs/40759991894), [Windows ARM64](https://github.com/orielhaim/Zup/actions/runs/40759997807). Baseline byte counts are in the [PR #7 measurements](./installer-size).
