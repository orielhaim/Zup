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

Pull requests run formatting, Clippy, fast tests, and doctests on Windows and Linux. Rust, WIT, script, and workflow edits trigger the corresponding checks. Each OS uses one job to reuse Rust build artifacts. Documentation-only changes do not run Rust CI. The Action workflow runs when its wrapper, CLI, or dependencies change.

**Full Verification** runs on master pushes, weekly, and on manual dispatch. It covers native service tests, staged-toolchain integration, and cross-built installers. Manual `sdk` and `release` inputs enable generated-project and release checks. ARM64 terminal tests run in standard CI. **Linux musl** and **Native service VMs** remain separate manual workflows and are not PR gates.

The VM workflow installs Rust 1.100, Zig 0.15.2, and cargo-zigbuild 0.24.0. Its self-hosted runner must provide Python 3, OpenSSH, and dedicated Alpine guests through `ZUP_SERVICE_VM_ROOT`. That directory contains `guest-key`, a prepopulated `known-hosts` file, and `alpine-x64/qmp.sock` and `alpine-arm64/qmp.sock`. Forward guest SSH to localhost ports 22342 and 22343 respectively. Use qcow2 for every writable device, including the ARM64 UEFI variable store, so internal QEMU snapshots can restore both disk and VM state. Enable the guest cgroups boot service with cgroup v2 mounted at `/sys/fs/cgroup`. Host keys must match the inventory before any guest mutation.

Each guest needs Python 3, stock OpenRC, kernel pidfds, and cgroup v2 with `cgroup.kill`. Configure unified cgroups and enable the native cgroups boot service. No supervisor patch or build receipt is required.

The workflow runs installer lifecycle, external-interference, and process-crash suites serially for each architecture. Disposable x64 Alpine overlays can also run `scripts/verify-stock-openrc.py` against the musl library-test artifact, and `scripts/verify-linux-deployment.py` against a composed installer runtime. The latter covers generation and fixed payloads, persistent data, source-independent retained rollback, native service startup after a real reboot, and uninstall. These checks do not establish power-loss durability.
