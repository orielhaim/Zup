# Installer size measurements

These measurements describe PR #7. See [terminal installer measurements](/ship/terminal-size) for the subsequent shared Console frontend.

Measured on native GitHub runners with Rust 1.99 on 2026-10-28. Each application contains one 22-byte file, with no custom assets, prerequisites, or plugins. Runtime capabilities remain available. Sizes include the complete installer, not just its application payload.

The baseline is merged PR #6 using its release profile. The optimized build uses the `installer` profile.

| Platform | Frontend | Before | After | Reduction |
| --- | --- | ---: | ---: | ---: |
| Linux x64 | Headless | 15.62 MiB | 6.30 MiB | 59.6% |
| Linux x64 | Console | 15.64 MiB | 6.30 MiB | 59.7% |
| Linux x64 | GUI | 52.64 MiB | 15.74 MiB | 70.1% |
| Linux ARM64 | Headless | 14.45 MiB | 5.63 MiB | 61.0% |
| Linux ARM64 | Console | 14.45 MiB | 5.63 MiB | 61.0% |
| Linux ARM64 | GUI | 48.46 MiB | 14.48 MiB | 70.1% |
| Windows x64 | Headless | 20.32 MiB | 12.00 MiB | 41.0% |
| Windows x64 | Console | 20.74 MiB | 12.30 MiB | 40.7% |
| Windows x64 | GUI | 42.37 MiB | 17.45 MiB | 58.8% |
| Windows ARM64 | Headless | 17.21 MiB | 11.08 MiB | 35.6% |
| Windows ARM64 | Console | 17.57 MiB | 11.36 MiB | 35.3% |
| Windows ARM64 | GUI | 36.23 MiB | 16.36 MiB | 54.8% |

## GUI breakdown

The native preset remains a complete GPUI application. The last column includes its compressed executable, the package, and container alignment. Application assets contribute zero bytes in this fixture.

| Platform | Runtime | Preset before compression | Preset and package stored |
| --- | ---: | ---: | ---: |
| Linux x64 | 7.32 MiB | 25.49 MiB | 8.42 MiB |
| Linux ARM64 | 6.57 MiB | 23.26 MiB | 7.91 MiB |
| Windows x64 | 12.83 MiB | 14.60 MiB | 4.63 MiB |
| Windows ARM64 | 11.89 MiB | 13.31 MiB | 4.47 MiB |

The Linux Headless package occupies about 1.2 KiB plus its 121-byte footer. Windows Headless adds 1.5 KiB to its runtime for the payload, metadata, and PE resource alignment. Console adds effectively no overhead on Linux and about 0.3 MiB on Windows.

## Dispatchers and thin bootstrappers

These are the bare Windows x86 launcher components, measured separately from application runtimes. GUI and console launcher sizes differ by at most 512 bytes.

| Build host | Component | Before | After |
| --- | --- | ---: | ---: |
| Windows x64 | Offline dispatcher | 1.15 MiB | 1.08 MiB |
| Windows x64 | Thin bootstrapper | 4.12 MiB | 4.05 MiB |
| Windows ARM64 | Offline dispatcher | 1.14 MiB | 1.06 MiB |
| Windows ARM64 | Thin bootstrapper | 4.11 MiB | 4.05 MiB |

## Runtime and build costs

Local Linux trials used isolated directories and real installation operations. Headless peak RSS fell from about 40 MiB to 18 MiB for a tiny payload. Warm process startup with `--help` was around 3 ms. Install, component modification, upgrade, repair, and uninstall passed; repair preserved edited files unless explicitly forced, and uninstall preserved an unowned file. The compressed GUI loaded its SVG branding and compact layout and closed normally under Xvfb.

The size profile uses `s`, Thin LTO, and symbol stripping while retaining unwinding for plugin failure containment. A `z` trial saved about 0.28 MiB but was slower in paired 4 MiB lifecycle trials. Bootstrappers retain their separate `z`/Fat LTO/abort policy; they perform acquisition before transaction execution. These profiles are built once and reused by ordinary `zup build`. Developer source builds keep their normal profile, with only hashing and Zstd dependencies optimized.

Hashing the same 38 MiB binary took about 2.1 seconds in an ordinary debug build and 0.06 seconds with only `sha2` optimized. Large dev-template GUI composition fell from about 29 seconds to 7 seconds after the hot dependency overrides. Cold template generation still includes compilation and LTO; these measurements do not establish a portable cold-build speedup.

Windows Headless remains above the 8 MiB goal. Approximate link-map attribution identifies about 3.5 MiB of lifecycle/serialization code and 1.8 MiB of networking/cryptography code in its text section. WebAssembly accounts for about 0.5 MiB of named text, so removing it would not resolve the main overhead. Generic inlining and folded symbols make these attribution figures approximate.

Windows x86 runtime and preset sizes were not measured. The x86 dispatchers were measured; Full Verification exercises them through cross-built installers. GUI startup before/after and native display rendering on ARM64/Windows were not measured.

## Reproduce

```sh
cargo xtask toolchain build --profile installer
cargo build -p zup --bin zup
python scripts/ci/installer-size.py --cli target/debug/zup --toolchain target/installer/toolchain/0.0.1 --output target/sizes
```

On Windows, use `target/debug/zup.exe`. The Full Verification workflow has a size-only mode with revision, profile, and native-runner selection. Its artifacts contain the byte counts, inspection results, and Windows runtime link maps.

Sources: [baseline](https://github.com/orielhaim/Zup/actions/runs/40712661630), [optimized Linux measurements](https://github.com/orielhaim/Zup/actions/runs/40716845790), [final Windows x64](https://github.com/orielhaim/Zup/actions/runs/40726815465), [final Windows ARM64](https://github.com/orielhaim/Zup/actions/runs/40726820983).
