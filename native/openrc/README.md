# OpenRC native control

Zup requires the `pidfd-control-v1` interface supplied by this patchset against OpenRC 0.63.2. Stock OpenRC does not provide it. Zup must refuse service deployment when this interface is unavailable.

The patch retains OpenRC's supervisor and shared retry engine. It adds pidfd control, a configurable exec handoff deadline, controller-instance and child-generation checks, bounded shutdown observation, and explicit shutdown acknowledgment. A failed handoff suppresses respawn and stops the captured child, including when the initiating client disconnects. It also reports native metadata write failures. The legacy OpenRC interface remains available to other services.

Managed services always use strict supervision, including services with automatic restart disabled. They run foreground native ELF executables. Scripts, daemonizing executables, reexec reconstruction, and healthcheck callbacks are unsupported. A surviving descendant prevents Zup from reporting quiescence.

Build on native x64 or ARM64 Linux with Python 3.12 or newer, a C compiler, Git, Meson, Ninja, pkg-config, Linux headers, and libcap development headers. On Alpine:

```sh
apk add build-base git meson ninja pkgconf linux-headers libcap-dev
python3 scripts/build-openrc-control.py --output "$PWD/target/openrc-control"
```

The builder verifies the locked source archive and patch, statically links OpenRC's internal libraries, and writes `supervise-daemon`, its license, and `build.json`. The executable still uses the host libc and libcap. It does not install files or change services. Provision the corrected controller as part of the host's OpenRC package maintenance before deploying Zup services. Keep the build receipt with native test results.

Do not replace a controller while managed services are running. Zup verifies its executable inode and refuses a changed native execution authority. The OS service manager and its configuration remain host prerequisites.

`scripts/verify-openrc-controller.py` exercises the built artifact in an exclusively assigned disposable OpenRC guest. It requires the build receipt, a C compiler for delayed-exec injection, Python 3, and a compiled `zup-test-linux-service` fixture. The suite covers delayed and interrupted startup, native escalation, bounded respawns, controller crashes, stale identities, competing shutdown observers, and actual PID reuse. The native VM workflow retains its build provenance and test output.
