# Plugin constraints

Plugins are planning extensions, not install scripts.

They do not get a general filesystem, network, process or host-command API. They receive planner context and return typed resources. This keeps their output inspectable with `zup plan` before the machine is changed.

## Failures

A plugin may return a typed `plugin-error`. A failed plugin rejects planning; Zup does not keep a partial set of resources.

Runtime failures such as a trap, timeout, memory exhaustion or invalid output are reported as plugin failures rather than being converted into an incomplete plan.

## Resource limits

Current planner limits include:

| Limit | Value |
| --- | ---: |
| Resources returned by one plan | 4,096 |
| One generated file | 1 MiB |
| Total generated-file bytes | 8 MiB |
| One string | 64 KiB |
| Arguments | 256 |
| Total argument bytes | 1 MiB |
| Plugin error text | 512 bytes |

Treat these as guardrails, not targets. A plugin generating megabytes of configuration is usually the wrong abstraction.

For the exact WIT types, use the [Plugin API reference](/reference/plugin-api).
