// SPDX-License-Identifier: GPL-2.0

use super::{super::AxvisorTimerEventContext, timer_vendor_bridge};
use kernel::pr_info;

type TimerCoreInvoker = fn(AxvisorTimerEventContext) -> bool;

/// Linux-side timer core link.
///
/// Current role:
/// - create a dedicated replacement layer between adapter glue and future
///   `axvisor_core::vmm::timer::check_events()` wiring
///
/// Current implementation:
/// - routes through a local invoker and a dedicated vendor bridge
///
/// Future target:
/// - `vendor::axvisor_core::vmm::timer::check_events()`
pub(crate) fn timer_check_events(event: AxvisorTimerEventContext) -> bool {
    prepare_timer_check(event);
    let ret = invoke_timer_check_core(event);
    finalize_timer_check(event);
    ret
}

fn prepare_timer_check(_event: AxvisorTimerEventContext) {
    // Future target: last host-side preparation before entering AxVisor timer event logic.
}

fn invoke_timer_check_core(event: AxvisorTimerEventContext) -> bool {
    let invoker = timer_core_invoker();
    invoker(event)
}

fn finalize_timer_check(_event: AxvisorTimerEventContext) {
    // Future target: post-check cleanup if needed.
}

fn timer_core_invoker() -> TimerCoreInvoker {
    vendor_timer_bridge
}

fn vendor_timer_bridge(event: AxvisorTimerEventContext) -> bool {
    pr_info!(
        "axvisor_adapter: core_link::timer entering vendor bridge cpu_id={} deadline_nanos={} fire_count={}\n",
        event.cpu_id,
        event.deadline_nanos,
        event.fire_count
    );
    timer_vendor_bridge::timer_check_events(event)
}
