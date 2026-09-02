# AxVisor Vendor Layout

This directory intentionally contains two different layers.

## 1. Linux bridge layer

These Rust modules are Linux-side adapter glue and naming boundaries:

- `axvisor_core/`
- `axvisor_api/`

They exist to keep:

- `axvisor_adapter_main.rs`
- `core_link/`

free from direct upstream crate visibility details.

## 2. Upstream source staging layer

The `upstream/` subtree is reserved for copied or trimmed source snapshots from
the upstream tgoskits crates that will later be wired into the Linux Rust build.

Current rule:

- bridge modules stay in `vendor/axvisor_*`
- copied upstream crate sources stay in `vendor/upstream/*`

This split avoids mixing Linux glue code with upstream crate source trees.
