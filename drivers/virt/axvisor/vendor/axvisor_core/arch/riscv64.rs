// SPDX-License-Identifier: GPL-2.0

use core::pin::Pin;

type VendorAxvisorCoreExternalIrqEntry = fn(usize) -> bool;

unsafe extern "C" {
    fn axvisor_linux_bridge_inject_current_interrupt(irq_id: usize) -> bool;
    fn axvisor_linux_bridge_inject_interrupt(vm_id: usize, irq_id: usize) -> bool;
    fn axvisor_linux_bridge_current_vm_id() -> usize;
}

kernel::sync::global_lock! {
    // SAFETY: IRQ bridge registration is initialized on first use.
    unsafe(uninit) static VENDOR_AXVISOR_CORE_EXTERNAL_IRQ_ENTRY: Mutex<Option<VendorAxvisorCoreExternalIrqEntry>> = None;
}

/// Final `vendor::axvisor_core::arch::riscv64` bridge.
///
/// Current role:
/// - keep guest external interrupt injection wiring concentrated at one function
/// - invoke the real `axvisor_core::arch::riscv64::inject_current_interrupt(irq_id)`
pub(crate) fn inject_current_interrupt(irq_id: usize) -> bool {
    ensure_external_irq_bridge_init();
    let entry = vendor_axvisor_core_external_irq_entry();
    entry(irq_id)
}

pub(crate) fn inject_interrupt(vm_id: usize, irq_id: usize) -> bool {
    unsafe { axvisor_linux_bridge_inject_interrupt(vm_id, irq_id) }
}

pub(crate) fn current_vm_id() -> Option<usize> {
    let vm_id = unsafe { axvisor_linux_bridge_current_vm_id() };
    (vm_id != usize::MAX).then_some(vm_id)
}

pub(crate) fn register_external_irq_entry(entry: VendorAxvisorCoreExternalIrqEntry) {
    ensure_external_irq_bridge_init();
    *VENDOR_AXVISOR_CORE_EXTERNAL_IRQ_ENTRY.lock() = Some(entry);
}

pub(crate) fn external_irq_entry_registered() -> bool {
    ensure_external_irq_bridge_init();
    VENDOR_AXVISOR_CORE_EXTERNAL_IRQ_ENTRY.lock().is_some() || bridge_external_irq_entry_available()
}

fn ensure_external_irq_bridge_init() {
    // SAFETY: global lock init is idempotent during module lifetime.
    unsafe {
        VENDOR_AXVISOR_CORE_EXTERNAL_IRQ_ENTRY.init();
    }
}

fn vendor_axvisor_core_external_irq_entry() -> VendorAxvisorCoreExternalIrqEntry {
    VENDOR_AXVISOR_CORE_EXTERNAL_IRQ_ENTRY
        .lock()
        .as_ref()
        .copied()
        .unwrap_or(vendor_axvisor_core_external_irq_bridge_entry)
}

fn vendor_axvisor_core_external_irq_bridge_entry(irq_id: usize) -> bool {
    // SAFETY: The target-side bridge exports a stable C ABI wrapper around
    // `axvisor_core::arch::riscv64::inject_current_interrupt()`.
    unsafe { axvisor_linux_bridge_inject_current_interrupt(irq_id) }
}

const fn bridge_external_irq_entry_available() -> bool {
    true
}

fn vendor_axvisor_core_external_irq_stub(_irq_id: usize) -> bool {
    false
}
