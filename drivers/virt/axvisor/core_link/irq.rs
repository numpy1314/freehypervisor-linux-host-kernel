// SPDX-License-Identifier: GPL-2.0

use super::{super::ExternalIrqEvent, irq_vendor_bridge};
use kernel::pr_info;

fn host_emerg(msg: &str) {
    unsafe {
        super::super::axvisor_linux_host_emerg_write_bytes(msg.as_ptr(), msg.len());
        super::super::axvisor_linux_host_emerg_write_bytes(b"\n".as_ptr(), 1);
    }
}

type ExternalIrqCoreInvoker = fn(ExternalIrqEvent) -> bool;

/// Linux-side external IRQ core link.
///
/// Current role:
/// - create a dedicated replacement layer between adapter glue and future
///   guest interrupt injection wiring
///
/// Current implementation:
/// - routes through a local invoker and a dedicated vendor bridge
///
/// Future target:
/// - explicit-VM RISC-V external interrupt injection.
pub(crate) fn inject_external_interrupt(event: ExternalIrqEvent) -> bool {
    prepare_external_irq_inject(event);
    let ret = invoke_external_irq_core(event);
    finalize_external_irq_inject(event);
    ret
}

fn prepare_external_irq_inject(_event: ExternalIrqEvent) {
    // Future target: last host-side preparation before guest interrupt injection.
}

fn invoke_external_irq_core(event: ExternalIrqEvent) -> bool {
    let invoker = external_irq_core_invoker();
    invoker(event)
}

fn finalize_external_irq_inject(_event: ExternalIrqEvent) {
    // Future target: post-injection cleanup if needed.
}

fn external_irq_core_invoker() -> ExternalIrqCoreInvoker {
    vendor_external_irq_bridge
}

fn vendor_external_irq_bridge(event: ExternalIrqEvent) -> bool {
    host_emerg("core_link::irq vendor_bridge enter");
    pr_info!(
        "axvisor_adapter: core_link::irq entering vendor bridge vector={} irq_id={} vm_id={} cpu_id={} call_index={}\n",
        event.vector,
        event.irq_id,
        event.vm_id,
        event.cpu_id,
        event.call_index
    );
    irq_vendor_bridge::inject_external_interrupt(event)
}
