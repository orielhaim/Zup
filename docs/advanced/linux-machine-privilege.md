# Linux machine-scope privilege: threat model and architecture

Phase 5 establishes the Linux machine-scope privilege boundary. This
document records what the privileged worker defends against, what it
deliberately does not, and how each defense is constructed. It is a
security architecture record, not user documentation: user-facing
behavior lives in the [platform support matrix](/guide/platforms).

## The boundary

```text
unprivileged installer/frontend
        │  user choices / expected plan identity
        ▼
unprivileged planning (read-only, binds the expected digest)
        │  explicit elevation request
        ▼
      pkexec (polkit authenticates the administrator)
        ▼
privileged Zup worker (this binary, `__privileged-worker`)
        ├─ authenticate peer and session
        ├─ independently verify the package
        ├─ independently validate machine intent
        ├─ reconstruct and verify plan identity
        ├─ enforce the privileged path policy
        ├─ acquire the machine lock
        └─ execute the normal Zup transaction → /opt, /var/lib/zup, typed
     systemd unit sources
```

Everything originating from the unprivileged process is untrusted:
arguments, paths, serialized plans, operation names, component
selections, scope, transaction ids, digests, socket paths, environment
variables, frontend state, and package metadata - even when Zup itself
generated it earlier. The architecture is propose → reconstruct/verify
→ enforce → execute. There is no path on which the unprivileged
process validates and the privileged process blindly executes: the
client sends no plan at all, only intent, and the worker plans from
its own verified inputs.

## Threats and defenses

| Threat | Defense |
|---|---|
| Malformed or malicious installer/package metadata | The carrier verifier runs inside the worker on the trusted carrier; malformed packages refuse before planning. |
| Compromised or untrusted unprivileged frontend | Intent is revalidated field by field; the plan digest must equal the worker's reconstruction; Execute names the exact digest once. |
| Another process running as the same user | Peer uid must equal the authorizing uid; the rendezvous is private; state only trusts root-owned private entries. |
| Concurrent installer sessions | The root lock is acquired at preparation and held through execution; a second session refuses as busy. |
| PID reuse | The peer is pinned with a pidfd held for the session (or pid plus start time plus uid where pidfd is unavailable); liveness is rechecked before Execute. |
| IPC endpoint replacement | The rendezvous directory is validated (owner, type, privacy) before connecting; peer credentials decide identity, never the pathname. The worker additionally requires the exact client pid it was launched for, and the client skips impostor connections until its real worker arrives. |
| Replay of old worker messages | Versioned bounded framing, per-sender sequences, one Prepare and one Execute per session, enforced by the portable session tracker. |
| Cross-session message confusion | Every frame binds the session identity; strangers refuse. |
| Plan substitution after authorization | Prepare carries the client's expected digest; Prepared echoes the worker's reconstruction; Execute names it exactly; mismatch refuses. |
| Package substitution after authorization | The carrier inode is pinned at verification and rechecked before Execute; per-file digests and post-publish verification add depth. |
| Symlink and path substitution | Descriptor-relative operations, symlink-ancestor refusal, no-follow opens, and the privileged destination allowlist (`/opt`, `/var/opt`, `/var/lib/zup`, plus typed `<unit>.service` sources under `/usr/local/lib/systemd/system`). |
| Filesystem races | No check-then-act on names: kernel-enforced exclusive publication, durable backups before replace, atomic renames. |
| Hard links | Nothing publishes in place: creates are exclusive, replaces rename over the name, removals unlink the name. A hard-linked victim keeps its bytes because the inode is never truncated or written through. |
| Arbitrary absolute paths | The destination policy refuses everything outside the allowed trees; install-directory overrides stay inside the program tree; `..` is refused, never resolved. |
| Malicious special files | Unexpected-kind refusals everywhere a regular file or directory is expected, in state, payload, and maintenance alike. |
| Worker or client death during execution | The transaction is durable: the next invocation recovers the root-owned journal before accepting new work. |
| Stale journals | Recovery-before-mutation on every machine operation; committed-but-unpublished gaps are published first. |
| Untrusted environment variables and `PATH` | Machine roots are policy constants, never environment; `pkexec` is resolved from absolute system paths and validated, never from `PATH`. |
| Malicious plugins | Machine scope refuses projects that need plugin execution; planning itself rejects active plugins before any plugin code could run. |
| Service unit substitution | The unit name is re-derived from the stable service identity and the bytes re-rendered from the operation inside the worker; the plan digest binds both, and Execute names the exact digest. |
| Foreign service binaries | The executable must resolve under the Zup-owned program tree to a declared executable payload, and is revalidated (regular, root-owned, private, runnable, no-follow) before systemd integration commits. |
| Unit identity collision | Same-name units in `/etc`, `/run`, `/usr/local/lib` (foreign), or `/usr/lib` refuse the transaction; distro and administrator units are never overwritten. |
| Administrator override removal | Full `/etc` overrides and drop-ins are never deleted; an override shadowing the source is a conflict requiring administrator action. |
| Unrelated enablement deletion | Broad disable operations that would delete administrator-added links refuse instead; only Zup-owned enablement is retired. |
| Service as command runner | No shell, no `StartUnit`/`StopUnit`/`RestartUnit` on the manager surface, no D-Bus force flags: installing registers boot policy and never executes application code. |
| Cancelled authentication | Typed outcomes distinguish cancellation, denial, missing mechanism, worker failure, and protocol failure. |

## Explicit non-goals

- **Root itself is not an attacker.** No defense is attempted against an
  already-compromised kernel or root user.
- **The initial `pkexec` trust boundary is honest, not cryptographic.**
  If an administrator authorizes a tampered executable, Zup cannot turn
  that executable back into trusted code. What the architecture prevents
  is the worker becoming a reusable confused deputy: after the first
  install, maintenance prefers the root-owned generation, and every
  operation re-verifies before it trusts.
- **No daemon, no setuid, no sudo fallback, no capabilities.** One
  authorization serves one bounded worker lifetime. Test harnesses may
  use `sudo` to stage isolated root-owned environments in CI; production
  never does.
- **No user services, socket/timer units, service users, environment
  files, hardening or resource directives, desktop integration, PATH
  mutation, or package-manager prerequisites in the privileged worker.**
  Those are later phases with their own trust models.

## Trust anchors

- The session identity travels in `pkexec` argv because `pkexec` passes
  arguments, not secrets. That exposure is safe by construction: the
  session id is identity, not capability. Possession of it authorizes
  nothing - the live socket connection, the kernel peer credentials, the
  pidfd pin, and the per-session handshake do. It is never logged.
- State root creation verifies the trusted parent and every existing
  entry, sets explicit modes, and never inherits umask behavior (the
  worker additionally runs under a restrictive umask).
- Public metadata (ledger) is root-owned `0644` so unprivileged planning
  and status inspection can read it; private state (journals) is `0600`
  under `0700` directories; lock markers are `0644`; maintenance
  generations are runnable but never writable below root.
- Ownership and privacy are verified before trust, on the ledger, the
  journals, the lock markers, and the maintenance generation - never
  assumed from a pathname.
- Test isolation never flows through environment variables or IPC: tests
  inject explicit roots through constructors the production paths never
  call, and the real worker always enforces the production roots.
