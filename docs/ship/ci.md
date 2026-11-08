# Release CI

Zup can generate the GitHub Actions release workflow from project configuration.

```bash
zup ci github generate
```

The default output is:

```text
.github/workflows/release.yml
```

Commit that file. It is part of the release definition and should be reviewed.

## Check drift

```bash
zup ci github check
```

Use this in CI to fail when the committed workflow no longer matches the manifest/generator.

## GitHub Action

The repository also exposes `orielhaim/zup@action-v1`. A compact release step is:

```yaml
permissions:
  contents: write

steps:
  - uses: actions/checkout@v7
  - uses: orielhaim/zup@action-v1
    with:
      operation: release
```

The action supports separate `setup`, `build`, `compose`, `finalize`, `attest`, `publish`, and `release` operations when a pipeline needs explicit phases.

Prefer the generated workflow when you want Zup to keep the target matrix and release steps synchronized with `zup.toml`.

## Repository checks

Pull requests run formatting, Clippy, fast tests, and doctests on Windows and Linux. Linux also builds the pinned OpenRC controller and checks its required kernel interface. Native patch, WIT, and workflow edits trigger these checks. Each OS uses one job to reuse Rust build artifacts. Documentation-only changes do not run Rust CI. The Action workflow runs when its wrapper, CLI, or dependencies change.

**Full Verification** runs on master pushes, weekly, and on manual dispatch. It covers native service tests, staged-toolchain integration, and cross-built installers. Manual `sdk` and `release` inputs enable generated-project and release checks. ARM64 terminal tests run in standard CI. **Linux musl** and **Native service VMs** remain separate manual workflows and are not PR gates.

The VM workflow installs Rust 1.99, Zig 0.15.2, and cargo-zigbuild 0.24.0. Its self-hosted runner must provide Python 3, OpenSSH, and dedicated Alpine guests through `ZUP_SERVICE_VM_ROOT`. That directory contains `guest-key`, a prepopulated `known-hosts` file, and `alpine-x64/qmp.sock` and `alpine-arm64/qmp.sock`. Forward guest SSH to localhost ports 22342 and 22343 respectively. Use qcow2 for every writable device, including the ARM64 UEFI variable store, so internal QEMU snapshots can restore both disk and VM state. Enable the guest cgroups boot service with cgroup v2 mounted at `/sys/fs/cgroup`. Host keys must match the inventory before any guest mutation.

Each guest needs Python 3 and the controller built from `native/openrc/upstream.json` using `scripts/build-openrc-control.py`. Provision its binary at `/sbin/supervise-daemon` and retain its build receipt at `/opt/zup-controller-native/build.json`, or set `ZUP_OPENRC_BUILD_RECEIPT` to the guest receipt path. The controller suite compares that receipt with the current source lock and the installed binary's SHA-256 before running. Stale or missing native builds fail verification.

The controller suite requires an exclusively assigned disposable guest. It exercises real PID reuse through the guest's `ns_last_pid`, restores that value, and verifies that a reused foreign PID survives shutdown. The workflow then runs installer lifecycle, external-interference, and transaction crash suites serially for each architecture. Their logs and the controller build receipt are uploaded together.
