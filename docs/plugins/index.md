# Plugins

A Zup plugin is a WebAssembly Component Model planner extension.

The installer asks the plugin for additional declarative resources while building the installation plan. The plugin returns resources; Zup owns their normal install, repair, upgrade and uninstall lifecycle.

Use a plugin when a resource must be computed from planning context. Keep static resources in `zup.toml`.

## Good plugin jobs

- generate a configuration file from app version, target or selected components;
- declare launchers or services that depend on component selection;
- derive protocol/file-association resources from plugin-owned logic.

## Poor plugin jobs

- copy a fixed file already known to the project;
- run arbitrary install scripts;
- mutate the target machine directly;
- duplicate resources that the manifest can already declare.

## Contract

The public ABI is `zup:plugin@1.0.0`. A plugin exports one planner function:

```wit
plan: func(context: context) -> result<installation-plan, plugin-error>;
```

Rust is one authoring option. The contract is WIT, so another Component Model toolchain can implement the same world.

Follow [Create a plugin](./create), then [Configure a plugin](./configure).
