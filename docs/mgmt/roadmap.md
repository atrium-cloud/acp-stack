# Roadmap

This roadmap is a planning document for maintainers. Product contracts live under [../specs](../specs).

v0.1.0 shipped the initial release scope: local runtime, trust layer, agent catalog, MCP, logs and metrics, packaging, and operations. The release criteria for that scope are retired.

## Later Scope

The following remain out of scope:

- multiple targets sharing one harness (Array ships multi-target but requires a distinct harness per target)
- broad cross-distro package/runtime reconciliation
- complete OS-level interception of arbitrary shell activity
- built-in TLS termination or advanced WAF policy
- snapshots and hibernation
- hosted fleet management
- billing and tenant management
