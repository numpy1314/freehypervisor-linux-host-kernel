// SPDX-License-Identifier: GPL-2.0

use core::pin::Pin;

/// Final `vendor::axvisor_core::vmm::timer` bridge.
///
/// Current role:
/// - keep timer-event core entry wiring concentrated at one function
/// - adapt the real `axvisor_core::vmm::timer::check_events()` `()` return
///   into the adapter's boolean "core path was entered" convention
type VendorAxvisorCoreTimerEntry = fn();

unsafe extern "C" {
    fn axvisor_linux_bridge_timer_check_events();
}

kernel::sync::global_lock! {
    // SAFETY: Timer bridge registration is initialized on first use.
    unsafe(uninit) static VENDOR_AXVISOR_CORE_TIMER_ENTRY: Mutex<Option<VendorAxvisorCoreTimerEntry>> = None;
}

pub(crate) fn check_events() -> bool {
    ensure_timer_bridge_init();
    let entry = vendor_axvisor_core_timer_entry();
    entry();
    timer_entry_registered()
}

pub(crate) fn register_timer_entry(entry: VendorAxvisorCoreTimerEntry) {
    ensure_timer_bridge_init();
    *VENDOR_AXVISOR_CORE_TIMER_ENTRY.lock() = Some(entry);
}

pub(crate) fn timer_entry_registered() -> bool {
    ensure_timer_bridge_init();
    VENDOR_AXVISOR_CORE_TIMER_ENTRY.lock().is_some() || bridge_timer_entry_available()
}

fn ensure_timer_bridge_init() {
    // SAFETY: global lock init is idempotent during module lifetime.
    unsafe {
        VENDOR_AXVISOR_CORE_TIMER_ENTRY.init();
    }
}

fn vendor_axvisor_core_timer_entry() -> VendorAxvisorCoreTimerEntry {
    VENDOR_AXVISOR_CORE_TIMER_ENTRY
        .lock()
        .as_ref()
        .copied()
        .unwrap_or(vendor_axvisor_core_timer_bridge_entry)
}

fn vendor_axvisor_core_timer_bridge_entry() {
    // SAFETY: The target-side bridge exports a stable C ABI wrapper around
    // `axvisor_core::vmm::timer::check_events()`.
    unsafe {
        axvisor_linux_bridge_timer_check_events();
    }
}

const fn bridge_timer_entry_available() -> bool {
    true
}

fn vendor_axvisor_core_timer_stub() {
}
