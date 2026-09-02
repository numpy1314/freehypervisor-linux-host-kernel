// SPDX-License-Identifier: GPL-2.0

use core::pin::Pin;
use kernel::pr_info;

type VendorAxvisorCoreBootEntry = fn();

unsafe extern "C" {
    fn axvisor_linux_bridge_boot_run();
}

kernel::sync::global_lock! {
    // SAFETY: Boot bridge registration is initialized on first use.
    unsafe(uninit) static VENDOR_AXVISOR_CORE_BOOT_ENTRY: Mutex<Option<VendorAxvisorCoreBootEntry>> = None;
}

/// Final `vendor::axvisor_core::boot` bridge.
///
/// Current role:
/// - keep a single Linux-side replacement point for the future real
///   `axvisor_core::boot::run()`
/// - avoid spreading crate-visibility logic into outer adapter layers
///
/// Current behavior:
/// - remains a module-local placeholder until the real core crate is linked
pub(crate) fn run() {
    ensure_boot_bridge_init();
    let entry = vendor_axvisor_core_boot_entry();
    entry();
}

pub(crate) fn register_boot_entry(entry: VendorAxvisorCoreBootEntry) {
    ensure_boot_bridge_init();
    *VENDOR_AXVISOR_CORE_BOOT_ENTRY.lock() = Some(entry);
}

pub(crate) fn boot_entry_registered() -> bool {
    ensure_boot_bridge_init();
    VENDOR_AXVISOR_CORE_BOOT_ENTRY.lock().is_some() || bridge_boot_entry_available()
}

fn ensure_boot_bridge_init() {
    // SAFETY: global lock init is idempotent during module lifetime.
    unsafe {
        VENDOR_AXVISOR_CORE_BOOT_ENTRY.init();
    }
}

fn vendor_axvisor_core_boot_entry() -> VendorAxvisorCoreBootEntry {
    VENDOR_AXVISOR_CORE_BOOT_ENTRY
        .lock()
        .as_ref()
        .copied()
        .unwrap_or(vendor_axvisor_core_boot_bridge_entry)
}

fn vendor_axvisor_core_boot_bridge_entry() {
    // SAFETY: The target-side bridge exports a stable C ABI wrapper around
    // `axvisor_core::boot::run()`. If it is not linked yet, modpost/link will
    // fail and this path will never be callable at runtime.
    unsafe {
        axvisor_linux_bridge_boot_run();
    }
}

const fn bridge_boot_entry_available() -> bool {
    true
}

fn vendor_axvisor_core_boot_stub() {
    pr_info!("axvisor_adapter: vendor::axvisor_core::boot stub invoked\n");
}
