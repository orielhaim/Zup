# Prerequisites

A prerequisite is something the machine must have before the application can run.
Zup checks for it, and can install it from a package you ship or one already on
the network.

```toml
[[prerequisites]]
id = "vcruntime"
name = "Microsoft Visual C++ Runtime"
requirement = { kind = "installed_package", id = "{F3017226-FE2C-4285-8A0D-0A1A51023F78}" }
package = { type = "remote", url = "https://example.com/vcredist.exe", sha256 = "a1b2c3...", filename = "vcredist.exe" }
```

| Key | Required | Meaning |
| --- | --- | --- |
| `id` | yes | A stable identifier, e.g. `vcruntime` |
| `name` | yes | The label shown |
| `description` | no | One line |
| `requirement` | yes | How Zup decides whether it is satisfied |
| `package` | yes | Where the installer comes from |
| `installer` | no | How to run it |
| `component` | no | Only required when that component is selected |
| `when` | no | Only checked when a [condition](/configure/files#conditions) holds |
| `target` | `current` | Which architecture to check for |
| `targets` | empty | Only check for these target profiles |

Ids are compared case-insensitively, so `runtime` and `Runtime` collide. An id
must start with an alphanumeric character and continue with alphanumerics, `.`,
`_` or `-`.

## requirement

How Zup decides the prerequisite is satisfied. One of three kinds.

### A runtime

```toml
requirement = { kind = "runtime", id = "windows.webview2.evergreen", version = ">=0.90" }
```

`id` is a lowercase dotted path; each segment starts with a lowercase letter and
contains only `[a-z0-9_]`. `version` is an optional semver requirement.

### An installed package

```toml
requirement = { kind = "installed_package", id = "{F3017226-FE2C-4285-8A0D-0A1A51023F78}" }
requirement = { kind = "installed_package", id = "org.example.product", version = ">=2.0" }
```

The id is whatever the machine's package database uses - a Windows Installer
product code, or a vendor's own identifier. No whitespace, no path separators.

### A file version

```toml
requirement = { kind = "file_version", path = "C:\\Windows\\System32\\kernel32.dll" }
```

The path must be absolute and literal: no [path templates](/configure/install#path-templates)
are resolved here, because the file being checked for exists on the target
machine and not on the build machine.

::: warning These kinds were removed
`registry_value` and `msi_product` requirements, and a `kind` on `installer`
naming a package format such as `exe` or `msi`, were part of schema 1 and are
refused now. `installed_package` covers the MSI case. Nothing on the `installer`
table names a format: the provider owns the command line.
:::

## package

Where the installer comes from. One of two types.

Embedded in the artifact:

```toml
package = { type = "embedded", path = "redist/vcredist.exe", sha256 = "a1b2c3...", size = 14680000 }
```

All three fields are required. `path` is `/`-separated, relative, and may not
contain `..`. Zup hashes the file and checks the size at build time, so a
mismatch is caught by `zup check`.

Fetched from the network:

```toml
package = { type = "remote", url = "https://example.com/vcredist.exe", sha256 = "a1b2c3...", filename = "vcredist.exe" }
```

`url` must be HTTPS with no credentials and no fragment. `filename` must be a
safe Windows basename - which is why it is spelled out rather than inferred from
the URL. `size` is optional here and checked when present.

## installer

How to run the package. Every field has a default:

```toml
[prerequisites.installer]
arguments = ["/install", "/quiet"]
success_exit_codes = [0]
reboot_exit_codes = [1641, 3010]
privilege = "system"
```

| Key | Default | Meaning |
| --- | --- | --- |
| `arguments` | `[]` | Passed to the package. At most 128, and no templates |
| `success_exit_codes` | `[0]` | Exit codes that mean installed |
| `reboot_exit_codes` | `[1641, 3010]` | Exit codes that mean installed, pending a reboot |
| `privilege` | `system` | `user` needs no elevation; `system` does |

The two exit-code sets must both be non-empty and disjoint.

## target

Which architecture to check for:

| Value | Meaning |
| --- | --- |
| `current` | The architecture being installed. The default |
| `x86` \| `x64` \| `arm64` | One specific architecture |
| `any` | Any of them satisfies it |

## Limits

Zup refuses a prerequisite that exceeds these, and the diagnostic says which:

| Limit | Value |
| --- | --- |
| Package size | 2 GiB |
| Prerequisite id | 128 bytes |
| Argument count | 128 |
| Argument length | 4096 bytes |

Next: [installer UI](/customize/installer-ui).
