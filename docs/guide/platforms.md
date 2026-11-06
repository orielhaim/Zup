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

| Capability                                            | Status                                                                                                         |
| ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| Targets                                               | `linux-x64`, `linux-arm64`, `linux-x64-musl`, `linux-arm64-musl`                                               |
| Frontends                                             | GNU: `gui`, `console`, `headless`. musl: `console`, `headless`. GUI needs a Wayland or X11 session.            |
| Scope                                                 | `user`, `machine`, `either` (`either` needs an explicit `--scope`)                                             |
| Artifact                                              | one self-contained installer per target (no universal)                                                         |
| Signing                                               | no platform signature; authenticity is the artifact digest and release identity                                |
| Machine payload                                       | `/opt/<App>`, `/var/opt` for shared data, `/var/lib/zup` for state                                             |
| Machine services                                      | systemd or OpenRC, machine scope only                                                                          |
| User services                                         | not supported                                                                                                  |
| Desktop launchers, menu, protocols, file associations | user scope; desktop launchers follow XDG user directories                                                      |
| Directory PATH entries                                | user and machine scope, through systemd `environment.d`                                                        |
| Prerequisites                                         | native packages through APT, DNF/RPM, APK, or Pacman; detect-only or repository policy                         |
| Plugins                                               | user and machine scope; generated resources follow the same capability and ownership rules as static resources |
| Authenticated updates                                 | installed GUI, Console, and Headless applications                                                              |

Linux installers are extensionless executables. GNU GUI requires Wayland or X11 and a preset binary for the target. musl supports Console and Headless only.

Machine installs use a short-lived privileged worker. User payload operations do not elevate; `zup build` never needs administrator rights. Machine services use the active supported manager, systemd or OpenRC. User services are unsupported. See [services](/guide/services) for policies, identities, and recovery.

## Desktop shortcuts and directory PATH entries

A user `location = "desktop"` launcher uses `XDG_DESKTOP_DIR` from the XDG user-directory configuration. A desktop directory set to the home directory is disabled. Missing or disabled configuration produces an error; Zup does not guess a directory. A configured directory can be absent and is created through the file transaction. Symlink traversal and unrelated destination files are refused. Desktop entries are executable. Some desktops require the user to approve a launcher before running it; Zup does not forge desktop-environment trust metadata.

`[[path]]` adds directories to the environment of services launched by the systemd user manager. It does not expose a single executable as a command. User installations own one file under `$XDG_CONFIG_HOME/environment.d`; machine installations own one under `/usr/local/lib/environment.d`. Different applications and scopes have distinct filenames. Machine PATH directories must lie within the application install directory and have root-owned, non-writable ancestry. The generator must be installed. Systems without it fail clearly.

Start a new login session to propagate changes. Existing processes keep their inherited environment. SSH logins, shells launched outside the user manager, and desktop sessions that do not import its environment may need separate administrator configuration. Zup does not edit shell startup files. Existing PATH entries remain in order after the application's directories, including any inherited duplicates. Uninstall removes only the owned configuration file; it cannot change environments of running processes.

Cross-build musl runtime templates with `cargo xtask toolchain build --target linux-x64-musl --profile dev` or `--target linux-arm64-musl`. Install the corresponding Rust target, `cargo-zigbuild`, and Zig first. GNU and musl descriptors remain distinct, even for static binaries. A dynamic ELF with a GNU interpreter cannot satisfy a musl target, or vice versa. Static ELF does not prove which C library produced it; descriptors bind those bytes to the build target. Plugin AOT fingerprints include the full Rust target triple, so GNU AOT objects cannot be reused for musl.

See [system integration](/guide/integration) and [prerequisites](/guide/prerequisites) for configuration.
