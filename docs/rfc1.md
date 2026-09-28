# RFC 0001 - zup

## A Programmable, Transactional, Cross-Platform Application Installer

**Status:** Draft
**Project:** `zup`
**Initial platform:** Windows
**Long-term platforms:** Windows, macOS, Linux
**Implementation language:** Rust
**Primary authoring format:** Declarative, language-agnostic manifest
**Default UI:** GPUI
**Extension model:** Planner-only WebAssembly components with a typed WIT world

> **Partially superseded.** This document is the original design record. Its
> reasoning still holds; its `zup.toml` surface does not. The manifest is now
> schema 1: a top-level `schema = 1` and a `[build.targets.<profile>]` map
> replaced `[build] target`, `[target]`, and `[source]`, and install locations
> are `${location.*}` semantic variables rather than `${known.*}` host paths.
> Sections 8, 10, 11, 12, 13, and 159 below are updated to the shipped syntax;
> smaller snippets are illustrative and use the same vocabulary. For the
> authoritative surface, see [architecture](architecture.md) and
> [schema/zup.schema.json](../schema/zup.schema.json). Windows remains the only
> implemented platform backend; the manifest accepts other targets and the
> build refuses them at an explicit boundary.

---

## 1. Abstract

`zup` is a programmable application installation framework intended to combine three properties that existing installer ecosystems rarely provide together:

1. **Deep customization comparable to NSIS**
2. **A modern, reliable installation engine**
3. **An excellent user experience by default**

The framework itself is implemented in Rust, but Rust is not part of the normal authoring experience. A C application, a Go application, a Java application, an Electron application, or a collection of binaries should all be able to use `zup` without requiring their developers to write Rust.

A typical installer should require little more than a declarative manifest:

```toml
schema = 1
frontend = "gui"

[app]
id = "com.acme.myapp"
name = "MyApp"
version = "1.4.0"
publisher = "Acme"
main = "myapp.exe"

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "./dist" }

[install]
scope = "either"

[install.directory]
user = "${location.user_data}/Programs/MyApp"
machine = "${location.programs}/MyApp"

[[launchers]]
location = "menu"
name = "MyApp"
target = "${location.programs}/MyApp/myapp.exe"
```

From this, `zup` should be able to produce a polished standalone installer with sensible installation, upgrade, repair, rollback, recovery, and uninstall semantics.

At the same time, `zup` must not become a constrained "app packager" that only works for the easy 80% of applications. Installer authors must be able to describe complex component graphs, services, registry state, file associations, prerequisites, arbitrary layouts, advanced conditions, privileged operations, and capability-scoped extensions when a corresponding WIT world is available.

The default experience should be simple.

The underlying system should not be.

The long-term objective is a fully cross-platform installer framework. The first implementation targets Windows only, because Windows combines the largest traditional installer surface with many of the system integration primitives that the architecture must eventually model: per-user and per-machine installation, services, registry state, application registration, launchers, locked executable files, UAC, reboot handling, and enterprise deployment.

The central design principle is:

> **Beautiful by default, programmable by design, conservative where reliability matters.**

---

# 2. Motivation

Application installers have developed along several largely separate paths.

Traditional systems such as NSIS provide enormous freedom. An NSIS installer is effectively a program. It can execute arbitrary logic, display arbitrary pages, invoke plugins, manipulate the operating system, and implement highly application-specific flows. That flexibility is one reason NSIS remains useful. Its model, however, requires learning an installer-specific scripting language and carrying significant responsibility for correctness. NSIS also exposes custom pages and plugin mechanisms rather than enforcing a narrow predefined installer flow.

WiX and Windows Installer approach the problem differently. MSI provides a mature machine-state and enterprise deployment model, while WiX Burn provides a bootstrapper engine in which a Bootstrapper Application controls the user experience and higher-level behavior. Custom Bootstrapper Applications can replace the standard wizard UI almost entirely.

Qt Installer Framework provides another powerful cross-platform model, including components, package repositories, custom pages and scripted operations. It is capable, but its authoring and UI architecture remain strongly rooted in traditional installer concepts such as wizard pages, components and Qt-specific scripting.

At the opposite end, projects such as Velopack optimize heavily for developer convenience. Velopack is Rust-based, cross-platform and language-agnostic, and deliberately provides a relatively constrained installer experience: on Windows, the normal setup flow is intentionally lightweight and minimally customizable, while MSI integration exists when conventional machine-wide deployment is required.

This leaves a useful space between these systems.

`zup` is not based on the premise that installers do not exist, or that existing engines are universally bad. The opportunity is narrower:

> There is room for an open, language-agnostic installer framework that preserves NSIS-like programmability while making reliability, transactional behavior, modern native UI, secure updates, and declarative authoring first-class concepts.

`zup` should not merely put a prettier window in front of old installer behavior.

The engine itself is the product.

---

# 3. Goals

`zup` has the following primary goals.

## 3.1 Language-agnostic authoring

An installer author should not need to know Rust.

The normal interaction with `zup` is:

```bash
zup build
```

against a project such as:

```text
project/
├── dist/
│   ├── myapp.exe
│   ├── helper.dll
│   └── assets/
├── zup.toml
└── installer-assets/
```

The application may have been produced by:

* C
* C++
* Rust
* Zig
* Go
* .NET
* Java
* Python
* Electron
* Tauri
* a game engine
* another build system
* or no conventional programming language at all

Rust is the implementation language of `zup`, not its required user language.

---

## 3.2 Excellent defaults

A developer should be able to provide:

* application name,
* application ID,
* version,
* payload,
* icon,

and receive an installer whose interface is already suitable for shipping.

The default installer should not present a seven-page wizard because the implementation happens to support seven decisions.

The default should optimize for the normal case:

```text
┌──────────────────────────────────────────┐
│                                          │
│                 [icon]                   │
│                                          │
│              Install Acme                │
│                                          │
│        128 MB · C:\Program Files\Acme    │
│                                          │
│             [ Install ]                  │
│                                          │
│               Advanced                   │
│                                          │
└──────────────────────────────────────────┘
```

Installation becomes a progress state in the same surface:

```text
┌──────────────────────────────────────────┐
│                                          │
│                 [icon]                   │
│                                          │
│             Installing Acme              │
│                                          │
│        ███████████████░░░  78%           │
│                                          │
│         Installing application…          │
│                                          │
└──────────────────────────────────────────┘
```

Completion becomes:

```text
┌──────────────────────────────────────────┐
│                                          │
│                    ✓                     │
│                                          │
│               Acme is ready              │
│                                          │
│              [ Open Acme ]               │
│                                          │
└──────────────────────────────────────────┘
```

Complexity should appear only when the application actually has complexity.

---

## 3.3 Deep customization

The default UI is not a restriction.

An installer author must eventually be able to control:

* layout,
* screens,
* transitions,
* branding,
* components,
* questions,
* validation,
* conditions,
* installation flow,
* custom operations,
* failure presentation,
* prerequisite UX,
* post-install choices,

without replacing the engine.

`zup` should provide approximately the same philosophical freedom that makes NSIS useful, while avoiding the requirement that every installer be written as an imperative installer script.

---

## 3.4 Reliable state transitions

Installation is fundamentally mutation of machine state.

`zup` must treat that as such.

The engine should understand:

* what exists before installation,
* what it intends to change,
* what it changed,
* whether the change succeeded,
* what state exists after a crash,
* which state belongs to `zup`,
* which state has subsequently been changed by somebody else,
* what can safely be reverted,
* what must be preserved.

An installer should not be conceptually equivalent to:

```text
copy some files
write some registry values
hope everything succeeds
```

---

## 3.5 Safe recovery

Power loss, process termination, user cancellation and OS failure are normal failure modes.

An installation transaction must be recoverable after interruption.

Where atomic OS primitives exist, `zup` should use them.

Where they do not exist, `zup` should use explicit journaling, durable receipts, and ownership-aware rollback.

---

## 3.6 First-class lifecycle management

Install, update, modify, repair and uninstall should not be separate technologies.

They are different plans produced by the same state engine.

A component installed by the original installer should retain enough provenance that future versions can reason about it.

---

## 3.7 Full cross-platform architecture

Windows is the v1 platform.

Windows must not become the architectural definition of an installer.

Concepts such as "registry value" must not leak into platform-neutral abstractions as if every operating system had a registry.

The core architecture must distinguish:

* universal installation semantics,
* shared high-level platform concepts,
* genuinely platform-specific operations.

---

# 4. Non-goals

## 4.1 Reimplementing every package manager

`zup` is not intended to replace:

* apt,
* dnf,
* Homebrew,
* winget,
* Chocolatey,
* Flatpak,
* the Microsoft Store,
* the Mac App Store,

or other ecosystem distribution systems.

It may later integrate with, emit metadata for, or package applications for some of these systems.

---

## 4.2 Pretending every OS is the same

Cross-platform does not mean forcing Linux, Windows and macOS into identical low-level primitives.

A Windows service, macOS launch daemon and systemd service may satisfy a similar application-level requirement, but their semantics differ.

`zup` should expose common intent where that intent is actually common and allow platform-specific configuration where it is not.

---

## 4.3 Inventing a second general-purpose programming language

A major problem with installer ecosystems is that developers often need to learn an installer-specific language.

`zup` should not solve that by inventing another one.

The manifest and UI formats should be declarative.

Conditions should use a deliberately small side-effect-free expression language.

Arbitrary computation is outside the declarative manifest and belongs behind a versioned, capability-scoped WIT world.

---

## 4.4 Replacing operating-system security mechanisms

`zup` should integrate with:

* Authenticode,
* UAC,
* Windows trust verification,
* macOS code signing,
* macOS notarization,

rather than attempting to create parallel substitutes.

---

# 5. Design Principles

## 5.1 Declarative first, imperative when necessary

Most applications need concepts such as:

```text
install these files
create this launcher
register this service
add this directory to PATH
```

These should be data.

For example:

```toml
[[launchers]]
location = "menu"
name = "Acme"
target = "${location.programs}/Acme/Acme.exe"
```

not:

```text
OpenLauncherManager()
ResolveStartMenu()
CreateShellLink()
...
```

Declarative state gives the engine information it cannot obtain from arbitrary code.

It enables:

* planning,
* validation,
* privilege prediction,
* disk-space calculation,
* conflict detection,
* rollback generation,
* dry-run inspection,
* better diagnostics,
* platform adaptation.

The declarative manifest provides no imperative execution escape hatch.

---

## 5.2 Headless engine, replaceable frontend

The installation engine must have no dependency on GPUI semantics.

Conceptually:

```text
                   zup manifest
                        │
                        ▼
                 ┌──────────────┐
                 │  Compiler /  │
                 │  Validator   │
                 └──────┬───────┘
                        ▼
                 Installer IR
                        │
                        ▼
                 ┌──────────────┐
                 │   Planner    │
                 └──────┬───────┘
                        ▼
                  Execution Plan
                        │
                        ▼
             ┌────────────────────┐
             │ Transaction Engine │
             └─────────┬──────────┘
                       │
                Platform Backend
                       │
                       ▼
                      OS

              ▲
              │ events / snapshots
              │ commands
              ▼

            Frontend
        ┌─────────────┐
        │ GPUI default│
        └─────────────┘
```

GPUI is the initial reference frontend.

It is not the engine API.

---

## 5.3 Platform backend isolation

The core should never call `RegSetValueExW`, `CreateServiceW`, or `IShellLinkW` directly.

The Windows backend owns those details.

The engine deals with higher-level actions.

---

## 5.4 Prefer boring system primitives

A modern installer does not need exotic storage tricks.

It needs careful composition of documented system behavior.

For Windows this means, among other things:

* documented Win32 APIs,
* Restart Manager,
* Service Control Manager,
* Known Folder APIs,
* COM shell integration,
* explicit registry views,
* Authenticode,
* staged file replacement,
* journaled recovery.

Microsoft explicitly discourages new applications from depending on Transactional NTFS and notes that TxF may not remain available in the future. `zup` must therefore not depend on TxF for transactional behavior.

---

## 5.5 Never confuse rollback with "run the opposite command"

Rollback requires knowledge of previous state and current ownership.

If installation changed:

```text
foo = "old"
```

to:

```text
foo = "zup-value"
```

uninstallation should not blindly delete `foo`.

It should reason:

```text
before installation: "old"
value written by zup: "zup-value"
current value: ?
```

If current state is still:

```text
"zup-value"
```

then restoration to `"old"` may be safe.

If current state is now:

```text
"something-else"
```

then another actor has changed it, and `zup` should normally leave it alone.

---

# 6. Existing Ecosystem and Positioning

There is no useful reason for `zup` to pretend it exists in a vacuum.

## 6.1 NSIS

NSIS demonstrates the value of:

* arbitrary installer logic,
* plugins,
* custom UI,
* callbacks,
* compact standalone executables.

Its weakness for `zup`'s goals is not lack of power.

It is that installer authors operate primarily through an installer-specific imperative scripting model.

`zup` should preserve the flexibility while moving the common case into typed declarative state.

---

## 6.2 WiX, MSI and Burn

Windows Installer remains important for:

* enterprise deployment,
* machine inventory,
* organization-wide tooling,
* conventional MSI workflows.

Burn demonstrates a useful separation between bootstrapper engine and custom Bootstrapper Application.

`zup` should not force MSI's internal model into its own core.

Future MSI interoperability or MSI-generation support should be an adapter/backend concern.

---

## 6.3 Qt Installer Framework

Qt IFW shows that:

* cross-platform component installation,
* online repositories,
* custom pages,
* scripted installation logic,

can coexist in one installer system.

`zup` differs mainly in authoring philosophy and its desired default UX.

---

## 6.4 Velopack

Velopack is especially relevant because it is already:

* Rust-based,
* cross-platform,
* language-agnostic,
* update-aware.

Its deliberately light-touch installer customization demonstrates that excellent developer ergonomics are possible when scope is constrained.

`zup` chooses a broader customization target.

The intended distinction is roughly:

```text
Velopack:
    simple application installation/update
    strong opinions
    minimal customization

zup:
    simple by default
    programmable when needed
    custom installer experiences
    generalized machine-state engine
```

This means `zup` carries substantially more architectural responsibility.

---

# 7. System Architecture

The project is divided conceptually into five independent layers.

```text
┌───────────────────────────────────────────────┐
│               Authoring Layer                 │
│                                               │
│ zup.toml · UI documents · assets · CLI        │
└─────────────────────┬─────────────────────────┘
                      │ compile
                      ▼
┌───────────────────────────────────────────────┐
│               Installer IR                    │
│                                               │
│ normalized app · actions · conditions · UI    │
└─────────────────────┬─────────────────────────┘
                      │ plan
                      ▼
┌───────────────────────────────────────────────┐
│           Transaction / Lifecycle Engine       │
│                                               │
│ plan · journal · execute · verify · recover   │
└───────────────┬───────────────────────────────┘
                │
        ┌───────┴────────┐
        ▼                ▼
┌───────────────┐  ┌──────────────────────────┐
│ Platform      │  │ Frontend Protocol        │
│ Backend       │  │                          │
│               │  │ GPUI / headless / future│
└──────┬────────┘  └──────────────────────────┘
       │
       ▼
 Operating system
```

A sixth concern, packaging, wraps the resulting runtime and payload:

```text
Installer runtime
      +
Installer IR
      +
UI assets
      +
Payload metadata
      +
Optional payload
      ↓
zup bundle / installer executable
```

These layers must remain separable.

---

# 8. Authoring Model

## 8.1 `zup.toml`

The primary project entry point is a versioned declarative manifest.

```toml
schema = 1
```

Schema versioning is explicit from the beginning. The parser refuses any
`schema` value other than the supported one, and there is no read path for the
previous shape: a version break means old manifests are rewritten, not
migrated.

---

## 8.2 Minimal example

```toml
schema = 1
frontend = "gui"

[app]
id = "com.acme.myapp"
name = "MyApp"
version = "1.4.0"
publisher = "Acme"
main = "myapp.exe"

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "either"

[install.directory]
user = "${location.user_data}/Programs/Acme/MyApp"
machine = "${location.programs}/Acme/MyApp"

[[launchers]]
location = "menu"
name = "MyApp"
target = "${location.programs}/Acme/MyApp/myapp.exe"
```

This should already be sufficient for a production-quality default installer.

---

# 9. Application Identity

A stable application ID is fundamental.

```toml
[app]
id = "com.acme.myapp"
name = "MyApp"
version = "1.4.0"
publisher = "Acme"
```

`app.id` identifies the logical product.

It should not depend on:

* executable filename,
* installation path,
* display name,
* version.

The ID is used for:

* installation discovery,
* persistent ledger paths,
* update metadata,
* component ownership,
* repair,
* uninstall,
* migration between versions.

A separate installation identifier may exist when multiple independent instances are permitted.

---

# 10. Installation Scope

Windows distinguishes meaningfully between per-user and per-machine installations. Windows Installer itself models these as separate installation contexts with different filesystem, registry and launcher behavior.

`zup` should model scope explicitly:

```toml
[install]
scope = "user"
```

or:

```toml
scope = "machine"
```

or:

```toml
scope = "either"
```

For `either`, the UI may expose the choice if both plans are valid.

Every scope the manifest can install to needs a directory template, so the
three alternatives above are each paired with the block below.

A semantic path may therefore depend on scope:

```toml
[install.directory]
user = "${location.user_data}/Programs/Acme/MyApp"
machine = "${location.programs}/Acme/MyApp"
```

The planner resolves the final path only once scope is known.

---

# 11. Variables

Manifest values may reference a controlled set of variables.

Examples:

```text
${app.id}
${app.name}
${app.version}
${location.programs}
${location.user_data}
${location.shared_data}
${location.menu}
${location.desktop}
```

A `${location.*}` variable names a semantic install location, not a host path.
The backend maps it for the selected scope, so the same manifest text is correct
on every machine: `programs` is the machine-wide program folder, `user_data`
and `shared_data` are the per-user and per-machine data folders, and `menu` and
`desktop` are the scope's own menu folder and desktop.

There is also an `install` variable, which expands to the install directory
template of the selected scope. It is useful where a resource must follow the
resolved install directory rather than a fixed location.

Variables are not general-purpose code.

Expansion is deterministic and validated.

Unknown variables are errors.

---

# 12. Conditions

Conditions must be expressive enough for application installation but deliberately not become a scripting language.

Example:

```toml
when = 'component("cli") && !component("debug")'
```

The only primitive query is component selection:

```text
component(...)
```

combined with `!`, `&&`, `||`, and parentheses. Target identity is not a
condition: applicability to a target is declared separately, with
`targets = ["<profile>"]` on the resource.

Conditions are:

* side-effect free,
* deterministic for a given machine snapshot,
* statically parseable,
* inspectable by `zup`,
* non-Turing-complete.

This matters because the planner must understand why an action exists.

---

# 13. Components

Components are first-class installation units.

```toml
[[components]]
id = "core"
name = "Application"
required = true
default = true

[[components]]
id = "cli"
name = "Command-line tools"
default = true

[[components]]
id = "service"
name = "Background service"
default = false
```

A component may describe:

* dependencies,
* supported targets,
* default state,
* required state.

Supported targets are named profiles, not architectures:

```toml
[[components]]
id = "cli"
name = "Command-line tools"
default = true
targets = ["windows-x64"]
```

Example:

```toml
[[components]]
id = "service"
name = "Background service"
default = false
requires = ["core"]
```

Actions can be conditional on selected components:

```toml
[[path]]
value = "${location.programs}/Acme/bin"
component = "cli"
when = 'component("cli")'
```

Component selection is therefore input to planning, not a UI-only concept.

---

# 14. Installer Intermediate Representation

The public manifest should not directly become the runtime engine API.

Compilation produces a normalized Installer IR.

The IR should contain explicit, validated representations of:

* application identity,
* version,
* target platform,
* target architecture,
* install scope possibilities,
* source artifacts,
* component graph,
* normalized actions,
* conditions,
* prerequisite definitions,
* update information,
* UI definition,
* localization resources,
* trust configuration.

This permits the authoring syntax to evolve without tightly coupling the runtime to TOML.

It also permits future frontends or build systems to generate Installer IR without generating TOML first.

The IR should have its own explicit format/schema version.

---

# 15. Action Model

The transaction engine operates on Actions.

An Action is not merely:

```rust
fn apply();
fn rollback();
```

That is insufficient for crash recovery.

Conceptually, an action supports a lifecycle resembling:

```text
inspect
   ↓
plan
   ↓
prepare
   ↓
record intent
   ↓
apply
   ↓
verify
   ↓
record receipt
```

and, when necessary:

```text
reconcile
rollback
uninstall
repair
```

---

## 15.1 Inspect

`inspect` obtains relevant current state.

For example, a registry action may observe:

```text
key exists: yes
value exists: yes
type: REG_SZ
value: "old"
```

A file action may observe:

```text
exists: yes
size: 4,819,102
hash: ...
```

Inspection must be side-effect free.

---

## 15.2 Plan

Planning compares current and desired state.

Example:

```text
desired file hash == existing file hash
```

may produce:

```text
NoOp
```

instead of rewriting the file.

Planning is responsible for producing an explicit intended mutation.

---

## 15.3 Prepare

Preparation can:

* stage files,
* verify payload availability,
* calculate disk requirements,
* resolve known paths,
* validate permissions,
* verify signatures,
* check prerequisite state.

Preparation must avoid irreversible system mutation where possible.

---

## 15.4 Durable intent

Before an external side effect occurs, the journal records what is about to happen.

This is crucial for the ambiguous crash case:

```text
journal says operation was about to run
process crashed
did the operation complete?
```

On recovery, `zup` can inspect system state and reconcile.

---

## 15.5 Apply

The mutation is performed.

---

## 15.6 Verify

Success should not be inferred purely from an API return code when observable state can be verified.

For example:

```text
Create launcher
↓
Verify launcher exists and resolves to expected target
```

or:

```text
Start service
↓
Verify SCM state
```

---

## 15.7 Receipt

After verification, `zup` records a durable receipt describing the resulting state and ownership information.

---

# 16. Action Metadata

Each action should carry metadata such as:

```text
stable action ID
action type
component
condition
dependencies
required privilege
resource locks
estimated work
progress weight
rollback capability
side-effect class
platform requirements
```

Example conceptual IR:

```text
Action {
    id: "install-main-files",
    kind: FileTreeInstall,
    requires: [],
    privilege: System,
    resources: [
        Path("C:\\Program Files\\Acme")
    ],
    rollback: GuaranteedUntilCommit
}
```

Stable action IDs are important across updates.

---

# 17. Built-in Action Families

The following are expected to be native concepts rather than opaque scripts.

## Filesystem

* create directory
* install file
* install tree
* move
* replace
* delete managed file
* create symlink where supported
* extract archive
* preserve file
* shared file ownership

## Network / payload

* download
* verify payload
* fetch prerequisite

## Windows system integration

* registry value/key
* launcher
* service
* environment variable
* PATH entry
* file type registration
* URI/protocol registration
* startup registration
* scheduled task
* Add/Remove Programs registration

## Processes

* run prerequisite
* stop known application
* restart application

## Lifecycle

* register installation
* register component
* persist update metadata
* create maintenance entry

Not every action is required on every platform.

---

# 18. Application-Specific Extensions

The current extension boundary is a planner-only WebAssembly component implementing `zup:plugin/planner@1.0.0` from the checked-in `wit/zup-plugin.wit`. A manifest binds a component by `id` and `source`, with optional `component` and `when` selection. The component receives typed planning context and returns typed installation resources or a typed `plugin-error`.

`zup build` resolves and hashes the source, validates zero imports and the exact planner export and signature, AOT-compiles the component, verifies the AOT output, and embeds the verified artifact. The installer runtime uses the Component Model only; source-manifest lifecycle mode does not JIT plugins. Generated files join the ordinary plan, transaction, ownership, repair, and uninstall paths.

The declarative manifest still has no executable custom actions. This milestone has no plugin-provided privileged action API; new capabilities require a separate, versioned WIT world. See [`docs/plugins.md`](plugins.md) for the authoring contract and example.

---

# 19. Planning

Planning is a first-class product capability.

Before changing the machine, `zup` should be able to know the intended installation plan.

Example:

```text
Install plan for Acme 1.4.0

Scope
  Machine

Destination
  C:\Program Files\Acme

Components
  Application
  Command-line tools

Changes
  + Create C:\Program Files\Acme
  + Install 43 files (86.3 MB)
  + Install service "acme-agent"
  + Add C:\Program Files\Acme\bin to system PATH
  + Create Start Menu launcher
  + Register acme:// protocol

Requires elevation
  Yes

Applications currently blocking update
  None

Disk required
  91.4 MB
```

This same plan drives:

* UI,
* diagnostics,
* privilege decisions,
* execution,
* silent mode,
* logging.

There should not be a separate hidden installer path.

---

# 20. Execution Graph

An installer is not necessarily a simple sequence.

A plan should form a dependency graph.

Example:

```text
           Download payload
                  │
                  ▼
            Verify payload
                  │
                  ▼
            Extract staging
                  │
        ┌─────────┴─────────┐
        ▼                   ▼
  Stop service      Resolve locked apps
        └─────────┬─────────┘
                  ▼
             Replace files
                  │
        ┌─────────┴─────────┐
        ▼                   ▼
 Write registry       Create launchers
        │                   │
        └─────────┬─────────┘
                  ▼
             Start service
                  │
                  ▼
                Commit
```

Independent preparation work may execute concurrently.

Global system mutations should be conservative about parallelism.

Correctness is more important than saving a few hundred milliseconds.

---

# 21. Resource Locking

Actions should declare resources they mutate.

Examples:

```text
filesystem path
registry path
service name
environment block
application registration
```

The planner can reject or serialize conflicting operations.

This also becomes useful when components or planner extensions produce operations independently.

---

# 22. Transaction Semantics

`zup` should not claim impossible ACID guarantees across arbitrary operating-system state.

There is no universal atomic transaction combining:

```text
filesystem
registry
SCM
launchers
network access
```

Instead, `zup` provides:

> **durable, journaled, recoverable transactions with precise rollback for engine-owned state and atomic primitives where the OS provides them.**

This wording matters.

---

# 23. Transaction State Machine

A transaction may move through states such as:

```text
Created
   ↓
Planned
   ↓
Prepared
   ↓
Applying
   ↓
Verifying
   ↓
Committing
   ↓
Committed
```

Failure may move into:

```text
RollbackPending
   ↓
RollingBack
   ↓
RolledBack
```

Recovery may move into:

```text
Recovering
```

and, in the exceptional case that `zup` cannot establish safe state:

```text
NeedsManualRecovery
```

The engine must prefer explicit uncertainty over pretending a rollback succeeded.

---

# 24. Write-Ahead Journal

The transaction journal must be durable.

Before executing an important side effect:

```text
Intent(Action #42)
```

is persisted.

After successful verification:

```text
Receipt(Action #42)
```

is persisted.

Consider a crash at:

```text
Intent
  ↓
CreateServiceW(...)
  ↓
CRASH
  ↓
Receipt
```

After restart, the journal does not know whether service creation completed.

The recovery path must call the action's reconciliation logic:

```text
Does the expected service now exist?
Does it match the expected binary/configuration?
```

The system can then mark the action as:

* completed,
* not completed,
* replaced/conflicted,
* ambiguous.

---

# 25. Filesystem Transactions

Filesystem changes should use staging.

Example:

```text
payload
   ↓
staging/
   ├── Acme.exe
   ├── foo.dll
   └── assets/
```

Files are:

1. extracted,
2. size/hash verified,
3. prepared,
4. then committed into the installation.

Where suitable, Windows' `ReplaceFile` API can replace an existing file while optionally maintaining a backup.

`zup` should not use TxF as its transaction model. Microsoft recommends alternatives for new development.

---

# 26. Cancellation

"Cancel" must not mean:

```text
kill whatever is running right now
```

Cancellation is a state transition.

The UI can request cancellation.

The engine reaches the next safe interruption point and either:

* stops before commit,
* rolls back,
* or explains that a critical atomic operation must complete first.

This prevents cancellation itself from becoming a corruption mechanism.

---

# 27. Installation Ledger

The transaction journal is temporary operational state.

The installation ledger is persistent lifecycle state.

These are separate concepts.

A ledger may be stored conceptually under:

```text
per-user:
%LOCALAPPDATA%\zup\installations\<app-id>\

per-machine:
%PROGRAMDATA%\zup\installations\<app-id>\
```

Exact layout is implementation-defined and versioned.

---

# 28. Ledger Contents

The ledger may include:

```text
application ID
installation ID
installed version
scope
installation path
selected components
normalized manifest/IR subset
action receipts
ownership records
previous-state snapshots where required
update trust root
update channel
maintenance metadata
transaction history
current journal pointer
```

It should not become an unbounded permanent log of every transient event.

Structured logs can be stored separately with retention policies.

---

# 29. Ownership-Aware Uninstall

Uninstall must reason about ownership.

## 29.1 Registry example

Installation observes:

```text
HK...\Foo
before: "old"
```

and writes:

```text
"acme"
```

The ledger stores both.

At uninstall:

### Current value is `"acme"`

The engine may restore:

```text
"old"
```

### Current value is `"someone-else"`

The engine leaves it untouched.

This is substantially safer than blindly executing the textual inverse of the original action.

---

## 29.2 PATH example

If `zup` adds:

```text
C:\Program Files\Acme\bin
```

it must later remove exactly that logical PATH entry.

It must not restore an old entire PATH string and thereby destroy unrelated changes made after installation.

---

## 29.3 Launcher example

If a launcher still points to the target and metadata created by `zup`, it can be removed.

If another tool has replaced the launcher, `zup` should avoid claiming ownership merely because the filename matches.

---

## 29.4 Files

Application payload files are generally owned strongly by the installation.

User-generated data is not.

The manifest must be able to distinguish policies such as:

```text
managed
preserve
shared
user-data
```

Uninstall must never infer that "everything under this directory" can safely be deleted solely because an application once installed there.

---

# 30. Repair

Repair is a natural consequence of desired-state installation.

For managed resources, `zup` can inspect:

```text
desired state
vs
current state
```

and build a repair plan.

For example:

```text
Acme.exe missing
foo.dll correct
launcher missing
service correct
```

produces:

```text
restore Acme.exe
recreate launcher
```

not a full blind reinstall.

---

# 31. Modify

Component changes should use the same planner.

Changing:

```text
CLI: off → on
Service: on → off
```

produces a plan containing only the relevant delta.

This means component state must remain available in the ledger.

---

# 32. Update

Updating from version A to version B is not a second installer engine.

The planner receives:

```text
current installation state
+
target release state
```

and generates the required delta.

The same journal, rollback, ownership and platform mechanisms remain in use.

---

# 33. Downgrades

Downgrades are denied. The lifecycle compares the package version against the
installed version, treats an equal version as a modify, and refuses a lower one
outright. There is no manifest option to permit a downgrade, and version
ordering plus trust metadata prevent accidental rollback to stale releases.

---

# 34. Payload Model

A `zup` release contains logical payload content independent from its outer installer executable.

Conceptually:

```text
Release
├── metadata
├── components
├── content manifest
├── UI assets
└── payload objects
```

A content entry includes information such as:

```text
logical path
size
SHA-256
component
platform
architecture
file metadata
```

Example:

```text
bin/Acme.exe
  size: 18,438,912
  sha256: ...
  component: core

bin/acme-cli.exe
  size: 5,902,336
  sha256: ...
  component: cli
```

---

# 35. Offline Installer

An offline installer contains everything required for installation.

Conceptually:

```text
┌───────────────────────┐
│ zup runtime           │
│ Installer IR          │
│ UI assets             │
│ Payload index         │
│ Compressed payload    │
└───────────────────────┘
```

The exact physical container is an internal format.

It must be versioned separately from the authoring manifest.

---

# 36. Web Installer

A web installer contains:

```text
runtime
manifest/IR
trusted release metadata
UI
```

but downloads payload on demand.

Benefits include:

* small initial executable,
* component-specific download,
* reduced repeated distribution,
* easier patching.

An application may offer both offline and web installers generated from the same source definition.

---

# 37. Hybrid Installer

A future or optional mode may embed core content while downloading optional components.

Example:

```text
core application: embedded
language packs: remote
large models: remote
developer tools: remote
```

This falls naturally out of the component/content model.

---

# 38. Compression

`zstd` is a strong default candidate for payload compression. The Rust `zstd` crate provides streaming compression/decompression bindings and a mature interface.

A simple initial internal organization could use component archives conceptually similar to:

```text
component.tar.zst
```

`tar` is attractive because it is streamable and simple.

However, privileged archive extraction is security-sensitive. The Rust `tar` crate includes path traversal protections, but its documentation explicitly limits what it can guarantee under concurrent mutation of the destination tree. `zup` should therefore apply its own hardened extraction policy rather than blindly treating generic `Archive::unpack` as the complete security boundary.

---

# 39. Hardened Extraction

Payload extraction should reject or carefully validate:

* absolute paths,
* `..` escapes,
* unexpected alternate data streams where relevant,
* unsafe reparse points,
* symlink escapes,
* device paths,
* destination changes during privileged extraction.

The preferred architecture is:

```text
untrusted/compressed payload
        ↓
private staging directory
        ↓
validate normalized entries
        ↓
extract
        ↓
hash verify
        ↓
commit managed files
```

Do not unpack an untrusted archive directly into `Program Files`.

---

# 40. Download Engine

Remote payload support should provide:

* HTTPS,
* proxy support,
* redirects under policy,
* resumable downloads,
* HTTP range requests,
* retries with backoff,
* mirror/CDN fallback,
* expected-size validation,
* streaming hash verification,
* cancellation,
* progress reporting,
* `.part` temporary files.

`reqwest` is the leading Rust candidate for this layer and currently provides async/blocking HTTP, TLS, proxy and redirect support.

Downloaded bytes are not trusted merely because TLS succeeded.

They must match trusted release metadata.

---

# 41. Content-Aware Updates

The first update optimization should be file/component awareness, not universal binary diffing.

Example:

```text
Release 1.4
├── Acme.exe       changed
├── foo.dll        unchanged
├── bar.dll        changed
└── assets.dat     unchanged
```

The updater downloads only:

```text
Acme.exe
bar.dll
```

or the payload objects containing them.

This is much easier to reason about than attempting to binary-diff everything.

---

# 42. Binary Delta Updates

For very large changed files, binary deltas can be worthwhile.

The Rust ecosystem contains a `bsdiff` implementation that can serve as a candidate.

Deltas should remain an optimization.

A valid release must remain installable without requiring an arbitrarily long historical chain of patches.

---

# 43. Update Metadata Security

Application update infrastructure is a long-lived security boundary.

A compromised CDN should not automatically become permission to distribute arbitrary binaries.

The Update Framework is designed to address classes of repository compromise, rollback and stale-metadata attacks.

The Rust `tough` crate implements substantial TUF functionality and is a strong candidate for `zup`'s update trust layer. Its currently documented limitations, including incomplete support for some advanced TUF features such as delegated roles, should be considered before making the format contract permanent.

Conceptually, a release repository can use:

```text
root
timestamp
snapshot
targets
```

with a trusted root initially embedded in the installer.

---

# 44. Simpler Signed Metadata

For installations that do not require a full TUF repository in the initial design, a signed metadata model can be simpler.

Rust has mature Minisign-compatible libraries such as `minisign` and the small verification-focused `minisign-verify`.

The architecture should permit the trust mechanism to be upgraded without changing installation semantics.

TUF remains the preferred long-term model for hosted automatic updates.

---

# 45. Hashing

SHA-256 should be available at external trust boundaries.

The RustCrypto `sha2` crate is an appropriate implementation candidate.

BLAKE3 is attractive for internal high-throughput content indexing, cache keys or deduplication.

These roles should not be confused.

A possible policy:

```text
signed release manifest:
    SHA-256

internal build cache/content IDs:
    BLAKE3
```

---

# 46. Signing

Signing has multiple layers.

## 46.1 Bundle integrity

`zup` verifies that payload objects match the signed release description.

## 46.2 Windows publisher identity

The final `.exe` should support standard Authenticode signing.

For Windows trust decisions, `zup` should use the operating system's trust APIs such as `WinVerifyTrust` where appropriate, rather than treating an Authenticode parsing library as equivalent to Windows trust policy.

## 46.3 Build-side signing integration

`zup` should not become a private-key manager.

Instead, signing providers can invoke or integrate with systems such as:

```text
signtool
Azure Trusted Signing
HSM-backed signing
SignPath
custom organization command
```

The artifact must be fully assembled before its final executable signature is applied, because the PE/Authenticode image hash covers the executable according to Windows Authenticode hashing rules.

---

# 47. Windows v1 Backend

Windows is the first concrete platform implementation.

The backend should prefer documented OS interfaces and avoid unnecessary compatibility shims.

---

# 48. Unicode and Paths

All Windows-facing code should use Unicode APIs.

Internally:

```text
Path / PathBuf
UTF-16 at the Win32 boundary
```

rather than assuming UTF-8 round-tripping through legacy ANSI APIs.

`zup` itself should declare itself long-path aware. Modern Windows can remove the traditional `MAX_PATH` limitation for many Win32 APIs when the application opts in appropriately.

---

# 49. Known Folders

Do not construct paths such as:

```text
C:\Program Files
C:\Users\foo\AppData
```

by string convention.

Windows provides the Known Folder API, including `SHGetKnownFolderPath`, and Microsoft recommends modern known-folder APIs rather than older CSIDL-based mechanisms for new applications.

This should back variables such as:

```text
${location.programs}
${location.user_data}
${location.shared_data}
${location.menu}
${location.desktop}
```

The manifest never names a host path; the backend maps each semantic location to
the right known folder for the selected scope.

---

# 50. Registry

Registry operations must model:

* hive,
* key path,
* value name,
* value type,
* value bytes/semantic value,
* previous state,
* desired state,
* view.

Windows exposes distinct 32-bit and 64-bit registry views on 64-bit systems. Explicit `KEY_WOW64_32KEY` / `KEY_WOW64_64KEY` semantics exist precisely because the view matters.

Therefore a registry action must not silently depend on the bitness of the installer process.

Registry state is an opaque backend payload, not a portable manifest resource.
The engine journals the bytes and hands them back to the adapter, so the
manifest never spells a key. A value that must point at the application is
written by the adapter from the lowered target path, not by a template.

---

# 51. Services

Windows service management should use Service Control Manager semantics.

The Windows API exposes service creation and management through SCM APIs such as `CreateService`.

A declarative service definition may look like:

```toml
[[services]]
id = "acme-agent"
name = "acme-agent"
display_name = "Acme Agent"
binary = "${location.programs}/Acme/acme-agent.exe"
start = "automatic"
component = "service"
```

The backend should model:

* service identity,
* binary path,
* arguments,
* startup behavior,
* dependencies,
* account,
* current state,
* whether `zup` created or merely modified it.

---

# 52. Launchers

Windows `.lnk` creation should use shell APIs rather than hand-authoring binary link files unless there is a strong reason otherwise.

`IShellLinkW` and related COM persistence interfaces are the normal native mechanism for shell links.

Example:

```toml
[[launchers]]
location = "menu"
name = "Acme"
target = "${location.programs}/Acme/Acme.exe"
working_directory = "${location.programs}/Acme"
```

---

# 53. Environment Variables and PATH

Environment-variable actions must be structural.

For PATH, `zup` should reason about individual entries rather than blindly replacing the full string.

After persistent Windows environment changes, the system convention is to broadcast a `WM_SETTINGCHANGE` notification using `"Environment"` so interested applications can refresh their environment view.

Example:

```toml
[[path]]
value = "${location.programs}/Acme/bin"
component = "cli"
when = 'component("cli")'
```

The entry belongs to the install scope that owns it, so uninstall removes
exactly this logical entry and nothing else.

---

# 54. File Associations

Applications can register that they support certain file types.

`zup` should distinguish:

```text
register capability
```

from:

```text
make application the user's default
```

Modern Windows default-app behavior is user-controlled. The installer should register proper application capabilities and COM class identifiers, and must not silently hijack user defaults.

---

# 55. URI Protocols

Protocol registration should similarly be modeled as an application capability.

Example:

```toml
[[protocols]]
scheme = "acme"
executable = "${location.programs}/Acme/Acme.exe"
args = ["--url", "%1"]
```

Ownership-aware uninstallation removes `zup`'s registration only when still applicable.

---

# 56. Add/Remove Programs Registration

A native `zup` installation should appear in the standard Windows installed-apps experience.

Windows uses uninstall registration metadata under the conventional Uninstall registry locations for desktop applications.

The registration should expose appropriate commands for:

* uninstall,
* modify,
* repair,

when those operations are supported.

Displayed metadata includes concepts such as:

```text
DisplayName
DisplayVersion
Publisher
InstallLocation
DisplayIcon
EstimatedSize
UninstallString
```

`zup` should generate these from canonical application metadata rather than requiring authors to duplicate them manually.

---

# 57. Locked Files

Locked application files are a central desktop-update problem.

Windows provides Restart Manager specifically to help installers determine which applications and services are using resources and to coordinate shutdown/restart, reducing unnecessary reboot requirements. Custom installers can use it directly.

`zup` should make Restart Manager a first-class Windows facility.

Example UX:

```text
Acme needs to update files currently in use.

The following application must close:
  Acme

[ Close and continue ]
[ I'll close it myself ]
```

After installation, eligible applications may be restarted.

---

# 58. Reboot Fallback

A reboot should be a fallback, not the normal answer to locked files.

Windows supports delayed rename/delete operations at reboot through `MoveFileEx` with `MOVEFILE_DELAY_UNTIL_REBOOT`.

`zup` can use such facilities when an operation genuinely cannot complete while Windows is running.

The transaction then ends in a state such as:

```text
CommittedPendingReboot
```

rather than falsely presenting the machine as already fully transitioned.

---

# 59. Scheduled Tasks

Some applications legitimately require scheduled tasks.

Windows Task Scheduler 2.0 exposes APIs suitable for programmatic registration and management.

This should be a platform action, not a special UI concept.

---

# 60. Elevation Architecture

The default UI process should not run as administrator.

Instead:

```text
┌─────────────────────────────┐
│ zup frontend/runtime        │
│ standard user token         │
└──────────────┬──────────────┘
               │
        authenticated IPC
               │
               ▼
┌─────────────────────────────┐
│ zup privileged worker       │
│ elevated token              │
└──────────────┬──────────────┘
               │
               ▼
     privileged OS actions
```

Windows supports the `runas` shell verb for requesting elevation through UAC.

The architecture should use elevation only when the chosen plan contains privileged actions.

---

# 61. Why Split Elevation

Keeping the UI unelevated provides several benefits:

* smaller privileged attack surface,
* no need to render the entire UI at high privilege,
* downloads and preflight can happen before elevation,
* UAC appears only when genuinely necessary,
* per-user installs can remain entirely unelevated,
* IPC permissions become explicit,
* privileged code can have fewer dependencies.

The worker should be deliberately boring.

---

# 62. Privileged IPC

The elevated worker must not expose a generic:

```text
execute arbitrary command as admin
```

interface.

The IPC connection should be bound to the initiating user and transaction.

Possible protections include:

* restrictive named-pipe ACL,
* initiating SID validation,
* random session nonce,
* immutable plan hash,
* protocol version,
* explicit capability list,
* transaction identifier.

The worker receives an already validated plan or constrained privileged operations.

The frontend cannot simply request arbitrary shell execution after elevation.

---

# 63. GPUI Frontend

GPUI is the initial default renderer.

Current GPUI architecture provides native platform implementations for Windows, macOS and Linux-family desktops, with Windows using native Win32/DirectWrite integration underneath the toolkit. GPUI remains pre-1.0 and explicitly warns that breaking changes are expected.

That makes it suitable as a frontend dependency, but dangerous as a core protocol dependency.

Therefore:

```text
zup engine
    │
frontend protocol
    │
zup-ui-gpui
```

not:

```text
zup engine == GPUI application
```

---

# 64. Frontend Protocol

The frontend receives immutable snapshots/events describing engine state.

Conceptually:

```text
Idle
Planning
Ready
Downloading
WaitingForElevation
WaitingForProcesses
Installing
Verifying
RollingBack
RebootRequired
Completed
Failed
```

A snapshot may contain:

```text
current phase
overall progress
current operation
download progress
selected scope
selected components
installation path
disk requirements
blocking processes
error information
reboot requirement
```

---

# 65. Frontend Commands

The UI sends constrained commands such as:

```text
StartInstall
Cancel
Retry
SetScope
SetInstallPath
SetComponent
CloseBlockingProcesses
Continue
LaunchApplication
OpenLog
```

It does not invoke OS APIs directly.

---

# 66. Default UI Philosophy

The default interface should avoid traditional installer ceremony.

Do not show pages merely because installers historically showed them.

Examples of information that should normally fit one initial surface:

```text
application
publisher
version
size
destination
primary action
```

Less common controls belong under progressive disclosure:

```text
Advanced
```

---

# 67. Advanced Panel

An example advanced view:

```text
Installation
────────────────────────────────────

Location
C:\Program Files\Acme                Change

Install for
● Everyone on this computer
○ Just me

Components
[x] Application                     114 MB
[x] Command-line tools                8 MB
[ ] Background service               12 MB

Options
[x] Start Menu launcher
[x] Add command-line tools to PATH
[ ] Desktop launcher
```

This is not a second "wizard mode".

It is detail attached to the installation plan.

---

# 68. Custom UI

Custom UI must not require Rust.

The v1 authoring model should support a declarative UI document describing a view tree.

It should provide primitives such as:

```text
column
row
stack
grid
text
image
icon
button
progress
checkbox
radio
select
text field
link
scroll view
separator
component selector
path selector
```

These elements bind to engine state.

Conceptually:

```text
column
  image app.icon
  text "Install ${app.name}"
  text "${plan.download_size} · ${plan.install_path}"

  if plan.has_options
    button "Advanced" -> open(options)

  button "Install" -> install
```

The exact serialization syntax should remain a format detail rather than becoming the runtime ABI.

---

# 69. UI Expressions

UI visibility and text bindings may use the same or closely related pure expression system used by the manifest.

Example:

```text
visible = 'scope == "machine"'
```

or:

```text
enabled = 'plan.ready && !engine.busy'
```

The expression engine cannot:

* write files,
* access arbitrary network resources,
* modify the registry,
* execute processes.

UI code is presentation and input logic.

System mutation belongs to the engine.

---

# 70. Full Layout Freedom

"Declarative" must not mean "choose from three templates."

The UI system should eventually permit:

* arbitrary view hierarchy,
* multiple screens,
* custom navigation,
* custom progress presentation,
* branded onboarding,
* component selection,
* prerequisite prompts,
* custom completion behavior.

An application should be able to build a highly unusual installer without replacing the transaction engine.

---

# 71. Default Theme

The default theme should support:

* light mode,
* dark mode,
* system preference,
* high contrast,
* native DPI scaling,
* keyboard navigation,
* RTL layout,
* localization,
* reduced motion,
* accessible labels/focus.

Visual customization should expose semantic tokens rather than forcing authors to restyle every element:

```text
accent
background
surface
text-primary
text-secondary
border
danger
success
radius
spacing
motion
```

---

# 72. Accessibility

Accessibility is a release requirement, not an optional polish item.

GPUI exposes accessibility integration paths through AccessKit, but Windows accessibility support in the broader Zed/GPUI ecosystem has had active gaps reported by users. `zup` therefore must not assume that choosing GPUI automatically solves desktop screen-reader accessibility.

Keeping the frontend protocol independent gives `zup` several options:

* contribute missing GPUI support,
* implement required platform integration,
* temporarily replace individual controls,
* or replace the frontend implementation without replacing the installer engine.

---

# 73. Localization

All default UI strings should come from structured localization resources.

Installer authors should be able to:

* use built-in translations,
* override individual strings,
* add application-specific strings,
* define locale fallback behavior.

UI layout should be resilient to text expansion.

Localization must not require rebuilding engine logic.

---

# 74. Silent and Headless Installation

The exact same plan and transaction engine should operate without a GUI.

Conceptually:

```bash
AcmeSetup.exe --silent
```

or:

```bash
zup-runtime install --headless
```

Silent mode must not be a separate hidden script path.

It consumes the same:

```text
manifest
IR
plan
platform backend
transaction engine
```

and differs only in interaction policy.

Ambiguous decisions must either have declared defaults or become headless errors.

---

# 75. Machine-Readable Output

Automation should be able to request structured diagnostics:

```bash
AcmeSetup.exe --plan --json
```

Example:

```json
{
  "scope": "machine",
  "requires_authorization": true,
  "install_bytes": 95839872,
  "download_bytes": 0,
  "blocking_processes": [],
  "actions": 51
}
```

This improves CI, enterprise tooling and installer testing.

---

# 76. Prerequisites

Applications frequently require external runtimes or drivers.

Prerequisites should be modeled explicitly.

Example conceptual manifest:

```toml
[[prerequisites]]
id = "provider.toolchain.v14"
name = "Acme Toolchain 14"
target = "x64"
requirement = { kind = "runtime", id = "provider.toolchain.v14", version = ">=14, <15" }
package = { type = "remote", url = "https://downloads.example.com/toolchain/x64/toolchain.exe", sha256 = "0000000000000000000000000000000000000000000000000000000000000000", filename = "toolchain.exe" }

[prerequisites.installer]
arguments = ["/quiet", "/norestart"]
success_exit_codes = [0]
reboot_exit_codes = [1641, 3010]
privilege = "system"
```

`requirement` is one of `runtime`, `installed_package`, or `file_version`, and
`package` is `embedded` or `remote`; each is an internally tagged value, so its
`kind` or `type` names the variant. The identifier shape is namespaced and the
platform backend owns what each identifier means. An `embedded` package names a
project-relative `path` with a `sha256` and `size`; a `remote` package names a
`url`, a `sha256`, and a `filename`.

A prerequisite model should understand:

```text
detection
version requirement
download
signature/hash verification
execution
success exit codes
reboot exit codes
```

Prerequisites are not just arbitrary scripts rendered as UI pages.

---

# 77. Custom Prerequisite Detection

Some detection mechanisms can be built in:

```text
file version
registry value
installed application
runtime version
OS feature
```

More complicated checks require an explicit built-in detection provider; a custom provider belongs to a separate capability-scoped WIT world.

---

# 78. Restart Manager UX

Blocking processes should be part of normal engine state.

The UI may receive:

```text
BlockingProcess {
    name: "Acme",
    pid: ...,
    restartable: true
}
```

The frontend can then produce application-appropriate UX rather than showing an ancient generic modal dialog.

---

# 79. Application Launch After Install

Launching the installed application is a frontend command whose actual execution is handled by the engine/platform layer.

This matters especially after elevated machine installation: the installed GUI application should normally launch in the user's standard context, not accidentally inherit an administrator token.

---

# 80. Maintenance Runtime

An installed application needs a stable lifecycle entry point for:

```text
modify
repair
update
uninstall
```

`zup` may install a small maintenance runtime or persist sufficient metadata for a bundled runtime to reconstruct these operations.

The maintenance mechanism must solve self-update/self-delete carefully.

A common technique is:

```text
copy maintenance executable to temporary location
launch temporary copy
replace/remove installation-owned runtime
exit temporary copy
cleanup
```

The exact mechanism is backend-specific.

---

# 81. Diagnostics

Every transaction should receive a unique identifier.

Structured events should use `tracing`-style semantics.

Example:

```text
transaction=019...
action=install_service
service=acme-agent
phase=apply
```

Logs should exist in both:

* human-readable form,
* machine-readable structured form.

The Rust `tracing` ecosystem is an appropriate candidate for structured instrumentation, with `tracing-subscriber` handling collection/formatting.

---

# 82. Log Security

Logs must avoid leaking:

* authorization headers,
* repository credentials,
* private keys,
* secret environment variables,
* sensitive command arguments.

Redaction belongs in the diagnostic layer rather than relying on every caller to remember it.

---

# 83. Determinism

Given:

```text
same zup version
same manifest
same input payload
same selected target
```

the build should be as deterministic as practical.

Sources of deliberate nondeterminism such as:

* Authenticode timestamps,
* signing service output,

should be separated conceptually from the deterministic payload/container build.

Deterministic manifests and content IDs simplify debugging and caching.

---

# 84. Build Pipeline

Conceptually:

```text
zup.toml
    │
    ▼
parse
    │
    ▼
schema validate
    │
    ▼
resolve source files
    │
    ▼
normalize paths
    │
    ▼
hash content
    │
    ▼
compile Installer IR
    │
    ▼
validate resource graph
    │
    ▼
build payload/component objects
    │
    ▼
compress
    │
    ▼
embed UI/assets/metadata
    │
    ▼
assemble final installer
    │
    ▼
sign
```

Signing is deliberately last.

---

# 85. Manifest Validation

Configuration typos must not be silently ignored.

If a developer writes:

```toml
publsher = "Acme"
```

instead of:

```toml
publisher = "Acme"
```

the build should fail or produce an unmistakable validation error.

Serde is the natural serialization foundation. `serde_ignored` can help identify fields that would otherwise be skipped during deserialization.

---

# 86. Editor Support

The manifest schema should be machine-describable.

A generated JSON Schema or equivalent representation can power:

* editor completion,
* diagnostics,
* hover documentation,
* validation outside `zup`.

`schemars` is a possible Rust-side building block for schema generation, though the public schema contract should not depend on the library itself.

---

# 87. Version Handling

Application versions should use a precise parser rather than ad-hoc string comparisons.

The `semver` crate is appropriate where the application chooses semantic-versioning semantics.

`zup` should distinguish:

```text
display version
release ordering version
```

if future application ecosystems require non-SemVer display strings.

---

# 88. Windows Architecture and Registry Bitness

The installer runtime architecture must not accidentally determine target semantics.

A 64-bit machine may install:

* x86 application,
* x86_64 application,
* ARM64 application,
* mixed components.

Registry view, Program Files location and payload architecture should therefore be explicit plan concepts.

---

# 89. Architecture Selection

A release may contain architecture variants, declared as a target matrix:

```toml
[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }

[build.targets.windows-arm64]
target = "aarch64-pc-windows-msvc"
source = { directory = "dist/windows-arm64" }
```

A release is selected by profile name or by canonical target triple, so a
variant is addressed by the identity of the machine it targets rather than by a
platform/architecture string pair.

A web installer can select the appropriate payload.

An offline installer may be platform-specific or multi-architecture.

---

# 90. Security Model

An installer often runs code at high privilege and is therefore a high-value attack surface.

`zup` should explicitly consider at least the following threats.

## Remote threats

* compromised CDN,
* substituted payload,
* replayed old release,
* stale metadata,
* malicious mirror.

## Local threats

* unprivileged process impersonating frontend IPC,
* symlink/reparse-point race,
* destination directory replacement,
* malicious pre-existing file,
* PATH manipulation,
* temporary-file attacks.

## Build/distribution threats

* wrong payload signed into release,
* corrupted archive,
* compromised update key,
* accidental downgrade.

## Extension threats

* buggy plugin planning,
* malicious plugin output,
* resource-limit or sandbox escape,
* AOT or bundle metadata mismatch.

---

# 91. Path Safety

Elevated operations should validate destinations as late as practical.

When necessary, path identity should be verified through handles rather than trusting only a previously resolved string.

Windows provides APIs such as `GetFinalPathNameByHandle` that allow software to inspect a path associated with an opened file handle.

The exact anti-reparse strategy belongs in the Windows backend security design.

---

# 92. Temporary Files

Temporary work directories should be created using secure filesystem semantics rather than predictable filenames.

The Rust `tempfile` crate is an appropriate candidate for ordinary temporary-file management.

Privileged staging requires additional permissions and ownership controls beyond merely generating a random path.

---

# 93. No Arbitrary Native Plugins

The installer does not load third-party DLLs or other native plugins into the privileged process. This removes an entire class of ABI instability, dependency collision, memory safety, and privilege-escalation surfaces. The manifest does not expose advanced behavior through helper executables.

---

# 94. Implemented Planner World

The current extension boundary is the checked-in `wit/zup-plugin.wit` world `plugin`. A component exports exactly `zup:plugin/planner@1.0.0` and its `plan` function. It must have zero imports and receives only explicit typed context: application identity, install directory and scope, the canonical target, and selected components.

The planner returns typed installation resources or a typed `plugin-error`. It does not inspect the environment, clock, filesystem, network, randomness, or other host state. The host remains responsible for resolving templates, validating collisions, applying elevation, and executing the resulting plan.

---

# 95. Build and Runtime Verification

`zup build --target <TRIPLE>` resolves and hashes each declared component, rejects core modules, validates the exact import-free planner interface and signature, AOT-compiles it with the pinned Wasmtime configuration, verifies the AOT artifact, and embeds it with target, WIT digest, engine fingerprint, size, and digest metadata.

The installer runtime loads only those verified AOT artifacts through the Component Model. Source-manifest lifecycle mode does not JIT plugins. Generated files are merged into the normal plan and then use the same install, ownership, repair, upgrade, and uninstall transaction path as manifest files.

---

# 96. Capability Expansion

The planner world is deliberately limited to planning. It has no plugin-provided privileged actions, custom action API, UI extension, or direct machine-mutation capability. Declarative resources use the host's existing ownership and elevation rules.

Any future filesystem, network, platform, UI, or other capability must be defined in a separate versioned WIT world with explicit capabilities and a defined lifecycle.

---

# 98. Language and Toolchain Boundary

Rust is one example language, not the plugin ABI. Other Component Model toolchains can target the existing WIT world as their language support matures.

---

# 99. Native Extensions

Native extensions may eventually be useful for highly specialized integration, but they are not the default model. Any future native integration should use an isolated helper process, explicit IPC, and a versioned protocol rather than dynamically loading arbitrary C ABI libraries into the core engine.

---

# 100. Rust Implementation Strategy

Rust provides strong building blocks for the engine, but `zup` should avoid unnecessary dependency layering over small OS APIs.

A useful distinction is:

```text
foundational dependency:
    substantial functionality worth depending on

thin wrapper:
    consider using Windows API directly
```

---

# 101. `windows` / `windows-sys`

**Role:** primary Windows API projection.

Microsoft's `windows` ecosystem exposes Windows APIs to Rust directly and should be the foundation of the Windows backend.

Use it for areas including:

* UAC,
* registry edge cases,
* SCM,
* Restart Manager if wrapping directly,
* Known Folders,
* COM,
* shell integration,
* Task Scheduler,
* trust APIs,
* process/token APIs.

This is the most important Windows dependency.

---

# 102. `winreg`

**Role:** ergonomic registry manipulation.

`winreg` is an established Rust registry wrapper and remains a reasonable candidate for common operations.

However, `zup` may prefer its own small abstraction over `windows-rs` where exact:

* registry view,
* security,
* transaction receipt,
* value typing,

semantics matter.

It should not depend on a wrapper merely to avoid a few Win32 calls.

---

# 103. `windows-service`

**Role:** candidate service-management helper.

The `windows-service` crate provides Rust abstractions for Windows service management and implementation.

For `zup`, direct SCM calls may still be preferable because the installer needs:

```text
inspect service configuration
create/update service
track ownership
delete safely
```

rather than a general service-runtime framework.

Microsoft also provides newer Windows service support in its Rust ecosystem.

---

# 104. `known-folders`

**Role:** safe Known Folder lookup.

The crate provides an ergonomic wrapper around Windows Known Folder APIs.

It is useful, although direct `SHGetKnownFolderPath` calls through `windows-rs` are simple enough that dependency minimization may win.

---

# 105. `lnks`

**Role:** Windows shell link helper.

`lnks` wraps native interfaces such as `IShellLinkW` and `IPersistFile`.

This is a candidate for launcher implementation.

Again, because the native API surface is relatively small, `zup` may instead own a narrow wrapper.

---

# 106. `restart_manager`

**Role:** safe Restart Manager bindings.

The `restart_manager` crate exposes Windows Restart Manager with a safe Rust API and is highly relevant to installation/update flows.

Because it is comparatively young, `zup` should audit it carefully before making it foundational.

The alternatives are:

* depend on it,
* vendor the needed logic,
* implement a narrow wrapper directly over Windows APIs.

The feature itself is important regardless of crate choice.

---

# 107. `atomic-write-file`

**Role:** atomic replacement of the transaction record.

Each transaction's `transaction.json` is a full snapshot written to a temp sibling, flushed, and renamed over the destination, so readers see either the old or the new complete record.

Transaction semantics — receipts, rollback, reconciliation, revisions, and locking — belong to `zup-transaction`; this crate only supplies the single-file atomic write.

---

# 108. `reqwest`

**Role:** HTTP download engine.

Recommended for:

* web installer payloads,
* update metadata,
* prerequisites,
* mirrors.

`zup` adds installer-specific policy around it:

```text
resume
retry
hash
trust
progress
temporary files
```

rather than exposing raw HTTP behavior.

---

# 109. `tokio`

**Role:** candidate async runtime.

Tokio remains the standard general-purpose asynchronous runtime in Rust and supports Windows asynchronous I/O.

It is useful for:

* concurrent downloads,
* frontend-engine IPC,
* asynchronous process interaction.

The core action model should not force every platform operation into async merely for stylistic consistency.

Many Win32 state mutations are naturally synchronous.

---

# 110. `zstd`

**Role:** default compression candidate.

Recommended for:

* payload archives,
* repository objects,
* optional metadata compression.

It combines strong ratios with fast decompression and streaming support.

---

# 111. `tar`

**Role:** simple streamable archive container candidate.

Useful particularly as:

```text
tar + zstd
```

Its built-in unpacking safety is useful but should not be treated as the complete privileged extraction security model.

---

# 112. `zip`

**Role:** compatibility archive support.

The Rust `zip` crate can support reading/writing ZIP payloads when projects already produce them.

ZIP does not need to be `zup`'s canonical internal format merely because it is common.

---

# 113. `sha2`

**Role:** trust-boundary SHA-256.

Recommended for:

* content manifest hashes,
* downloaded artifact validation,
* release metadata references.

---

# 114. `blake3`

**Role:** fast internal content identity.

Possible uses:

* local content cache,
* deduplication,
* build cache,
* internal object IDs.

It is complementary to, not necessarily a replacement for, SHA-256 in externally interoperable metadata.

---

# 115. `tough`

**Role:** TUF-backed secure update metadata.

Strong long-term candidate for:

* trusted root,
* targets metadata,
* repository freshness,
* rollback protection,
* signing-key rotation.

The exact supported TUF profile must be documented if adopted.

---

# 116. `minisign` / `minisign-verify`

**Role:** simpler signed metadata option.

Useful for configurations where full TUF infrastructure would be excessive, particularly early or offline distribution.

---

# 117. `authenticode`

**Role:** parsing/inspection utility candidate.

Rust crates exist for working with Authenticode structures.

For deciding whether Windows trusts a signed binary, `WinVerifyTrust` remains the correct platform-level authority.

---

# 118. `bsdiff`

**Role:** optional binary delta implementation.

Useful only where:

```text
changed file is large
delta is materially smaller
CPU/memory cost is worthwhile
```

It should not dictate the repository format.

---

# 119. `dirs`

**Role:** generic future cross-platform directory discovery.

`dirs` exposes standard platform paths across Windows, macOS and Linux-like systems.

For the Windows backend, direct Known Folder semantics are richer and more precise.

For platform-neutral utilities and future backends, it may still be useful.

---

# 120. `run_as`

**Role:** reference/helper for privilege elevation.

The crate provides cross-platform elevated execution helpers.

It is not sufficient as the architectural elevation model because `zup` requires:

* separate frontend/worker processes,
* authenticated IPC,
* immutable plan execution,
* privilege minimization.

Therefore it may be useful internally or as a reference, but should not define elevation semantics.

---

# 121. `tempfile`

**Role:** secure temporary files/directories.

Useful for:

* downloads,
* transient build files,
* temporary maintenance runtime,
* non-privileged staging.

---

# 122. `walkdir`

**Role:** build-time payload traversal.

Useful for scanning:

```text
dist/**
```

while constructing content manifests.

Filesystem traversal during privileged runtime mutation should remain under stronger platform-specific controls.

---

# 123. `memmap2`

**Role:** optional large-container optimization.

Memory mapping can be useful later for:

* large bundle indexes,
* random-access payload tables,
* efficient local repository access.

It is not required for the core architecture.

---

# 124. `serde`

**Role:** serialization foundation.

Recommended for:

* manifest parsing,
* IR,
* journal records,
* ledger structures,
* frontend protocol messages.

Persistent formats still require explicit schema/version discipline rather than blindly serializing internal Rust structs forever.

---

# 125. `toml`

**Role:** primary authoring manifest parser.

TOML is a strong fit because the normal installer definition consists mostly of:

```text
metadata
tables
lists
structured configuration
```

rather than deeply nested UI trees. The Rust `toml` ecosystem integrates naturally with Serde.

---

# 126. `semver`

**Role:** semantic release ordering and constraints.

Useful for:

```text
version comparison
minimum prerequisite version
upgrade rules
update metadata
```

where SemVer semantics are selected.

---

# 127. `uuid`

**Role:** identifiers.

Possible uses:

* transaction IDs,
* ephemeral installer sessions,
* installation-instance IDs.

Stable application identity remains author-defined rather than an automatically regenerated UUID.

---

# 128. `clap`

**Role:** `zup` CLI.

Appropriate for commands and arguments such as:

```text
zup build
zup validate
zup inspect
zup plan
```

and runtime maintenance tooling.

---

# 129. `thiserror`

**Role:** typed library errors.

The core/public Rust libraries should expose structured error types rather than opaque strings. `thiserror` is a natural implementation aid.

---

# 130. `anyhow`

**Role:** top-level application/build CLI error context.

Useful inside executable boundaries where rich diagnostic context is more important than a stable public error enum.

It should not become the public error contract of `zup-core`.

---

# 131. `embed-resource`

**Role:** PE resource integration.

Useful for build-time embedding of:

* application icon,
* Windows manifest,
* version information,
* static native resources.

It should not be abused as the general payload container format.

---

# 132. GPUI

**Role:** default installer frontend.

Strengths:

* Rust-native,
* GPU-accelerated,
* custom rendering,
* native desktop targets,
* strong control over appearance.

Risk:

* pre-1.0 API churn,
* accessibility maturity on Windows.

This is precisely why GPUI should sit behind `zup`'s frontend protocol.

---

# 133. Dependency Philosophy

The core dependency policy should be conservative.

A dependency is attractive when it provides:

* significant complexity reduction,
* good maintenance,
* auditable implementation,
* clear scope,
* compatible security posture.

A 200-line wrapper around a Win32 API is not automatically better than owning those 200 lines.

Critical areas that deserve especially strong scrutiny:

```text
transactions
elevation
signature verification
archive extraction
update trust
privileged IPC
```

---

# 134. Proposed Workspace Boundaries

A possible logical workspace structure:

```text
zup/
├── zup-core
├── zup-manifest
├── zup-bundle
├── zup-platform
├── zup-windows
├── zup-update
├── zup-ui
├── zup-ui-gpui
└── zup-cli
```

These names are architectural illustrations rather than permanent public crate commitments.

---

# 135. `zup-core`

Owns concepts such as:

```text
Installer IR
actions
planner
execution graph
transaction state
receipts
ledger model
engine state
frontend commands/events
```

It should contain minimal platform-specific code.

---

# 136. `zup-manifest`

Owns:

```text
zup.toml schema
parsing
validation
condition syntax
variable syntax
schema migration
editor schema generation
```

Authoring syntax should not infect lower layers.

---

# 137. `zup-bundle`

Owns:

```text
content manifest
bundle index
compression
payload object lookup
offline container
web payload descriptors
integrity verification
```

---

# 138. `zup-platform`

Owns backend contracts and shared platform-level semantic types.

It should not define Windows-specific primitives as universal types.

---

# 139. `zup-windows`

Owns:

```text
Known Folders
registry
SCM
launchers
Restart Manager
UAC/elevation worker
Task Scheduler
environment changes
ARP registration
Windows application registration
AuthentiCode/WinTrust integration
Windows path security
reboot scheduling
```

---

# 140. `zup-update`

Owns:

```text
release metadata
repository client
trust roots
channels
content resolution
delta selection
download planning
```

It uses the same core execution engine for applying an update.

---

# 141. `zup-ui`

Owns the frontend protocol and declarative UI model.

It should be render-engine independent.

---

# 142. `zup-ui-gpui`

Owns:

```text
default theme
controls
animations
layout
native window
accessibility bridge
frontend implementation
```

A future frontend can replace this crate without changing `zup-core`.

---

# 143. `zup-cli`

Owns developer-facing build and inspection workflows.

Examples include:

```text
zup build
zup validate
zup inspect
zup plan
```

The CLI is not the only possible future authoring frontend.

---

# 144. Platform-Neutral Action Semantics

Cross-platform actions should express common intent only where meaningful.

For example:

```text
InstallFiles
InstallService
RegisterProtocol
CreateApplicationLauncher
SetEnvironmentEntry
```

A backend may translate:

```text
InstallService
```

to:

```text
Windows → SCM service
macOS   → launchd daemon/agent
Linux   → systemd service
```

only when the author intentionally requests portable service semantics.

---

# 145. Platform-Specific Actions

Some concepts should remain explicitly namespaced.

Examples:

```text
windows.registry
windows.task
windows.com_registration

macos.launch_service
macos.pkg_script

linux.desktop_entry
linux.systemd
```

`zup` should not invent fake equivalents merely to make a manifest look symmetrical.

---

# 146. Capability Discovery

The planner should be able to ask a platform backend:

```text
Does this backend support action X?
Under what constraints?
```

Unsupported combinations should fail during build or planning rather than at the middle of installation.

---

# 147. Future macOS Backend

The long-term macOS implementation should respect native macOS application distribution rather than pretending it is Windows with different path separators.

Apple's normal outside-the-App-Store distribution security model relies on Developer ID signing and notarization. `.pkg` installers remain appropriate where software must install multiple components or content outside a simple application bundle.

Potential `zup` integration therefore includes:

```text
.app bundle installation
Developer ID signing integration
notarization/stapling workflow
.pkg generation/backend
LaunchAgents
LaunchDaemons
protocol handlers
application support directories
```

---

# 148. macOS Services

Persistent agents/services should integrate with `launchd` and its normal LaunchAgent/LaunchDaemon locations and semantics.

The core service abstraction should be broad enough to represent common intent without hiding macOS-specific configuration when required.

---

# 149. Future Linux Backend

Linux is not one single packaging environment.

A complete strategy may include:

```text
portable zup-native installation
.desktop integration
systemd integration
XDG directories
package-format adapters
Flatpak integration
AppImage-oriented distribution
```

Freedesktop specifications define standard desktop-entry behavior and shared desktop conventions.

---

# 150. Linux Native Distribution

`zup` should not assume that its own executable installer is always the best Linux artifact.

In many environments the correct output may eventually be:

```text
.deb
.rpm
Flatpak
AppImage
```

or metadata feeding those tools.

Flatpak provides cross-distribution application deployment with its own runtime/sandbox model, while AppImage provides a portable single-file application distribution approach.

The authoring model can remain useful even when the output backend differs.

---

# 151. Cross-Platform Output Model

Long term, the same application definition may support:

```text
zup build --target windows-x86_64
zup build --target macos-aarch64
zup build --target linux-x86_64
```

but those targets need not all produce the same physical artifact type.

For example:

```text
Windows
  Setup.exe

macOS
  .dmg / .pkg / app distribution artifact

Linux
  portable installer / package adapter
```

A shared manifest describes application intent.

The backend chooses appropriate native realization.

---

# 152. MSI and Enterprise Interoperability

`zup` should not position native `.exe` installation as the only possible Windows future.

Enterprise environments commonly expect MSI-compatible deployment workflows.

Possible future models include:

```text
MSI wrapper around zup runtime
MSI generation backend
enterprise bootstrapper
deployment metadata export
```

The important architectural constraint is:

> MSI-specific constraints must not become the internal definition of all `zup` actions.

The native engine remains free to provide capabilities beyond MSI.

---

# 153. Stable Silent Behavior

Enterprise and automated installation need predictable behavior.

The runtime should define:

* stable command-line arguments,
* stable machine-readable output,
* documented exit-status semantics,
* deterministic defaults,
* no surprise interactive dialogs in silent mode.

Where Windows deployment tools expect conventions such as reboot-required status, adapters can map `zup`'s richer internal result state to those conventions.

---

# 154. Reboots as State, Not an Error Code Alone

Internally:

```text
Success
SuccessRebootRequired
Cancelled
Failed
RecoveryRequired
```

is more useful than overloading all semantics into one platform-specific integer.

CLI/backend adapters can translate this state to suitable process exit codes.

---

# 155. UI and Engine Version Compatibility

The frontend protocol must be versioned.

A packaged installer contains compatible versions of:

```text
engine
frontend
Installer IR
bundle format
```

An installed maintenance runtime must reject state formats it cannot safely understand.

Migration must always be explicit.

---

# 156. Persistent Format Versioning

Every persistent format should carry a version:

```text
manifest schema
Installer IR
bundle format
transaction journal
installation ledger
update repository metadata
frontend protocol
plugin WIT/API
```

The implementation must not rely on:

```text
"Serde can deserialize this Rust struct, therefore it is a stable format."
```

Rust type layout and public persistence contracts are separate concerns.

---

# 157. Forward Compatibility

Unknown required features should fail closed.

For example, if a runtime encounters:

```text
required_feature = "registry-transaction-v3"
```

that it does not understand, it must not attempt a partial installation.

Optional metadata may be safely ignored when explicitly marked as optional.

---

# 158. Install-Time Engine Compatibility

Release metadata may state:

```text
minimum_zup_runtime
```

This prevents a very old maintenance runtime from applying a release whose action semantics it does not understand.

An update may first replace/update the maintenance runtime and then apply the application transaction.

---

# 159. Example Full Manifest

The following is a complete schema-1 manifest:

```toml
schema = 1

[app]
id = "com.acme.acme"
name = "Acme"
version = "1.4.0"
publisher = "Acme Inc."
main = "Acme.exe"

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "either"
allow_directory_override = true

[install.directory]
user = "${location.user_data}/Programs/Acme"
machine = "${location.programs}/Acme"

[[components]]
id = "core"
name = "Acme"
description = "The Acme desktop application."
required = true
default = true

[[components]]
id = "cli"
name = "Command-line tools"
description = "Adds the acme command."
default = true

[[components]]
id = "service"
name = "Background service"
default = false
requires = ["core"]

[[files]]
source = "**/*"
destination = "${location.programs}/Acme"
component = "core"

[[launchers]]
location = "menu"
name = "Acme"
target = "${location.programs}/Acme/Acme.exe"
component = "core"

[[path]]
value = "${location.programs}/Acme/bin"
component = "cli"
when = 'component("cli")'

[[services]]
id = "acme-agent"
name = "acme-agent"
display_name = "Acme Agent"
binary = "${location.programs}/Acme/acme-agent.exe"
start = "automatic"
component = "service"

[[protocols]]
scheme = "acme"
executable = "${location.programs}/Acme/Acme.exe"
args = ["--url", "%1"]

[[file_associations]]
extension = ".acme"
id = "Acme.Document"
description = "Acme Document"
executable = "${location.programs}/Acme/Acme.exe"

[updates]
channel = "stable"
repository = "https://updates.example.com/acme"
root = "update-root.json"

[ui]
accent = "#7c5cff"
theme = "system"
logo = "${location.programs}/Acme/Acme.exe"
```

No Rust is required.

No installer-specific programming language is required.

Yet the engine retains enough structure to understand most mutations.

---

# 160. Example Custom UI Relationship

A custom layout is not part of the manifest. The `[ui]` table carries branding
only, and the frontend supplies its own layout.

Conceptually, that document binds to values such as:

```text
app.name
app.icon
app.version

plan.install_path
plan.install_size
plan.download_size
plan.requires_authorization

engine.phase
engine.progress
engine.current_operation
engine.error

selection.scope
selection.components
```

and emits commands such as:

```text
install
cancel
retry
set_scope
toggle_component
choose_directory
close_blocking_apps
launch
```

It never receives unrestricted engine internals.

---

# 161. Default Experience vs Custom Experience

These are two views over the same engine.

```text
                   zup engine
                       │
          ┌────────────┴────────────┐
          ▼                         ▼
      default UI                custom UI
   zero configuration        arbitrary layout
          │                         │
          └────────────┬────────────┘
                       ▼
               identical plan
               identical journal
               identical backend
```

Customization must not require forfeiting reliability.

---

# 162. No Special "Wizard" Primitive

`zup` should not architect its UI around:

```text
WelcomePage
LicensePage
DirectoryPage
ComponentsPage
InstallPage
FinishPage
```

A developer may construct that flow.

It is not the universal installer model.

The frontend system deals with views and engine state, not sacred historical installer pages.

---

# 163. License Presentation

Some applications need license acceptance.

That should be an optional policy/UI element:

```text
agreement required before StartInstall becomes valid
```

not an unconditional second screen built into the installer architecture.

---

# 164. Prerequisite and Failure UX

Errors should expose structured causes.

Instead of engine-to-UI text:

```text
"Error 5"
```

the frontend may receive:

```text
Error {
    kind: PermissionDenied,
    action: "install-service",
    recoverability: RetryAfterElevation,
    technical_detail: ...
}
```

The default frontend can render a useful explanation.

A custom frontend can render something application-specific.

---

# 165. Recovery UX

If a previous installation was interrupted, startup should inspect the journal before offering a new installation.

Possible states:

```text
Previous installation can be safely resumed.
Previous installation will be rolled back.
Installation is already complete; cleanup remains.
Manual recovery is required.
```

The user should not be asked to understand journal internals.

---

# 166. Update UX

Automatic updates should use the same UI philosophy:

```text
Update available
Acme 1.5.0 · 18 MB

[ Update ]
```

or, when configured by the application:

```text
download in background
prepare staging
ask only when applications must close
```

The update engine remains policy-neutral.

The application decides whether its UX is:

* explicit,
* background,
* on-exit,
* on-startup.

---

# 167. Update Channels

Release metadata should support channels such as:

```text
stable
beta
nightly
```

Channel names are application-defined.

Trust relationships and downgrade rules remain enforced independently of channel labels.

---

# 168. Repository Portability

A `zup` update repository should be hostable on ordinary static storage/CDNs.

A basic repository should not require a proprietary `zup` cloud service.

Possible storage:

```text
S3-compatible object storage
GitHub Releases-backed publishing adapter
static HTTP
Cloudflare R2
ordinary CDN
```

Repository metadata should be portable.

---

# 169. No Vendor Lock-In

The framework should remain useful if every hosted `zup` service disappears.

Critical formats should be documented.

Application authors own:

```text
their installer
their signing keys
their repository
their payload
their metadata
```

Hosted tooling may eventually provide convenience, not ownership of the installation format.

---

# 170. Testing Model

Installer correctness requires more than unit tests.

The architecture should make actions testable through:

* pure planning tests,
* fake backend tests,
* recovery tests,
* failure injection,
* real Windows integration tests,
* VM snapshot tests.

Because actions expose:

```text
inspect
plan
apply
verify
reconcile
```

individual failure points can be simulated.

---

# 171. Failure Injection

The transaction engine should be designed so tests can simulate crashes after any durable boundary:

```text
after intent
before apply
after apply
before verification
after verification
before receipt
after receipt
```

A good installation engine should survive deliberately hostile interruption testing.

---

# 172. Dry Run

A true dry run stops before side effects.

It should still perform enough inspection to answer:

```text
what would change?
what requires elevation?
what would download?
what processes are blocking?
what disk space is required?
```

Dry-run output describes only operations represented by validated resource types.

---

# 173. Build-Time Inspection

`zup inspect` or equivalent tooling should be able to show information about a built installer:

```text
application
version
publisher
payload size
components
hashes
schema version
runtime version
signatures
remote sources
requested privileged capabilities
```

This is useful for security review and release pipelines.

---

# 174. Security Reviewability

A core advantage of declarative authoring is that an installer can be audited without executing arbitrary script for every ordinary operation.

A reviewer can inspect:

```text
files written
services installed
registry locations
PATH changes
protocol registrations
external commands
network sources
```

No opaque manifest execution path appears in the plan.

This is a meaningful improvement over treating the entire installer as arbitrary imperative code.

---

# 175. Default Privilege Policy

Actions declare privilege requirements.

The planner derives whether elevation is needed.

For example:

```text
install into user-local directory → user
user Start Menu launcher          → user
HKCU registry                     → user

Program Files                     → machine
HKLM                              → machine
machine service                   → machine
system PATH                       → machine
```

The author can constrain scope, but should not manually sprinkle `run_as_admin()` through the installer.

---

# 176. Privilege Escalation Timing

The installer should perform as much safe work as possible before elevation.

Example:

```text
parse
inspect
resolve plan
download
verify
extract private staging
calculate blockers
        ↓
UAC
        ↓
commit privileged changes
```

This reduces time spent with an elevated worker alive.

---

# 177. Network Separation

The privileged worker should preferably not need broad network access.

Payload downloading and verification can occur in the normal process.

The privileged worker receives already verified local artifacts plus an immutable plan.

This significantly narrows its responsibility.

---

# 178. Bundle Self-Containment

A normal Windows installer should not require:

* .NET runtime,
* WebView2,
* Node.js,
* Python,

just to display itself.

If the target application requires those runtimes, they are application prerequisites, not installer-runtime dependencies.

This is one reason GPUI and native Rust are attractive for the default frontend.

---

# 179. Installer Size

Small installer size is desirable but secondary to:

* correctness,
* security,
* startup time,
* maintainability.

The project should avoid pulling entire frameworks into the bootstrap runtime unnecessarily.

The headless engine and default UI should remain separable enough that web/maintenance runtimes can potentially omit unused pieces.

---

# 180. Startup Performance

A setup executable should reach its initial UI without:

```text
extracting the entire application
starting a local web server
booting a browser runtime
```

The initial metadata and UI assets should be directly accessible from the installer container.

Heavy payload work starts only after necessary preflight.

---

# 181. Installer Self-Verification

Before applying privileged payload, the runtime should be able to verify:

```text
bundle header
manifest integrity
content index
required runtime/schema versions
payload hashes
trusted release metadata
```

A malformed bundle must fail before partial installation.

---

# 182. Publisher Verification of Prerequisites

When installing third-party prerequisites, hash verification should be mandatory whenever an artifact is specified by digest.

Where appropriate, Windows publisher trust can additionally be checked through `WinVerifyTrust`.

A URL alone is not a sufficient trust declaration.

---

# 183. Repairing Unknown Drift

The engine should classify drift rather than flatten it into "broken."

Examples:

```text
Same
Gone
Modified
Replaced
Ambiguous
```

The exact vocabulary can evolve.

The important rule is that automatic repair must not overwrite state that appears to have been intentionally replaced by another owner unless policy explicitly permits it.

---

# 184. Shared Resources

Some resources may intentionally be shared across installations.

The manifest should eventually allow ownership semantics such as:

```text
exclusive
shared
preserve
external
```

A shared runtime cannot safely be deleted merely because one application uninstalls.

This becomes especially relevant for:

* common services,
* runtimes,
* shared data,
* machine-level configuration.

---

# 185. User Data

`zup` must distinguish application binaries from user data.

Uninstall defaults should preserve:

```text
documents
profiles
project data
databases
caches where policy says preserve
```

An application can explicitly offer "remove application data" as a separate choice, but that should not be inferred from directory location.

---

# 186. Default Uninstall UX

Uninstall should be substantially simpler than historical installer wizards.

Typical flow:

```text
Remove Acme?

Your personal data will be kept.

[ Remove ]
```

Optional destructive choices should be clearly separated:

```text
[ ] Also remove application data
```

The underlying plan remains inspectable.

---

# 187. Cross-Version Ownership

Stable resource identities and ownership must survive upgrades.

Suppose v1 installs:

```text
service "acme-agent"
```

and v2 changes its executable path.

The update engine should identify it as the same managed logical service, not:

```text
delete unrelated service
create coincidentally same-named service
```

Stable logical identity matters.

---

# 188. Migrations

Some releases contain genuine state migrations that cannot be represented as simple desired-state replacement.

These can use:

* built-in migration operation types,
* capability-scoped extensions when available.

Migration metadata should include:

```text
from-version constraints
idempotence expectations
rollback capability
privilege
```

---

# 189. Irreversible Operations

The current declarative operation set is selected so the engine owns durable receipts and precise rollback.

An operation that cannot meet that contract must not be admitted by the current manifest or runtime. A separate extension WIT world must expose its rollback guarantee before the planner can schedule it.

The default user UI should surface failure and recovery state without exposing internal transaction mechanics.

---

# 190. Commit Boundary

An installation transaction should have a meaningful commit boundary.

Before commit, temporary backups and staging state required for rollback are retained.

After commit:

* installation ledger becomes authoritative,
* temporary rollback material can be cleaned according to policy,
* deferred cleanup may continue.

This prevents cleanup itself from being confused with installation success.

---

# 191. Garbage Collection

Interrupted builds, stale downloads and old payload caches require cleanup.

Garbage collection must only remove objects proven to be:

* unreferenced,
* transaction-temporary,
* expired by policy.

It must never use aggressive heuristics over arbitrary application directories.

---

# 192. Cache

A content-addressed cache can avoid downloading or recompressing identical payload objects repeatedly.

Possible identity:

```text
BLAKE3(payload bytes)
```

or another internal digest.

Cache location and size policy differ between:

* developer build cache,
* installer download cache,
* installed maintenance cache.

---

# 193. Proxies and Enterprise Networks

HTTP behavior must respect real desktop environments.

`reqwest` supports proxy configuration primitives, but `zup` must define policy around:

* system proxy discovery,
* explicit proxy configuration,
* authenticated proxies,
* offline environments,
* TLS failure reporting.

Web installation should fail usefully rather than presenting a generic network error.

---

# 194. Retry Policy

Retries should distinguish:

```text
transient network failure
server 500
server 404
signature mismatch
hash mismatch
authentication failure
```

A cryptographic mismatch is not something to retry indefinitely.

---

# 195. Mirror Policy

When multiple mirrors exist, signed metadata determines valid content.

Mirrors provide availability, not trust.

A mirror returning correct bytes is equivalent to any other valid mirror.

A mirror returning different bytes is rejected.

---

# 196. Update Rollback

If an update fails after existing application files have begun to change, rollback should restore the previous committed release whenever sufficient rollback material exists.

This strongly favors:

```text
stage new version
retain old version until commit
```

over destructive in-place replacement.

---

# 197. Side-by-Side Staging

Applications that permit it may use a versioned layout:

```text
Acme/
├── versions/
│   ├── 1.4.0/
│   └── 1.5.0/
└── current -> ...
```

or a Windows-appropriate equivalent.

This can make switching versions particularly reliable.

It should be an install strategy, not a universal requirement.

Some applications require fixed paths.

---

# 198. Strategy Abstraction

Filesystem deployment may eventually support strategies such as:

```text
in-place managed
staged atomic replacement
side-by-side versions
portable
```

All still produce resources tracked by the same ownership/ledger model.

---

# 199. Default Application Architecture

For a normal Windows desktop application, the recommended default plan is approximately:

```text
private staging
    ↓
verify
    ↓
Restart Manager coordination
    ↓
commit files
    ↓
system integrations
    ↓
verify
    ↓
ledger commit
    ↓
restart/launch
```

This should require no advanced author configuration.

---

# 200. v1 Scope

v1 targets Windows and establishes the architecture required for later platforms.

Its conceptual product surface includes:

* language-agnostic `zup.toml`,
* schema validation,
* components,
* conditions,
* installation variables,
* Installer IR,
* planning,
* journaling,
* recovery,
* ownership-aware uninstall,
* repair,
* modify,
* upgrade/update semantics,
* per-user installation,
* per-machine installation,
* split UAC elevation,
* managed files/directories,
* registry operations,
* launchers,
* Windows services,
* PATH/environment integration,
* file association registration,
* URI protocol registration,
* installed-app registration,
* Restart Manager integration,
* prerequisites,
* offline installers,
* web installers,
* payload verification,
* signing integration,
* headless mode,
* default GPUI frontend,
* declarative custom frontend layout,
* structured diagnostics.

The current plugin milestone adds a narrow, planner-only WebAssembly component boundary. It uses the checked-in `zup:plugin/planner@1.0.0` WIT world, has no imports, and returns typed resources for the ordinary installation plan. It is not an unrestricted native-plugin ABI, privileged action API, or custom action escape hatch; new capabilities require separate WIT worlds.

---

# 201. v2 Direction

v2 expands the extensibility and portability model rather than replacing the v1 engine.

Primary directions include:

* additional capability-scoped extension worlds,
* capability-scoped mutation providers,
* custom condition providers,
* payload/repository providers,
* UI extensions,
* broader secure-update tooling,
* MSI/enterprise adapters,
* macOS backend,
* Linux backend,
* additional packaging outputs.

Exact version assignment is not a promise that every item must land simultaneously.

The architectural requirement is simply that v1 does not make them impossible.

---

# 202. Cross-Platform End State

The intended long-term model is:

```text
                  zup project
                      │
                      ▼
              platform-neutral IR
                      │
          ┌───────────┼───────────┐
          ▼           ▼           ▼
       Windows      macOS       Linux
       backend      backend     backend
          │           │           │
          ▼           ▼           ▼
      Setup.exe    dmg/pkg/...   native/
                               package outputs
```

The same project may contain platform-specific overrides without duplicating the entire installer definition.

---

# 203. Why `zup` Should Not Be "A Rust Installer"

Calling `zup` a "Rust installer" would undersell the architecture and create the wrong expectation.

Rust provides:

* safety,
* native binaries,
* excellent OS bindings,
* good compression/network/crypto ecosystems,
* portability.

But the product is for application developers generally.

The intended relationship is:

```text
written in Rust
≠
requires Rust
```

A C developer should experience:

```bash
clang src/*.c -o dist/acme.exe
zup build
```

not:

```text
learn Rust
implement Installer trait
compile custom Rust setup project
```

---

# 204. Why `zup` Should Not Be "A Pretty Installer"

The default interface is strategically important because it is immediately visible.

But a beautiful installer window alone is not the project.

Without the engine, `zup` would merely be another bootstrapper skin.

Its real value comes from the combination:

```text
modern UX
+
declarative authoring
+
deep customization
+
reliable machine-state engine
+
lifecycle management
+
secure distribution
```

---

# 205. Why `zup` Should Not Be "NSIS in Rust"

NSIS demonstrates the value of programmability.

`zup` should not copy its fundamental authoring model.

The key inversion is:

```text
NSIS:
    installer is primarily a script
    reusable commands manipulate state

zup:
    installer is primarily desired state
    engine derives mutations
    no arbitrary code path in the manifest
```

This difference enables much stronger planning, recovery, auditing and ownership behavior.

---

# 206. Core Product Thesis

The project can be summarized in four statements.

### 1. Installation should be data when possible

If an application wants a launcher, it should declare a launcher.

### 2. Machine mutation should be transactional where practical and recoverable everywhere else

A crash is an expected systems event.

### 3. Advanced customization should not require abandoning the engine

A completely custom installer UI should still receive the same planner, journal, rollback and platform semantics.

### 4. The easy path should already look excellent

A developer should need custom UI because their product requires it, not because the default installer is embarrassing.

---

# 207. Final Architectural Shape

The resulting system is approximately:

```text
                    ┌──────────────────┐
                    │    zup.toml      │
                    │   UI / assets    │
                    └────────┬─────────┘
                             │
                             ▼
                    ┌──────────────────┐
                    │ Manifest Compiler│
                    └────────┬─────────┘
                             │
                             ▼
                    ┌──────────────────┐
                    │   Installer IR   │
                    └────────┬─────────┘
                             │
                             ▼
                    ┌──────────────────┐
                    │     Planner      │
                    └────────┬─────────┘
                             │
                             ▼
                    ┌──────────────────┐
                    │ Execution Graph  │
                    └────────┬─────────┘
                             │
              ┌──────────────┴──────────────┐
              │                             │
              ▼                             ▼
     ┌──────────────────┐          ┌──────────────────┐
     │ Transaction Core │◄────────►│ Frontend Protocol│
     └────────┬─────────┘          └────────┬─────────┘
              │                             │
              │                             ▼
              │                    ┌──────────────────┐
              │                    │   GPUI default   │
              │                    │   Custom UI      │
              │                    │   Headless       │
              │                    └──────────────────┘
              ▼
     ┌──────────────────┐
     │ Platform Backend │
     └────────┬─────────┘
              │
              ▼
     ┌──────────────────┐
     │     Windows      │
     │                  │
     │ Filesystem       │
     │ Registry         │
     │ SCM              │
     │ Restart Manager  │
     │ Shell/COM        │
     │ UAC              │
     │ WinTrust         │
     └──────────────────┘

               current extension boundary:

                    │
       planner-only WebAssembly components
```

The central boundary is the Installer IR and resource-operation engine.

Everything else can evolve around it.

---

# 208. Conclusion

`zup` is intended to occupy the space between highly programmable traditional installer systems and highly opinionated modern application packagers.

It should provide the freedom normally associated with installer scripting without requiring every application author to learn or maintain an installer-specific programming language.

Its default authoring experience is declarative.

Its custom behavior is extensible.

Its default UI is modern and polished.

Its UI is replaceable.

Its engine is headless.

Its mutations are planned.

Its transactions are journaled.

Its rollback is ownership-aware.

Its updates use the same state model as installation.

Its privileged process is deliberately small.

Its trust model does not depend on the CDN.

Its Windows implementation uses modern documented system facilities rather than obsolete transaction mechanisms.

Its implementation is Rust-native, but its users do not need to be.

v1 establishes this model on Windows.

The long-term destination is a fully cross-platform application installation framework whose platform backends preserve native operating-system semantics rather than reducing every system to the lowest common denominator.

The simplest possible `zup` project should be almost boring:

```toml
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"
main = "Acme.exe"

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Acme"
```

and:

```bash
zup build
```

should produce something that already feels finished.

Everything underneath that command is where `zup` earns its existence.
