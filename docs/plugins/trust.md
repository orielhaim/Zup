# F14: compile untrusted WASM locally

Package publishers authorize application content. They do not have authority to
supply executable Wasmtime native code to the installer process. SHA-256, TUF,
release signatures, engine fingerprints and ELF headers establish identity or
compatibility, not compiler provenance.

The former loader admitted author-chosen native bytes after those checks and
called unsafe Component::deserialize. A regression at b8cc63d2 reproduced admission
of mutated AOT with a matching digest without invoking deserialization.

## Supply chain and trust

`zup.toml` resolves developer WASM components. `zup plugin build` still uses
wasm32-wasip3. The build host checks source identity and validates the component
with safe Wasmtime compilation, then packages the original WASM bytes. Cross-target
build validation executes on the build host while retaining the requested target
in planning context. Installation compiles on the actual Windows or Linux host.

Package metadata, embedded payloads, online TUF targets and acquired content caches
all carry those WASM bytes under their content identity. Installer, worker and
maintenance activation converge on the same loader. The loader copies bytes into
owned UntrustedSourceComponent storage after integrity checks. Lazy activation
uses Component::new and the existing strict import/export/WIT/resource validation.
Only ValidatedComponent can execute a planner. Hostile native AOT, including genuine
but publisher-supplied compiler output, is rejected as an unsupported input format.
Forging a WASM header only reaches safe parsing and compilation.

Zup has no native deserialization API or trusted-native-byte capability anymore.
The loader and contract validation modules forbid unsafe Rust. There is no unsafe
wrapper callable with Vec<u8>, and no trust flag. Wasmtime's compiler and sandbox
are the trusted computing base; package authors and acquisition storage are not.

The existing WASI P3 CLI restriction, denied clocks, absence of filesystem/network/
random imports, fuel, epoch watchdog, cancellation, memory bounds and output quotas
remain enforced by the same validation and invocation paths. Compilation is lazy
and bounded by component byte size, but its CPU cost is not subject to guest fuel.

## Format and cost

Component format 2 replaces AOT format 1, with component_size, component_sha256
and component_format_version metadata. Old packages must be rebuilt. Engine policy
fingerprints have a new domain and remain target/version/configuration bindings,
never evidence of trusted compiler origin. No compatibility aliases are retained.

The runtime now includes Cranelift. The compiler feature adds cross-architecture
build support; native runtime compilation is always available. There is no second
compiler or daemon. Each activated component is compiled once per executor and
reused in memory. New processes compile again. No persistent native code cache is
read or written, so cache replacement, publication crashes and native provenance
custody do not arise. Acquired WASM caches remain untrusted inputs to safe compilation.
Installer size increases and cold activation pays compilation cost. Release size
and cold/warm performance have not been benchmarked; no numeric claim is made.

## References

- [Wasmtime 50.0.1 Component::deserialize](https://docs.rs/wasmtime/50.0.1/wasmtime/component/struct.Component.html#method.deserialize)
  inherits Module::deserialize's requirement for unmodified compiler output.
- [Module::deserialize safety](https://docs.rs/wasmtime/50.0.1/wasmtime/struct.Module.html#method.deserialize)
- [Wasmtime security model](https://docs.wasmtime.dev/security.html)

Integrity of arbitrary native bytes cannot establish the unsafe API's precondition.
Use safe Component::new for externally supplied components.

## Validation on 2026-11-13

- Reproduced the pre-fix admission bug at b8cc63d2 without deserializing hostile bytes.
- Windows focused package/build/contract/runtime suites: 108 passed.
- Native Linux focused package/build/contract/runtime suites under WSL: 109 passed.
- Real Rust 1.100 wasm32-wasip3 fixture rebuilt and executed on both hosts.
- Embedded/acquired equivalence and native AOT rejection are covered on both hosts.
- Workspace all-features/all-targets Clippy with warnings denied passed.
- Workspace all-features doctests and formatting passed.
- Initial full Windows workspace run: 2078 passed, 16 installer lifecycle/UI failures,
  49 skipped. Staged runtime templates were stale; a fresh toolchain rerun follows.
- Privileged Windows/Linux execution was unavailable in this session. No host ACL,
  VM or elevation configuration was changed. Additional architecture/ABI hosts,
  release size and cold/warm performance measurements remain unverified.
- Linux installer suite after building the native test driver: 5 passed, including authenticated update and offline maintenance cases.
- Refreshed Windows toolchain templates: 30 installer tests passed, 1 failed,
  2 skipped. The remaining authenticated acquired crash-recovery test expected
  ZUP_TEST_FAULT_POINT to terminate a production runtime built without test-faults;
  the runtime instead exited successfully. Crash-instrumented coverage remains open.
  Generated-file lifecycle, repair, authenticated updates and offline maintenance passed.
