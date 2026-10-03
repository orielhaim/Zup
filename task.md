# Task: Rebuild Zup's Public SDK Architecture

Refactor Zup's public authoring architecture around a single official Rust SDK.

This is not a migration task. This is a prototype and Zup is currently its only real consumer. Treat the current public API, protocol layout, crate naming, generated projects, examples, tests, and documentation as disposable if a cleaner architecture requires breaking them.

Do not preserve compatibility with the current `zup-ui-sdk` design.

Do not add deprecated aliases, compatibility shims, transitional APIs, legacy wrappers, old command aliases, migration layers, dual protocol support, old package readers, or "temporary" adapters.

Do not bump protocol/API versions merely because the implementation changes. There are no external consumers that need migration. If the cleanest architecture requires completely replacing an existing protocol or ABI, replace it in place.

The goal is a deep architectural cleanup, not a rename.

## Core design

The public Rust authoring surface should become:

```text
zup-sdk
├── preset
└── plugin
```

A Rust developer writing Zup integrations should normally depend on one crate:

```toml
[dependencies]
zup-sdk = { version = "...", features = ["preset"] }
```

or:

```toml
[dependencies]
zup-sdk = { version = "...", features = ["plugin"] }
```

The developer should not need to understand or directly depend on:

```text
zup-ui-protocol
zup-ui-ipc
zup-plugin-contract
WIT versions
IPC framing
handshakes
protocol negotiation
wasmtime internals
host runtime internals
```

Those concepts may still exist internally where they represent real architectural boundaries, but they must not leak into the normal authoring experience.

The governing rule is:

> Authors depend on `zup-sdk`. Hosts depend on contracts. Authoring code should not need to understand the wire.

## 1. Replace `zup-ui-sdk`

`zup-ui-sdk` is conceptually misnamed.

It currently does far more than provide UI helpers. It owns preset startup, transport integration, handshake behavior, protocol-facing state, settings, assets, session handling, GPUI integration, and author-facing abstractions.

The real concept is a **preset SDK**.

Replace the current `zup-ui-sdk` architecture with a preset-specific implementation layer, preferably:

```text
zup-preset-sdk
```

and expose it publicly through:

```rust
zup_sdk::preset
```

Normal users should not need to know that `zup-preset-sdk` exists.

Do not leave `zup-ui-sdk` as an alias or compatibility crate. Delete it once all consumers have moved.

Do not preserve the old module paths.

Update every internal consumer, generated project, fixture, test, example, documentation reference, verification script, graph policy, workspace dependency, and comment accordingly.

## 2. Add the top-level `zup-sdk`

Create `zup-sdk` as the official Rust authoring facade.

It should expose narrowly defined authoring domains rather than internal implementation concepts.

Expected shape:

```rust
zup_sdk::preset
zup_sdk::plugin
```

Useful public imports may look like:

```rust
use zup_sdk::preset::prelude::*;
```

and:

```rust
use zup_sdk::plugin::prelude::*;
```

The facade should hide internal crate boundaries.

Do not turn `zup-sdk` into a dumping ground for arbitrary Zup internals.

Do not expose modules such as:

```text
protocol
ipc
runtime
installer
transaction
engine
artifact
bundle
host
```

unless there is an actual third-party authoring requirement for them.

The SDK models what developers **author**, not how Zup is implemented internally.

## 3. Feature model

Use coarse authoring features:

```text
preset
plugin
test-support
```

Do not create feature soup.

Avoid features such as:

```text
ui
protocol
ipc
wire
runtime
wasmtime
gpui
manifest
build
```

Those are implementation details, not authoring roles.

Prefer no meaningful default feature if that keeps dependency graphs clean.

A plugin build must not accidentally pull GPUI.

A preset build must not accidentally pull Wasmtime host/runtime infrastructure.

The CLI-generated project should specify the appropriate feature explicitly.

## 4. Preset authoring API

The preset author experience should be centered around:

```rust
zup_sdk::preset
```

A minimal application should conceptually look like:

```rust
use zup_sdk::preset::prelude::*;

struct MyPreset;

impl Preset for MyPreset {
    type Settings = Settings;

    fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
        // normal GPUI application code
    }
}

fn main() {
    zup_sdk::preset::run::<MyPreset>();
}
```

Presets should continue to be normal Rust/GPUI applications.

Do not create a Zup-specific layout DSL, widget abstraction, screen framework, markup format, or artificial component hierarchy.

Zup's SDK should provide the Zup-specific integration layer:

```text
Preset
PresetContext
session/state
typed settings
application assets
preset assets
actions
snapshots/presentation state
host capabilities where genuinely relevant
startup/runtime integration
GPUI stack integration
```

GPUI remains responsible for actual UI composition.

## 5. Hide the UI wire protocol from preset authors

Keep a clean versioned contract internally if it is architecturally useful.

A protocol crate like `zup-ui-protocol` may remain because separating pure wire/domain data from the installer engine is valuable.

Likewise, a portable transport implementation such as `zup-ui-ipc` may remain.

However, these should become implementation-level dependencies beneath the preset SDK.

Normal preset authors must not depend directly on them.

Remove public documentation that teaches normal preset authors how the wire works.

Remove unnecessary public re-exports like exposing the entire protocol crate merely as an escape hatch.

If an advanced raw interface is genuinely required later, it can be designed deliberately later. Do not preserve one now because the old SDK exposed it.

The preset SDK should own protocol compatibility and negotiation automatically.

The author updates the SDK; the SDK speaks the correct protocol.

## 6. Keep independent version axes internally

Do not conflate these concepts:

```text
Rust SDK version
preset UI wire protocol version
plugin Component Model ABI version
package/artifact format version
```

They are separate contracts.

For example, conceptually it should be possible for:

```text
zup-sdk 0.x
preset protocol N
plugin ABI M
```

to coexist.

But those details should normally be handled by the SDK and host automatically.

Do not make users manually coordinate protocol crate versions.

Again: because the project is currently a private prototype in practical terms, do not increment these versions simply to represent this refactor. Replace the current versioned definitions in place where appropriate.

Version increments should represent a compatibility event with real consumers, not internal prototype churn.

## 7. Fix the current preset dependency leakage

The current generated preset project claims the SDK is sufficient, but generated projects still directly depend on things such as:

```text
gpui-kit
serde
schemars
```

Clean this up as far as reasonably possible.

The desired authoring experience is close to:

```toml
[dependencies]
zup-sdk = { version = "...", features = ["preset"] }
```

The SDK should re-export the GPUI stack that presets are expected to use so that Zup owns compatible versions.

Do not require preset authors to coordinate Zup's GPUI version independently.

Evaluate whether serde/schemars also need to remain direct user dependencies.

If the clean solution is to re-export the required derive ecosystem, do so.

If a small derive macro owned by Zup provides substantially cleaner DX, consider something conceptually like:

```rust
#[derive(zup_sdk::preset::Settings)]
struct Settings {
    hero: Option<String>,
}
```

Do not add a macro merely for aesthetic reasons, however. Prefer the smallest clean API.

The key requirement is that the generated project should reflect the claim that Zup provides an SDK rather than making the user manually assemble Zup's implementation stack.

## 8. Build a real plugin SDK

The current plugin authoring workflow exposes far too much implementation machinery.

Today a Rust plugin author needs to understand or manually perform things like:

```text
copying/vendoring Zup's WIT
pinning the matching WIT contract
using wit-bindgen
choosing the correct world
installing compatible wasm-tools
building wasm32-unknown-unknown
componentizing the resulting module manually
understanding the plugin ABI directly
```

This is poor SDK-level DX.

Create a proper Rust plugin authoring layer beneath:

```rust
zup_sdk::plugin
```

Prefer a dedicated implementation crate such as:

```text
zup-plugin-sdk
```

if that keeps the dependency graph and responsibilities clean.

The normal Rust plugin author should work with a high-level Rust API.

Conceptually:

```rust
use zup_sdk::plugin::prelude::*;

struct MyPlugin;

impl Plugin for MyPlugin {
    fn plan(context: Context) -> Result<Plan, Error> {
        // ...
    }
}

zup_sdk::plugin::export!(MyPlugin);
```

The exact API is yours to design. Optimize for correctness, minimal ceremony, good diagnostics, and clear ownership.

## 9. Keep WIT as the canonical cross-language plugin ABI

Do not replace the Component Model contract with a Rust-only ABI.

The WIT contract is valuable because plugins should not fundamentally depend on Rust.

The relationship should be:

```text
WIT = canonical plugin ABI
zup-plugin-sdk = official Rust bindings + ergonomic authoring layer
zup-sdk::plugin = public Rust facade
```

A future plugin written in another language should still be able to implement the same WIT world.

The Rust SDK should generate or encapsulate bindings against the exact WIT contract owned by Zup.

A Rust author should not need to manually copy the WIT into their project.

## 10. Separate plugin guest SDK from plugin host infrastructure

Do not use the existing `zup-plugin-contract` crate as the public plugin author SDK if it contains host-side infrastructure.

Currently that area includes concerns such as:

```text
Wasmtime engine setup
validation
AOT compatibility
engine fingerprints
runtime configuration
compiler features
host invocation errors
resource limits
```

Those are host/build/runtime concerns.

A plugin guest should not pull them.

Split responsibilities aggressively if necessary.

A clean conceptual architecture would be:

```text
zup-sdk
  └── plugin

zup-plugin-sdk
  └── lightweight guest authoring API/bindings

WIT contract
  └── canonical ABI

zup-plugin-contract
  └── host-side ABI validation/metadata if still useful

zup-plugin-runtime
  └── execution

zup-plugin-build
  └── build/component/AOT pipeline
```

Rename, merge, or delete crates if the current responsibilities do not justify the boundaries.

Do not keep a crate simply because it already exists.

## 11. Reconsider redundant wrapper crates

This refactor is explicitly also a deep cleanup.

Inspect every crate around:

```text
UI
presets
plugins
protocols
IPC
plugin contracts
plugin build/runtime
preset development
preview
composition
```

For each crate, determine whether it represents a real architectural boundary or merely wraps another crate.

Delete or merge wrapper crates that add no meaningful isolation, stability boundary, target separation, dependency separation, or ownership boundary.

Avoid architectures that exist only to make a diagram look layered.

A crate should exist because it enforces something useful, for example:

```text
different publication boundary
different target/platform
different trust boundary
different ABI boundary
different dependency graph
different runtime ownership
different build-time vs runtime responsibility
```

If two crates always change together, expose the same abstraction, and have no useful independent boundary, strongly consider merging them.

## 12. Rename CLI concepts from UI to preset where appropriate

Review the current:

```bash
zup ui init
zup ui dev
zup ui pack
zup ui inspect
```

The actual authored object is a preset, not "UI".

Prefer:

```bash
zup preset init
zup preset dev
zup preset pack
zup preset inspect
```

unless there is a strong architectural reason not to.

If changing this, delete the old `zup ui ...` commands.

Do not preserve aliases.

Do not emit deprecation notices.

There are no external users to migrate.

Also review filenames and configuration names such as:

```text
zup.ui.dev.toml
```

and rename them if a preset-centric name is clearer.

Again, perform a real rename, not a compatibility bridge.

## 13. Normalize terminology across the repository

After the refactor, terminology should be consistent.

Use:

```text
preset
plugin
SDK
contract
protocol
host
guest
```

only for their actual architectural meanings.

Avoid using "UI" when the concept is actually "preset authoring".

Avoid calling host/runtime infrastructure an SDK.

Avoid calling internal wire types public API.

Avoid names that expose implementation details unnecessarily.

Search the entire repository for stale terminology and clean it thoroughly.

## 14. Generated preset projects

Rewrite the project generated by the preset init command.

It should demonstrate the intended modern architecture rather than compatibility with the old one.

Keep it extremely small.

The generated source should teach the preferred API naturally.

Do not generate unnecessary comments, wrappers, helper layers, redundant files, or dependencies.

A new developer should be able to understand the entire generated project quickly.

The generated project must depend on `zup-sdk`, not the removed old SDK.

## 15. Plugin project generation / tooling

Review whether Zup should have a plugin init/build workflow analogous to presets.

The Rust plugin workflow should not require users to manually know how to componentize raw Wasm.

Prefer Zup tooling to own the mechanical steps necessary to produce the canonical plugin component.

For example, the eventual UX could be centered around commands such as:

```bash
zup plugin init
zup plugin build
```

The exact command design may differ, but the responsibility should be clear:

**Zup should know how to build a Zup plugin.**

A normal Rust plugin author should not need to manually reproduce Zup's internal Wasm Component Model pipeline.

Avoid requiring a globally installed exact `wasm-tools` version if Zup's existing toolchain system can own this dependency correctly.

Reuse the toolchain architecture where appropriate instead of adding another independent version-management mechanism.

## 16. Public vs internal crates

After the refactor, explicitly classify crates.

There should be a very small public author-facing surface.

Ideally, from a Rust author's perspective the primary public package is:

```text
zup-sdk
```

Internal contracts may still need to be published to crates.io because Cargo resolves transitive dependencies there. That is fine.

Published does not mean user-facing.

Documentation and examples should consistently guide users toward `zup-sdk`.

A protocol crate may technically be public on crates.io while still being documented as an implementation contract rather than the supported application authoring API.

Update repository validation rules to reflect this distinction.

## 17. Dependency graph requirements

Preserve strong graph boundaries.

Preset authoring must not reach installer internals.

Plugin guest authoring must not reach Wasmtime host machinery.

Shipped installer binaries must not accidentally reach development tooling.

Build-only crates must remain build-only.

Protocol/domain contracts should not depend on large internal engine crates.

Host engine types must not leak directly across public protocol boundaries.

Convert types at boundaries intentionally.

Update the existing dependency graph checks so they validate the new architecture rather than preserving the old crate names.

Delete checks that only exist to enforce obsolete structure.

## 18. Protocol cleanup

Because compatibility is irrelevant at this stage, use this refactor as an opportunity to inspect the preset protocol itself.

Do not assume the current wire format must survive.

Inspect:

```text
handshake
hello messages
capabilities
configuration
snapshots
actions
session state machine
framing
version checks
description metadata
asset transport
settings transport
errors
```

Simplify anything that is unnecessarily complicated.

If some protocol types exist only because the previous crate layout required them, remove them.

If an API exposes lower-level state that authors do not need, hide or redesign it.

Keep the host authoritative over installation state and actions.

A preset must remain presentation code, not an installer engine.

The host must validate requested actions.

Do not let presets gain direct access to plans, transactions, elevation internals, machine mutation, package internals, or other privileged engine state merely to simplify an API.

## 19. Preset safety/trust boundary

Retain the important conceptual boundary:

```text
host owns installation state
preset renders host state
preset requests actions
host validates actions
```

The SDK should make the safe path the natural path.

A preset should not have to construct transport messages manually.

A click should translate to an SDK-level action request.

Host state updates should arrive as SDK-level observable state.

The wire protocol is an implementation detail beneath that model.

## 20. Plugin safety/trust boundary

Likewise retain the plugin model:

```text
plugin receives planning context
plugin returns declarative resources
Zup owns lifecycle execution
```

Do not turn plugins into arbitrary installer scripts.

The plugin SDK should make declarative resource generation ergonomic without weakening the current security/lifecycle model.

Plugins should not mutate the machine directly.

Zup should still own install, upgrade, modify, repair, and uninstall semantics for plugin-produced resources.

## 21. Settings design

Review preset settings as part of the cleanup.

The good property to retain is that settings have one typed source of truth and generate their validation schema from the same definition.

Do not introduce a second schema DSL.

Do not require authors to maintain Rust types and a separate Zup schema manually.

If the existing `serde + schemars` implementation remains the cleanest option, keep the concept while hiding unnecessary dependency/version management behind the SDK.

Keep settings observable if the host can legitimately update them while the preset is running.

Remove accidental complexity around settings propagation if it is not required.

## 22. Assets design

Likewise review preset assets.

Preserve the useful distinction between:

```text
application-provided assets
preset-owned assets
SDK/component-library assets
```

but simplify the API and implementation where possible.

Asset precedence should remain deterministic.

Do not expose transport-level asset representation unless authors actually need it.

## 23. GPUI integration

Zup should own the expected GPUI stack version for preset authors.

A preset should not need to independently coordinate `gpui-kit` compatibility with Zup.

Expose the supported GPUI stack through the preset SDK.

Clean up the current situation where documentation says the SDK owns the GPUI stack while generated projects still depend directly on `gpui-kit`.

If direct dependencies are truly necessary because of Cargo/macros/traits, document the technical reason and minimize them. Do not preserve them merely because the old template did.

## 24. Tests

Rewrite tests around behavior and architectural contracts, not old implementation structure.

Delete tests whose only purpose is preserving obsolete crate names, legacy API paths, old command names, migration behavior, or backward compatibility.

Add/update tests that prove important properties such as:

```text
a third-party preset can build using only the supported SDK surface
a third-party plugin can build using only the supported SDK surface
preset authoring does not reach internal installer crates
plugin guest authoring does not pull host Wasmtime infrastructure
SDK feature graphs remain isolated
generated preset projects build outside the workspace
generated plugin projects build outside the workspace if plugin generation exists
preset SDK and host complete a real session correctly
plugin SDK output satisfies the canonical WIT contract
the host rejects invalid/incompatible inputs cleanly
```

Continue using real external-style fixtures where they prove that a third-party project can actually consume the published packages.

Do not create excessive tests for trivial getters/wrappers.

Use `rstest` effectively where parameterization meaningfully reduces duplication.

## 25. Documentation

Rewrite documentation around the new mental model.

Do not document the migration from the old architecture.

Do not say:

```text
Previously this was...
The old zup-ui-sdk...
For compatibility...
Deprecated...
```

The old architecture should disappear as if it never existed.

Documentation should explain the clean system:

```text
zup-sdk
presets
plugins
preset development
plugin development
contracts only where advanced/internal documentation needs them
```

Preset docs should use:

```rust
zup_sdk::preset
```

Plugin docs should use:

```rust
zup_sdk::plugin
```

Remove instructions telling Rust plugin authors to manually vendor WIT if the new SDK eliminates that need.

Remove stale diagrams, references, commands, generated snippets, examples, and comments.

Keep docs concise.

## 26. Examples and fixtures

Rewrite all relevant examples and fixtures to consume the real public API exactly as an external user would.

Do not let repository examples cheat by depending on internal workspace crates.

The default preset should behave like a third-party preset.

Plugin examples should behave like third-party plugins.

If an example needs special path patches during repository testing, keep those isolated to test infrastructure rather than changing the conceptual dependency model presented to users.

## 27. Publication verification

Replace the current `verify-public-crates` assumptions with checks matching the new architecture.

The checks should verify the actual supported consumer graph rather than hard-code the obsolete:

```text
zup-ui-protocol
-> zup-ui-ipc
-> zup-ui-sdk
```

For example, verify that:

```text
zup-sdk preset feature builds from outside the workspace
zup-sdk plugin feature builds from outside the workspace
all transitive published dependencies resolve correctly
no authoring surface reaches internal-only crates
features do not pull unrelated authoring/runtime stacks
```

Use Cargo metadata to validate the resolved graph.

Do not keep special cases for crates that no longer exist.

## 28. Workspace cleanup

After the architectural work, perform a full repository sweep.

Remove:

```text
obsolete crates
unused modules
unused exports
dead feature flags
legacy command handlers
compatibility aliases
old generated templates
stale tests
stale fixtures
old protocol helpers
unnecessary wrappers
dead documentation
obsolete scripts
unused workspace dependencies
unused build dependencies
unnecessary comments describing deleted architecture
```

Run dependency cleanup tools and ensure the workspace graph reflects the actual design.

Do not leave empty compatibility crates behind.

Do not leave TODO comments saying that old pieces can be removed later.

Remove them now.

## 29. No legacy requirement

This requirement is strict.

There must be no migration path from the current prototype API.

Do not implement any of the following:

```rust
pub use new_name as old_name;
```

No:

```text
zup-ui-sdk compatibility package
old `zup ui` CLI alias
old settings parser fallback
protocol v1 + protocol v2 dual support
old plugin ABI adapter
legacy `.zupui` reader
deprecated module aliases
feature aliases
compatibility constructors
migration scripts
automatic conversion of old projects
```

If changing an API or format improves the architecture, break it.

If changing a protocol improves the architecture, replace it.

If changing a package shape improves the architecture, replace it.

If changing command names improves the architecture, delete the old commands.

The repository after this work should look like it was designed this way from the beginning.

## 30. Avoid pointless abstraction

Although this is a large refactor, do not solve it by adding more layers.

Prefer fewer, stronger abstractions.

Do not create one-line forwarding wrappers simply to preserve conceptual layers.

Do not add traits where concrete types provide the needed flexibility.

Do not create separate crates for every noun.

Do not create `Facade`, `Adapter`, `Manager`, `Bridge`, `Provider`, or `Service` types without a concrete responsibility.

The desired result is simpler than the current architecture from an author's perspective and ideally simpler internally as well.

## 31. Expected conceptual end state

The result should approximately feel like:

```text
                        Rust authors
                            │
                         zup-sdk
                      ┌─────┴─────┐
                      │           │
                   preset       plugin
                      │           │
                      │           │
              preset author   plugin author
                 runtime        bindings
                      │           │
              ┌───────┘           └─────────┐
              │                             │
       preset wire/IPC                canonical WIT ABI
              │                             │
              └───────────┐       ┌─────────┘
                          │       │
                         Zup host
```

The user-facing concepts are:

```text
Preset
Plugin
```

The implementation concepts remain beneath them.

## 32. Final validation

Before considering the task complete:

- build the entire workspace;
- run formatting;
- run Clippy with the repository's strict settings;
- run all tests;
- run dependency/graph checks;
- build an external-style preset against `zup-sdk`;
- build an external-style plugin against `zup-sdk`;
- confirm plugin-only builds do not pull GPUI;
- confirm preset-only builds do not pull Wasmtime host infrastructure;
- confirm removed crate names and old CLI terminology no longer appear except where genuinely discussing historical artifacts is unavoidable;
- inspect `cargo tree` for accidental dependency leakage;
- inspect the workspace for dead wrappers and obsolete modules after the first refactor pass;
- perform a second cleanup pass specifically looking for code that only exists because of the old architecture.

Do not stop when the new API merely works.

The task is complete when the old architecture is gone and the resulting repository has one coherent public authoring model.
