// SPDX-License-Identifier: GPL-2.0

use super::super::{AxvisorTimerEventContext, vendor};
use kernel::pr_info;

type TimerVendorBridgeEntry = fn(AxvisorTimerEventContext) -> bool;

/// Vendor/core visibility bridge for the Linux-side AxVisor timer entry.
///
/// Intended future role:
/// - become the first file on the timer path that switches from local fallback
///   to real `axvisor_core::vmm::timer::check_events()`
/// - isolate crate-visibility concerns from `core_link::timer`
///
/// Current role:
/// - route timer event polling into the dedicated `vendor::axvisor_core` bridge
pub(crate) fn timer_check_events(event: AxvisorTimerEventContext) -> bool {
    pr_info!(
        "axvisor_adapter: timer_vendor_bridge dispatch cpu_id={} deadline_nanos={} fire_count={}\n",
        event.cpu_id,
        event.deadline_nanos,
        event.fire_count
    );
    let entry = timer_vendor_bridge_entry();
    entry(event)
}

fn timer_vendor_bridge_entry() -> TimerVendorBridgeEntry {
    let _target: fn() -> bool = vendor::axvisor_core::vmm::timer::check_events;
    vendor_timer_check_events
}

fn vendor_timer_check_events(_event: AxvisorTimerEventContext) -> bool {
    vendor::axvisor_core::vmm::timer::check_events()
}
