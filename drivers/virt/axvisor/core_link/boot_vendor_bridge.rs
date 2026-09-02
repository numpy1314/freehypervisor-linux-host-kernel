// SPDX-License-Identifier: GPL-2.0

use super::{boot_axvisor_core_entry, super::AxvisorRuntimeStartContext};
use kernel::pr_info;

type BootVendorBridgeEntry = fn(AxvisorRuntimeStartContext);

/// Vendor/core visibility bridge for the Linux-side AxVisor boot entry.
///
/// Intended future role:
/// - become the first file that directly imports and calls `axvisor_core::boot::run()`
/// - isolate crate-visibility and vendoring concerns from `boot.rs`
///
/// Current role:
/// - route the adapter boot path into the dedicated `vendor::axvisor_core` bridge
/// - keep crate-visibility concerns isolated from outer glue
pub(crate) fn boot_run(ctx: AxvisorRuntimeStartContext) {
    pr_info!(
        "axvisor_adapter: boot_vendor_bridge dispatch host_cpu_num={} current_cpu_id={} run_call_index={}\n",
        ctx.host_cpu_num,
        ctx.current_cpu_id,
        ctx.run_call_index
    );
    let entry = boot_vendor_bridge_entry();
    entry(ctx);
}

fn boot_vendor_bridge_entry() -> BootVendorBridgeEntry {
    boot_axvisor_core_entry::boot_run
}
