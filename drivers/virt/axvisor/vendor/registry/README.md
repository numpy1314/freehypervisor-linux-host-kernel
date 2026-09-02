# AxVisor Registry Crate Staging

This directory is reserved for source snapshots copied from the local Cargo
registry cache, i.e. crates that are not part of the upstream tgoskits
workspace.

Current rule:

- `vendor/upstream/*` stores tgoskits workspace crates
- `vendor/registry/*` stores crates.io-derived dependencies

This keeps two different provenance classes separate:

1. upstream hypervisor workspace crates
2. third-party registry crates
