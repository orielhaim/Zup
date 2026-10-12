# Engineering Principles

## Architecture

- **Design for evolution.** Before 1.0, prefer sound architecture over backward compatibility. No legacy APIs, migration layers, or compatibility shims unless explicitly required. Git preserves history.
- **Solve problems at their source.** Fix flawed designs instead of stacking patches and workarounds.
- **Prefer simplicity.** Every abstraction, dependency, and layer must provide clear value. Avoid speculative extensibility and unnecessary indirection.
- **Stay modern.** Prefer current Rust features, tooling, and technologies when they offer meaningful advantages. Accept calculated risks without compromising correctness or security.
- **Use Rust's type system.** Prefer enums, newtypes, and explicit state modeling over magic values, boolean flags, and manual encodings. Make invalid states unrepresentable when practical.

## Testing

- **Test meaningful behavior.** Focus on observable contracts, critical invariants, and realistic failure scenarios, not implementation details.
- **Justify every test.** Before adding one, identify the specific risk it covers and check whether existing tests already cover it. More tests do not automatically mean better coverage.
- **Avoid brittle tests.** Don't assert internal structure, incidental ordering, exact wording, or other details that can change without affecting correctness.
- **Keep coverage focused.** Avoid repetitive edge cases, excessive permutations, and tests that simply mirror the implementation.
- **Use `rstest` intelligently.** Parameterize genuinely related cases and share useful fixtures without introducing elaborate test infrastructure.
- **Protect critical guarantees.** Prioritize security, authorization, transactions, rollback, recovery, installation lifecycles, and public contracts.
- **Avoid expensive repetition.** Reuse build artifacts and setup where practical. Keep CI checks purposeful and avoid unnecessary compilation or overlapping verification.
- Prefer `cargo nextest` for tests and `cargo test --doc` for doctests.

## Comments & Documentation

- **Prefer self-explanatory code.** Clear names, types, and structure should carry the explanation.
- **Comments are the exception.** Write one only when important reasoning cannot be understood from the code, such as a non-obvious safety invariant, protocol constraint, or external quirk.
- **Keep comments minimal.** No narration of obvious behavior, implementation walkthroughs, historical context, or lengthy explanations. When necessary, be precise and brief.
- **Keep documentation relevant.** Document the current system for its intended audience. Avoid filler, repetition, and implementation details that don't serve the reader.
- **Write concisely.** The same principles apply to test names, assertion messages, errors, and developer-facing text.
