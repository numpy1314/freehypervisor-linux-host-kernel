# AxVisor Upstream Vendor Staging

This directory is reserved for upstream crate source snapshots that will be
brought into the Linux-side AxVisor adapter in phases.

Current intent:

- keep Linux-side bridge modules under `vendor/axvisor_core` and
  `vendor/axvisor_api`
- keep upstream crate source snapshots under `vendor/upstream`
- avoid mixing Linux glue code with copied upstream crate sources

The first-round candidate crate set is documented in:

- `/home/bullet1517/freehypervisor/docs/axvisor-linux-upstream-vendor-layout.md`
