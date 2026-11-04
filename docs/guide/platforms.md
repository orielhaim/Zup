# Platforms and targets

A target names an operating system, an architecture, and a Linux C library ABI, with the operating system
first: `windows-x64`, `windows-arm64`, `windows-x86`, `linux-x64`, `linux-arm64`,
`linux-x64-musl`, `linux-arm64-musl`.

A target profile binds a project source directory to a target. When the profile
name is a target, no `target` field is needed:

```toml
[build.targets.windows-x64]
source = { directory = "dist/windows-x64" }

[build.targets.windows-arm64]
source = { directory = "dist/windows-arm64" }
```

A profile with any other name states its target explicitly:

```toml
[build.targets.production]
target = "windows-x64"
source = { directory = "dist/production" }
```

Declare one profile per payload/target pair. `--target` accepts profile names or targets:

```bash
zup build --target windows-x64
zup build --target linux-x64
```

With no `--target`, commands select every declared profile where the command allows it.

A bare operating system is not a target: `zup build --target windows` is
refused rather than guessing an architecture.

## Universal artifacts

A universal artifact is a distribution containing several compatible targets,
selected at launch time. It is declared separately from targets:

```toml
[build.artifacts.windows]
targets = ["windows-x64", "windows-arm64"]
```

```bash
zup build --artifact windows
```

Here `windows` names the artifact, not a target. If several Windows variants
are configured, one Windows installer is produced whose dispatcher selects the
matching architecture at runtime. A universal artifact supports only the
architectures it was built with.

## Windows

Targets: `windows-x64`, `windows-arm64`, `windows-x86`. Frontends: `gui`, `console`, `headless`. Universal and thin artifacts are Windows installers.

A Windows build embeds the package into the runtime template as PE resources — plain file I/O, no Windows API on the build host.

## Linux

| Capability | Status |
| --- | --- |
| Targets | `linux-x64`, `linux-arm64`, `linux-x64-musl`, `linux-arm64-musl` |
| Frontends | GNU: `gui`, `console`, `headless`. musl: `console`, `headless`. GUI needs a Wayland or X11 session. |
| Scope | `user`, `machine`, `either` (`either` needs an explicit `--scope`) |
| Artifact | one self-contained installer per target (no universal) |
| Signing | no platform signature; authenticity is the artifact digest and release identity |
| Machine payload | `/opt/<App>`, `/var/opt` for shared data, `/var/lib/zup` for state |
| Machine services | systemd system units, machine scope only |
| User services | not supported |
| Desktop launchers, menu, protocols, file associations | user scope; desktop launchers follow XDG user directories |
| Directory PATH entries | user and machine scope, through systemd `environment.d` |
| Prerequisites | native packages through APT, DNF/RPM, APK, or Pacman; detect-only or repository policy |
| Plugins | user and machine scope; generated resources follow the same capability and ownership rules as static resources |
| Authenticated updates | installed GUI, Console, and Headless applications |

A Linux build writes one extensionless installer per target (for example `Acme-Setup`).
Both architectures use the same lifecycle backend. musl runtimes are static;
the default GPUI preset is not available for musl, so musl GUI builds are refused. Native ARM64 CI runs the lifecycle suite and installers
composed on Windows. Cross-build templates with
`cargo xtask toolchain build --target linux-arm64 --profile release`; this needs
`cargo-zigbuild`, Zig, and the `aarch64-unknown-linux-gnu` Rust target.
Preset packages need a native binary for each GUI target.

## Linux machine scope

Machine installs run through a short-lived privileged worker: the installer plans without authority, requests administrator authorization through `pkexec` at execution time, and the worker re-verifies the package and executes inside `/opt`, `/var/opt`, `/var/lib/zup`, and the owned unit sources. User payload operations never elevate. Installing a shared prerequisite package can separately require polkit authorization. `zup build` never needs authority. Zup never handles an administrator password.

A machine project may declare `[[services]]` or plugin-generated services. Each becomes a systemd system unit owned at `/usr/local/lib/systemd/system/<unit>.service`. Installing, upgrading, repairing, or uninstalling never starts, stops, or restarts a service: Zup registers boot policy (`automatic` enables for boot, `manual` installs without enabling, `disabled` masks) and systemd decides when the process runs. The service binary must be a Zup-owned payload executable; anything else is refused. Failed service changes restore prior unit contents and boot policy; incomplete restoration reports recovery required.

`zup check` and `zup build` accept services on any host without systemd. The installing machine needs systemd 240+ only when services are present; `zup doctor` reports that readiness.

## Desktop shortcuts and directory PATH entries

A user `location = "desktop"` launcher uses `XDG_DESKTOP_DIR` from the XDG user-directory configuration. A desktop directory set to the home directory is disabled. Missing or disabled configuration produces an error; Zup does not guess a directory. A configured directory can be absent and is created through the file transaction. Symlink traversal and unrelated destination files are refused. Desktop entries are executable. Some desktops require the user to approve a launcher before running it; Zup does not forge desktop-environment trust metadata.

`[[path]]` adds directories to the environment of services launched by the systemd user manager. It does not expose a single executable as a command. User installations own one file under `$XDG_CONFIG_HOME/environment.d`; machine installations own one under `/usr/local/lib/environment.d`. Different applications and scopes have distinct filenames. Machine PATH directories must lie within the application install directory and have root-owned, non-writable ancestry. The generator must be installed. Systems without it fail clearly.

Start a new login session to propagate changes. Existing processes keep their inherited environment. SSH logins, shells launched outside the user manager, and desktop sessions that do not import its environment may need separate administrator configuration. Zup does not edit shell startup files. Existing PATH entries remain in order after the application's directories, including any inherited duplicates. Uninstall removes only the owned configuration file; it cannot change environments of running processes.

Cross-build musl runtime templates with `cargo xtask toolchain build --target linux-x64-musl --profile dev` or `--target linux-arm64-musl`. Install the corresponding Rust target, `cargo-zigbuild`, and Zig first. GNU and musl descriptors remain distinct, even for static binaries. A dynamic ELF with a GNU interpreter cannot satisfy a musl target, or vice versa. Static ELF does not prove which C library produced it; descriptors bind those bytes to the build target. Plugin AOT fingerprints include the full Rust target triple, so GNU AOT objects cannot be reused for musl.

Run `bash scripts/verify-linux.sh` for native GNU lifecycle validation. `sh scripts/verify-musl.sh` builds the native toolchain and exercises composition, package-manager bootstrap, install, repair, and uninstall inside a disposable Alpine environment. The Linux musl workflow runs it on native x64 and ARM64 hosts. It installs a test system package and is intended for disposable containers.

The PATH format follows [systemd environment.d](https://www.freedesktop.org/software/systemd/man/latest/environment.d.html), including its propagation limits. Desktop lookup follows [xdg-user-dirs](https://www.freedesktop.org/wiki/Software/xdg-user-dirs/); launcher syntax follows the [Desktop Entry Specification](https://specifications.freedesktop.org/desktop-entry/latest-single/). Zup uses the existing `directories` crate for XDG user-directory lookup and the existing desktop-entry renderer, rather than adding another parser. Native version checks delegate to [dpkg](https://manpages.debian.org/unstable/dpkg/dpkg.1.en.html), [RPM](https://rpm-software-management.github.io/rpm/man/rpm.8), [APK](https://gitlab.alpinelinux.org/alpine/apk-tools), and [Pacman](https://man.archlinux.org/man/core/pacman/pacman.8.en).
