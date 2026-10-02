# Artifacts

Targets describe native variants. Artifacts describe files users download.

Keep those concepts separate.

## Default

With no `[build.artifacts]`, Zup builds one offline installer per selected target. This is the smallest release model.

## Declare an artifact

```toml
[build.artifacts.desktop]
kind = "universal"
mode = "offline"
targets = ["windows-x64", "windows-arm64"]
```

Fields:

| Field | Values | Meaning |
| --- | --- | --- |
| `kind` | `single`, `universal` | one target or target selection at launch |
| `mode` | `offline`, `thin` | carries content or acquires release content |
| `targets` | profile IDs | variants included by the artifact |
| `channel` | string | follow a release channel instead of a fixed version |
| `output` | path | explicit output name |

## Offline

An offline artifact carries the content it needs. Use it when installation must work without an origin being reachable.

## Thin

A thin artifact is a bootstrapper. It carries release trust information and acquires the matching release/runtime/content from the configured repository.

Thin artifacts require update/repository configuration. They also require a fixed install scope; `scope = "either"` is refused because acquisition starts before an installer UI can ask the user to choose a scope.

## Universal

A universal artifact can contain several target variants and select the matching one when launched. This is useful when one download should cover x64 and ARM64.

Do not make every release universal by default. One file per target is easier to inspect and distribute when a single download is not a product requirement.
