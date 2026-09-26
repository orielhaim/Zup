# Secure updates

Updates use TUF metadata and static files. There is no zup update service.

## Configure and build

Add `[updates]` to `zup.toml`:

```toml
schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.4.0"
main = "Acme.exe"

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Acme"

[updates]
repository = "https://updates.example.com/acme"
channel = "stable"
root = "update-root.json"
```

`root` is read while building and its bytes are embedded in the installer package. The built installer and the committed maintenance `Setup.exe` retain the same repository, channel, and trusted root. At runtime, the configured repository URL is the directory containing `metadata/` and `targets/`.

## Channel descriptor

Stage a descriptor at `channels/<channel>.json` and the self-contained installer at its referenced target name:

```json
{
  "schema": 1,
  "app_id": "com.acme.desktop",
  "channel": "stable",
  "version": "1.4.0",
  "platform": "windows",
  "architecture": "x86_64",
  "target": "artifacts/1.4.0/windows-x86_64/Acme-Setup.exe"
}
```

The app id, channel, platform, architecture, and descriptor schema must match the installed package and running platform. The target must follow `artifacts/<version>/windows-<architecture>/<name>Setup.exe`. The version is SemVer and must be newer than the committed install version.

## Stage and publish with tuftool

Keep signing keys outside zup. `tuftool` is the standard repository creation and update tool from the `awslabs/tough` project. Place targets using their names beneath an input directory, then create or update the repository with that directory as `--add-targets`:

```powershell
$work = 'C:\release\acme-1.4.0'
$input = Join-Path $work 'input'
New-Item -ItemType Directory -Force (Join-Path $input 'channels') | Out-Null
New-Item -ItemType Directory -Force (Join-Path $input 'artifacts\1.4.0\windows-x86_64') | Out-Null
Copy-Item .\Acme-Setup.exe (Join-Path $input 'artifacts\1.4.0\windows-x86_64\Acme-Setup.exe')
$descriptorJson = @'
{"schema":1,"app_id":"com.acme.desktop","channel":"stable","version":"1.4.0","platform":"windows","architecture":"x86_64","target":"artifacts/1.4.0/windows-x86_64/Acme-Setup.exe"}
'@
Set-Content -Encoding utf8 (Join-Path $input 'channels\stable.json') $descriptorJson

tuftool create --root $trustedRoot --key $signingKey --add-targets $input `
  --targets-expires 'in 3 weeks' --targets-version 1 `
  --snapshot-expires 'in 3 weeks' --snapshot-version 1 `
  --timestamp-expires 'in 1 week' --timestamp-version 1 `
  --outdir (Join-Path $work 'repository')
```

Publish the contents of `repository/metadata/` and `repository/targets/` together to static HTTP storage. For subsequent releases, use `tuftool update` and monotonically increase targets, snapshot, and timestamp versions. Configure TUF role thresholds and keys in `root.json` for the deployment; keep root and targets signing offline and use a restricted online timestamp key for timestamp refreshes. Root rotation is published through the normal versioned TUF root chain. Ship the new root out-of-band in newly built installers; existing clients follow and verify repository root rotations from their embedded trusted root.

For smoke checks, `tuftool download --root <trusted-root> --metadata-url <repo>/metadata --targets-url <repo>/targets <output-dir>` exercises the same standard static layout. The update client also has local filesystem repository integration tests using `tough`'s repository editor and verifier.

## Client behavior

Run `Setup.exe update check` to inspect the signed channel descriptor, or
`Setup.exe update` to download and start a verified upgrade. Both accept
`--scope`, `--state-root`, `--output`, `--non-interactive`, and `--yes`; `--scope`
takes `user`, `machine`, or `either`, and `--output` takes `human`, `json`, or
`jsonl`. Safe TUF expiration checks are always enabled. Timestamp, snapshot,
and targets metadata are persisted at
`<update-state-root>/updates/<app-id>/<channel>/tuf/`; do not delete that
directory to recover from a verification failure. For machine installs, the
invoking user's `%LOCALAPPDATA%\zup` is the update state root so metadata and
the verified download are writable before the existing lifecycle requests
elevation for machine changes. Target bytes remain in a private `.partial`
quarantine file until the complete `tough` stream succeeds, then are atomically
renamed and passed to the existing upgrade lifecycle. The downloaded installer
publishes itself as maintenance only if its transaction commits.

A downgrade is refused: the lifecycle requires the new package version to be
greater than the installed version, an equal version is a modify, and a lower
version is an error. There is no manifest option to allow it.
