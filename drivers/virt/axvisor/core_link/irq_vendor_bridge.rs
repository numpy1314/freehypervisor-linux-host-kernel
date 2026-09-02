// SPDX-License-Identifier: GPL-2.0

use super::super::{ExternalIrqEvent, vendor};
use kernel::pr_info;

fn host_emerg(msg: &str) {
    unsafe {
        super::super::axvisor_linux_host_emerg_write_bytes(msg.as_ptr(), msg.len());
        super::super::axvisor_linux_host_emerg_write_bytes(b"\n".as_ptr(), 1);
    }
}

type ExternalIrqVendorBridgeEntry = fn(ExternalIrqEvent) -> bool;

/// Vendor/core visibility bridge for the Linux-side AxVisor external IRQ entry.
///
/// Intended future role:
/// - keep Linux external IRQ injection independent of ambient task context by
///   targeting the VM carried in `ExternalIrqEvent`
/// - isolate crate-visibility concerns from `core_link::irq`
///
/// Current role:
/// - route external IRQ injection into the dedicated `vendor::axvisor_core` bridge
pub(crate) fn inject_external_interrupt(event: ExternalIrqEvent) -> bool {
    host_emerg("irq_vendor_bridge::dispatch");
    pr_info!(
        "axvisor_adapter: irq_vendor_bridge dispatch vector={} irq_id={} vm_id={} cpu_id={} call_index={}\n",
        event.vector,
        event.irq_id,
        event.vm_id,
        event.cpu_id,
        event.call_index
    );
    let entry = external_irq_vendor_bridge_entry();
    entry(event)
}

fn external_irq_vendor_bridge_entry() -> ExternalIrqVendorBridgeEntry {
    let _target: fn(usize, usize) -> bool = vendor::axvisor_core::arch::riscv64::inject_interrupt;
    vendor_inject_external_interrupt
}

fn vendor_inject_external_interrupt(event: ExternalIrqEvent) -> bool {
    if event.irq_id == 0 {
        host_emerg("irq_vendor_bridge::skip_empty_irq");
        pr_info!(
            "axvisor_adapter: irq_vendor_bridge skip empty external event vector={} cpu_id={} call_index={}\n",
            event.vector,
            event.cpu_id,
            event.call_index
        );
        return false;
    }
    if event.vm_id == usize::MAX {
        host_emerg("irq_vendor_bridge::skip_no_vcpu_context");
        pr_info!(
            "axvisor_adapter: irq_vendor_bridge skip external event without vCPU context vector={} irq_id={} cpu_id={} call_index={}\n",
            event.vector,
            event.irq_id,
            event.cpu_id,
            event.call_index
        );
        return false;
    }

    let injected = vendor::axvisor_core::arch::riscv64::inject_interrupt(event.vm_id, event.irq_id);
    if injected {
        host_emerg("irq_vendor_bridge::injected");
    } else {
        host_emerg("irq_vendor_bridge::inject_failed");
    }
    pr_info!(
        "axvisor_adapter: irq_vendor_bridge injected vm_id={} irq_id={} result={}\n",
        event.vm_id,
        event.irq_id,
        injected
    );
    injected
}
