// SPDX-License-Identifier: GPL-2.0

use super::super::{AxvisorRuntimeStartContext, vendor};

type AxvisorCoreBootEntry = fn();

/// Final intended runtime boot entry placeholder.
///
/// Intended future role:
/// - become the first Linux-side file that directly imports and calls
///   `axvisor_core::boot::run()`
/// - keep the vendor/core visibility problem isolated from the rest of the
///   adapter and from the outer `core_link::boot` flow
///
/// Current status:
/// - not yet wired into the active path
/// - kept as a dedicated replacement target for `boot_vendor_bridge_entry()`
pub(crate) fn boot_run(_ctx: AxvisorRuntimeStartContext) {
    let entry = axvisor_core_boot_entry();
    entry();
}

fn axvisor_core_boot_entry() -> AxvisorCoreBootEntry {
    // Future direct wiring target:
    //     vendor::axvisor_core::boot::run
    //
    // Current blocker:
    // - `axvisor_core` is not yet visible inside the Linux kernel Rust build.
    vendor::axvisor_core::boot::run
}
