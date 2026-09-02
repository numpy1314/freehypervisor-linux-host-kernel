//! Device emulation operations for VPlicGlobal.
//!
//! Implements the `BaseDeviceOps` trait for MMIO read/write handling.

use alloc::format;
use core::sync::atomic::{AtomicUsize, Ordering};

use ax_errno::AxResult;
use axaddrspace::{GuestPhysAddrRange, device::AccessWidth};
#[cfg(not(axvisor_host_riscv64))]
use axaddrspace::HostPhysAddr;
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use bitmaps::Bitmap;

use crate::{consts::*, utils::*, vplic::VPlicGlobal};

const VCAUSE_INTERRUPT_BIT: usize = 1usize << (usize::BITS - 1);
const VCAUSE_VS_TIMER: usize = VCAUSE_INTERRUPT_BIT | 5;
const PLIC_PENDING_WORDS: usize = PLIC_NUM_SOURCES / 32;
static VPLIC_TRACE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn host_emerg_line(msg: &str) {
    axvisor_api::host::emerg_write_bytes(msg.as_bytes());
    axvisor_api::host::emerg_write_bytes(b"\n");
}

fn trace_vplic(msg: alloc::string::String) {
    let count = VPLIC_TRACE_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 64 || count.is_power_of_two() {
        host_emerg_line(&msg);
    }
}

impl VPlicGlobal {
    /// Reads the priority of an interrupt source from the host PLIC.
    fn irq_priority(&self, irq_id: usize) -> AxResult<u32> {
        #[cfg(axvisor_host_riscv64)]
        {
            return Ok(self.priorities.lock()[irq_id]);
        }

        #[cfg(not(axvisor_host_riscv64))]
        {
            let addr = HostPhysAddr::from_usize(
                self.host_plic_addr.as_usize() + PLIC_PRIORITY_OFFSET + irq_id * 4,
            );
            Ok(perform_mmio_read(addr, AccessWidth::Dword)? as u32)
        }
    }

    /// Reads the priority threshold configured for a PLIC context.
    fn context_threshold(&self, context_id: usize) -> AxResult<u32> {
        #[cfg(axvisor_host_riscv64)]
        {
            return Ok(self.thresholds.lock()[context_id]);
        }

        #[cfg(not(axvisor_host_riscv64))]
        {
            let addr = HostPhysAddr::from_usize(
                self.host_plic_addr.as_usize()
                    + PLIC_CONTEXT_CTRL_OFFSET
                    + context_id * PLIC_CONTEXT_STRIDE
                    + PLIC_CONTEXT_THRESHOLD_OFFSET,
            );
            Ok(perform_mmio_read(addr, AccessWidth::Dword)? as u32)
        }
    }

    /// Reads one enable register word for a PLIC context.
    fn context_enable_mask(&self, context_id: usize, reg_index: usize) -> AxResult<u32> {
        #[cfg(axvisor_host_riscv64)]
        {
            return Ok(self.enable_masks.lock()[context_id][reg_index]);
        }

        #[cfg(not(axvisor_host_riscv64))]
        {
            let addr = HostPhysAddr::from_usize(
                self.host_plic_addr.as_usize()
                    + PLIC_ENABLE_OFFSET
                    + context_id * PLIC_ENABLE_STRIDE
                    + reg_index * 4,
            );
            Ok(perform_mmio_read(addr, AccessWidth::Dword)? as u32)
        }
    }

    /// Returns pending interrupts that are not currently in service.
    fn pending_inactive_irqs(&self) -> Bitmap<{ PLIC_NUM_SOURCES }> {
        let pending_irqs = self.pending_irqs.lock();
        let active_irqs = self.active_irqs.lock();
        let mut candidates = *pending_irqs & !*active_irqs;
        // IRQ 0 is reserved by the PLIC specification and must never be claimed.
        candidates.set(0, false);
        candidates
    }

    /// Selects the highest-priority enabled IRQ from the candidate set.
    fn best_enabled_pending_irq(
        &self,
        context_id: usize,
        candidate_irqs: Bitmap<{ PLIC_NUM_SOURCES }>,
    ) -> AxResult<Option<(usize, u32)>> {
        let mut best_irq = None;
        let mut best_priority = 0;
        let mut cached_enable_reg_index = usize::MAX;
        let mut cached_enable_mask = 0u32;

        // Select the highest-priority IRQ that is pending, inactive, and
        // enabled for this context. Threshold filtering is applied separately
        // for interrupt notification, but not for claim.
        for irq_id in (&candidate_irqs).into_iter() {
            let reg_index = irq_id / 32;
            let bit_index = irq_id % 32;

            if reg_index != cached_enable_reg_index {
                cached_enable_mask = self.context_enable_mask(context_id, reg_index)?;
                cached_enable_reg_index = reg_index;
            }
            if (cached_enable_mask & (1 << bit_index)) == 0 {
                continue;
            }

            let priority = self.irq_priority(irq_id)?;
            if priority > best_priority {
                best_priority = priority;
                best_irq = Some((irq_id, priority));
            }
        }

        Ok(best_irq)
    }

    /// Returns the next IRQ that should assert VSEIP for this context.
    fn next_deliverable_irq(&self, context_id: usize) -> AxResult<Option<usize>> {
        let threshold = self.context_threshold(context_id)?;
        let candidate_irqs = self.pending_inactive_irqs();
        if let Some((irq_id, priority)) =
            self.best_enabled_pending_irq(context_id, candidate_irqs)?
        {
            if priority > threshold {
                return Ok(Some(irq_id));
            }
        }
        Ok(None)
    }

    /// Claims the next enabled pending IRQ and moves it to the active set.
    fn claim_next_irq(&self, context_id: usize) -> AxResult<Option<usize>> {
        loop {
            let candidate_irqs = self.pending_inactive_irqs();
            let Some((irq_id, _priority)) =
                self.best_enabled_pending_irq(context_id, candidate_irqs)?
            else {
                return Ok(None);
            };

            let mut pending_irqs = self.pending_irqs.lock();
            let mut active_irqs = self.active_irqs.lock();
            if !pending_irqs.get(irq_id) || active_irqs.get(irq_id) {
                continue;
            }

            // Claim moves the IRQ from pending to active until the guest
            // writes it back to the complete register.
            pending_irqs.set(irq_id, false);
            active_irqs.set(irq_id, true);
            return Ok(Some(irq_id));
        }
    }

    /// Recomputes whether VSEIP should remain asserted for one context.
    fn sync_vseip(&self, context_id: usize) -> AxResult<()> {
        // VSEIP should track whether this context still has a deliverable
        // external interrupt, not merely whether some pending bit is set.
        if self.next_deliverable_irq(context_id)?.is_some() {
            unsafe {
                // If the guest is already executing a VS timer interrupt handler,
                // the corresponding tick is "in service" from the guest's point of
                // view. Clearing VSTIP here avoids needlessly keeping a timer
                // interrupt pending while we queue the external interrupt.
                if riscv_h::register::vscause::read().bits() == VCAUSE_VS_TIMER {
                    riscv_h::register::hvip::clear_vstip();
                }
                riscv_h::register::hvip::set_vseip();
            }
        } else {
            unsafe {
                riscv_h::register::hvip::clear_vseip();
            }
        }
        Ok(())
    }

    /// Recomputes VSEIP for all guest supervisor contexts.
    fn sync_all_guest_contexts_vseip(&self) -> AxResult<()> {
        for context_id in (1..self.contexts_num).step_by(2) {
            self.sync_vseip(context_id)?;
        }
        Ok(())
    }
}

/// Implementation of device emulation operations for virtual PLIC.
impl BaseDeviceOps<GuestPhysAddrRange> for VPlicGlobal {
    fn emu_type(&self) -> axdevice_base::EmuDeviceType {
        EmuDeviceType::PPPTGlobal
    }

    fn address_range(&self) -> GuestPhysAddrRange {
        GuestPhysAddrRange::from_start_size(self.addr, self.size)
    }

    /// Handles MMIO read operations from the virtual PLIC.
    ///
    /// Only 32-bit (Dword) accesses are supported.
    /// Read operations are forwarded to the host PLIC for most registers,
    /// except for pending and claim/complete registers which are emulated.
    fn handle_read(
        &self,
        addr: <GuestPhysAddrRange as axaddrspace::device::DeviceAddrRange>::Addr,
        width: axaddrspace::device::AccessWidth,
    ) -> ax_errno::AxResult<usize> {
        assert_eq!(width, AccessWidth::Dword);
        let reg = addr - self.addr;
        // info!("vPlicGlobal read reg {reg:#x} width {width:?}");
        match reg {
            // priority
            offset if (PLIC_PRIORITY_OFFSET..PLIC_PENDING_OFFSET).contains(&offset) => {
                #[cfg(axvisor_host_riscv64)]
                {
                    let irq_id = (offset - PLIC_PRIORITY_OFFSET) / 4;
                    if irq_id >= PLIC_NUM_SOURCES {
                        return Ok(0);
                    }
                    Ok(self.priorities.lock()[irq_id] as usize)
                }
                #[cfg(not(axvisor_host_riscv64))]
                {
                    let host_addr =
                        HostPhysAddr::from_usize(reg + self.host_plic_addr.as_usize());
                    perform_mmio_read(host_addr, width)
                }
            }
            // pending
            offset if (PLIC_PENDING_OFFSET..PLIC_ENABLE_OFFSET).contains(&offset) => {
                let reg_index = (reg - PLIC_PENDING_OFFSET) / 4;
                if reg_index >= PLIC_PENDING_WORDS {
                    return Ok(0);
                }
                let bit_index_start = reg_index * 32;
                let mut val: u32 = 0;
                let mut bit_mask: u32 = 1;
                let pending_irqs = self.pending_irqs.lock();
                for i in 0..32 {
                    let irq_id = bit_index_start + i as usize;
                    if irq_id != 0 && pending_irqs.get(irq_id) {
                        val |= bit_mask;
                    }
                    bit_mask <<= 1;
                }
                Ok(val as usize)
            }
            // enable
            offset if (PLIC_ENABLE_OFFSET..PLIC_CONTEXT_CTRL_OFFSET).contains(&offset) => {
                #[cfg(axvisor_host_riscv64)]
                {
                    let context_id = (offset - PLIC_ENABLE_OFFSET) / PLIC_ENABLE_STRIDE;
                    let reg_index =
                        ((offset - PLIC_ENABLE_OFFSET) % PLIC_ENABLE_STRIDE) / 4;
                    if context_id >= self.contexts_num || reg_index >= PLIC_PENDING_WORDS {
                        return Ok(0);
                    }
                    Ok(self.enable_masks.lock()[context_id][reg_index] as usize)
                }
                #[cfg(not(axvisor_host_riscv64))]
                {
                    let host_addr =
                        HostPhysAddr::from_usize(reg + self.host_plic_addr.as_usize());
                    perform_mmio_read(host_addr, width)
                }
            }
            // threshold
            offset
                if offset >= PLIC_CONTEXT_CTRL_OFFSET
                    && (offset - PLIC_CONTEXT_CTRL_OFFSET) % PLIC_CONTEXT_STRIDE == 0 =>
            {
                #[cfg(axvisor_host_riscv64)]
                {
                    let context_id = (offset - PLIC_CONTEXT_CTRL_OFFSET) / PLIC_CONTEXT_STRIDE;
                    if context_id >= self.contexts_num {
                        return Ok(0);
                    }
                    Ok(self.thresholds.lock()[context_id] as usize)
                }
                #[cfg(not(axvisor_host_riscv64))]
                {
                    let host_addr =
                        HostPhysAddr::from_usize(reg + self.host_plic_addr.as_usize());
                    perform_mmio_read(host_addr, width)
                }
            }
            // claim/complete
            offset
                if offset >= PLIC_CONTEXT_CTRL_OFFSET
                    && (offset - PLIC_CONTEXT_CTRL_OFFSET - PLIC_CONTEXT_CLAIM_COMPLETE_OFFSET)
                        % PLIC_CONTEXT_STRIDE
                        == 0 =>
            {
                let context_id =
                    (offset - PLIC_CONTEXT_CTRL_OFFSET - PLIC_CONTEXT_CLAIM_COMPLETE_OFFSET)
                        / PLIC_CONTEXT_STRIDE;
                assert!(
                    context_id < self.contexts_num,
                    "Invalid context id {context_id}"
                );
                let Some(irq_id) = self.claim_next_irq(context_id)? else {
                    #[cfg(axvisor_host_riscv64)]
                    trace_vplic(format!(
                        "riscv_vplic::claim_empty context={} reg={:#x}",
                        context_id, reg
                    ));
                    self.sync_vseip(context_id)?;
                    return Ok(0);
                };
                #[cfg(axvisor_host_riscv64)]
                trace_vplic(format!(
                    "riscv_vplic::claim context={} irq={} reg={:#x}",
                    context_id, irq_id, reg
                ));
                self.sync_vseip(context_id)?;
                Ok(irq_id)
            }
            _ => {
                unimplemented!("Unsupported vPlicGlobal read for reg {reg:#x}")
            }
        }
    }

    /// Handles MMIO write operations to the virtual PLIC.
    ///
    /// Only 32-bit (Dword) accesses are supported.
    /// Write operations are forwarded to the host PLIC for most registers.
    /// Writes to the pending register are used for interrupt injection by the hypervisor.
    /// Writes to the claim/complete register complete interrupt handling.
    fn handle_write(
        &self,
        addr: <GuestPhysAddrRange as axaddrspace::device::DeviceAddrRange>::Addr,
        width: axaddrspace::device::AccessWidth,
        val: usize,
    ) -> ax_errno::AxResult {
        assert_eq!(width, AccessWidth::Dword);
        let reg = addr - self.addr;
        // info!("vPlicGlobal write reg {reg:#x} width {width:?} val {val:#x}");
        match reg {
            // priority
            offset if (PLIC_PRIORITY_OFFSET..PLIC_PENDING_OFFSET).contains(&offset) => {
                #[cfg(axvisor_host_riscv64)]
                {
                    let irq_id = (offset - PLIC_PRIORITY_OFFSET) / 4;
                    if irq_id >= PLIC_NUM_SOURCES {
                        return Ok(());
                    }
                    if irq_id != 0 {
                        self.priorities.lock()[irq_id] = val as u32;
                    }
                    return self.sync_all_guest_contexts_vseip();
                }
                #[cfg(not(axvisor_host_riscv64))]
                {
                    let host_addr =
                        HostPhysAddr::from_usize(reg + self.host_plic_addr.as_usize());
                    perform_mmio_write(host_addr, width, val)?;
                    self.sync_all_guest_contexts_vseip()
                }
            }
            // pending (Here is uesd for hyperivosr to inject pending IRQs, later should move it to a separate interface)
            offset if (PLIC_PENDING_OFFSET..PLIC_ENABLE_OFFSET).contains(&offset) => {
                // Note: here append, not overwrite.
                let reg_index = (reg - PLIC_PENDING_OFFSET) / 4;
                if reg_index >= PLIC_PENDING_WORDS {
                    return Ok(());
                }
                let val = val as u32;
                let mut bit_mask: u32 = 1;
                let mut pending_irqs = self.pending_irqs.lock();
                for i in 0..32 {
                    if (val & bit_mask) != 0 {
                        let irq_id = reg_index * 32 + i;
                        if irq_id != 0 {
                            // Set the pending bit.
                            pending_irqs.set(irq_id, true);
                            #[cfg(axvisor_host_riscv64)]
                            if irq_id == 8 {
                                trace_vplic(format!(
                                    "riscv_vplic::pending_set irq={} reg_index={} val={:#x}",
                                    irq_id, reg_index, val
                                ));
                            }
                        }
                    }
                    bit_mask <<= 1;
                }

                drop(pending_irqs);
                self.sync_all_guest_contexts_vseip()
            }
            // enable
            offset if (PLIC_ENABLE_OFFSET..PLIC_CONTEXT_CTRL_OFFSET).contains(&offset) => {
                let context_id = (reg - PLIC_ENABLE_OFFSET) / PLIC_ENABLE_STRIDE;
                assert!(
                    context_id < self.contexts_num,
                    "Invalid context id {context_id}"
                );
                #[cfg(axvisor_host_riscv64)]
                {
                    let reg_index = ((reg - PLIC_ENABLE_OFFSET) % PLIC_ENABLE_STRIDE) / 4;
                    if reg_index >= PLIC_PENDING_WORDS {
                        return Ok(());
                    }
                    let mut enable_masks = self.enable_masks.lock();
                    enable_masks[context_id][reg_index] = (val as u32) & !(1u32);
                    drop(enable_masks);
                    if reg_index == 0 && ((val as u32) & (1u32 << 8)) != 0 {
                        trace_vplic(format!(
                            "riscv_vplic::enable_irq8 context={} mask={:#x}",
                            context_id, val
                        ));
                    }
                }
                #[cfg(not(axvisor_host_riscv64))]
                {
                    let host_addr =
                        HostPhysAddr::from_usize(reg + self.host_plic_addr.as_usize());
                    perform_mmio_write(host_addr, width, val)?;
                }
                // A mask update can instantly expose or hide already-pending IRQs.
                self.sync_vseip(context_id)
            }
            // threshold
            offset
                if offset >= PLIC_CONTEXT_CTRL_OFFSET
                    && (offset - PLIC_CONTEXT_CTRL_OFFSET) % PLIC_CONTEXT_STRIDE == 0 =>
            {
                let context_id = (offset - PLIC_CONTEXT_CTRL_OFFSET) / PLIC_CONTEXT_STRIDE;
                assert!(
                    context_id < self.contexts_num,
                    "Invalid context id {context_id}"
                );
                #[cfg(axvisor_host_riscv64)]
                {
                    self.thresholds.lock()[context_id] = val as u32;
                }
                #[cfg(not(axvisor_host_riscv64))]
                {
                    let host_addr =
                        HostPhysAddr::from_usize(reg + self.host_plic_addr.as_usize());
                    perform_mmio_write(host_addr, width, val)?;
                }
                // Threshold changes must be reflected on the hart line immediately.
                self.sync_vseip(context_id)
            }
            // claim/complete
            offset
                if offset >= PLIC_CONTEXT_CTRL_OFFSET
                    && (offset - PLIC_CONTEXT_CTRL_OFFSET - PLIC_CONTEXT_CLAIM_COMPLETE_OFFSET)
                        % PLIC_CONTEXT_STRIDE
                        == 0 =>
            {
                // info!("vPlicGlobal: Writing to CLAIM/COMPLETE reg {reg:#x} val {val:#x}");
                let context_id =
                    (offset - PLIC_CONTEXT_CTRL_OFFSET - PLIC_CONTEXT_CLAIM_COMPLETE_OFFSET)
                        / PLIC_CONTEXT_STRIDE;
                assert!(
                    context_id < self.contexts_num,
                    "Invalid context id {context_id}"
                );
                let irq_id = val;

                if irq_id == 0 || irq_id >= PLIC_NUM_SOURCES {
                    return self.sync_vseip(context_id);
                }
                let mut active_irqs = self.active_irqs.lock();
                if !active_irqs.get(irq_id) {
                    #[cfg(axvisor_host_riscv64)]
                    trace_vplic(format!(
                        "riscv_vplic::complete_inactive context={} irq={} reg={:#x}",
                        context_id, irq_id, reg
                    ));
                    return self.sync_vseip(context_id);
                }

                #[cfg(axvisor_host_riscv64)]
                {
                    let _ = width;
                    trace_vplic(format!(
                        "riscv_vplic::complete context={} irq={} reg={:#x}",
                        context_id, irq_id, reg
                    ));
                    complete_passthrough_irq(irq_id)?;
                }
                #[cfg(not(axvisor_host_riscv64))]
                {
                    let host_addr =
                        HostPhysAddr::from_usize(reg + self.host_plic_addr.as_usize());
                    // Write host PLIC.
                    perform_mmio_write(host_addr, width, irq_id)?;
                }
                // Clear the active bit only after the completion is accepted.
                active_irqs.set(irq_id, false);
                drop(active_irqs);
                self.sync_vseip(context_id)
            }
            _ => {
                unimplemented!("Unsupported vPlicGlobal read for reg {reg:#x}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use axaddrspace::GuestPhysAddr;

    use super::*;

    #[test]
    fn pending_inactive_irqs_excludes_reserved_irq_zero() {
        let vplic = VPlicGlobal::new(GuestPhysAddr::from(0x0c00_0000), Some(0x400000), 2);

        {
            let mut pending_irqs = vplic.pending_irqs.lock();
            pending_irqs.set(0, true);
            pending_irqs.set(1, true);
        }

        let candidates = vplic.pending_inactive_irqs();

        assert!(!candidates.get(0));
        assert!(candidates.get(1));
    }
}
