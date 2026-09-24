# Product

<!-- impeccable:product-schema 1 -->

## Users

People installing or maintaining desktop applications distributed as zup packages, and developers who build those packages.

## Product Purpose

Zup plans, installs, updates, modifies, repairs, and uninstalls packaged desktop applications through one transactional runtime.

## Positioning

The native installer and maintenance UI are frontends to the same planner, transaction engine, and authenticated elevated worker used by the CLI.

## Operating Context

Installers are launched directly from Windows Explorer. Maintenance executables are installed with the application. Machine-scope work requests elevation only when the operation requires it.

## Capabilities and Constraints

- Keep the developer CLI and headless lifecycle path functional.
- UI commands and progress use typed interfaces; GPUI does not implement lifecycle behavior.
- The default visual accent is cool blue on neutral surfaces, with system light and dark themes.
- App name, publisher, and version come from canonical application metadata.
- Restart Manager detection is read-only; the user can retry after closing blockers.
- Cancellation is cooperative at safe transaction boundaries.
- Windows screen-reader support is limited by the current GPUI platform stack; semantic roles and keyboard operation are still required.

## Product Principles

- A single lifecycle engine serves graphical and headless entry points.
- Elevation belongs to the authenticated worker, not the interface process.
- Operations report measurable progress across the complete plan.
- Ownership drift remains visible and does not trigger destructive cleanup.
