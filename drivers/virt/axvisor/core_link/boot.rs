// SPDX-License-Identifier: GPL-2.0

use super::{boot_vendor_bridge, super::AxvisorRuntimeStartContext};
use kernel::pr_info;

type BootCoreInvoker = fn(AxvisorRuntimeStartContext);

/// First-stage Linux-side core boot link.
///
/// Current role:
/// - move the runtime core entry target out of `axvisor_adapter_main.rs`
/// - keep a stable replacement point for future real `axvisor_core::boot::run()` wiring
///
/// Current implementation:
/// - forwards through the vendor bridge into `vendor::axvisor_core::boot::run`
///
/// Future target:
/// - real `axvisor_core::boot::run()` wrapper
pub(crate) fn boot_run(ctx: AxvisorRuntimeStartContext) {
    prepare_boot_run(ctx);
    invoke_boot_run_core(ctx);
    finalize_boot_run(ctx);
}

fn prepare_boot_run(_ctx: AxvisorRuntimeStartContext) {
    // Future target: last host-side preparation before entering `axvisor_core::boot::run()`.
}

fn invoke_boot_run_core(ctx: AxvisorRuntimeStartContext) {
    let invoker = boot_core_invoker();
    invoker(ctx);
}

fn finalize_boot_run(_ctx: AxvisorRuntimeStartContext) {
    // Future target: post-run cleanup if the core boot entry ever returns.
}

fn boot_core_invoker() -> BootCoreInvoker {
    real_boot_run_bridge
}

fn real_boot_run_bridge(ctx: AxvisorRuntimeStartContext) {
    pr_info!(
        "axvisor_adapter: core_link::boot entering vendor bridge host_cpu_num={} current_cpu_id={} run_call_index={}\n",
        ctx.host_cpu_num,
        ctx.current_cpu_id,
        ctx.run_call_index
    );
    boot_vendor_bridge::boot_run(ctx);
}
