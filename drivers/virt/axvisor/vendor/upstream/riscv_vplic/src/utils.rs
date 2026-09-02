//! MMIO utility functions.
//!
//! Internal helper functions for performing memory-mapped I/O operations.

use core::result::Result::Ok;

use ax_errno::AxResult;
#[cfg(not(axvisor_host_riscv64))]
use axaddrspace::{HostPhysAddr, device::AccessWidth};

#[cfg(axvisor_host_riscv64)]
unsafe extern "Rust" {
    fn axvisor_linux_bridge_complete_passthrough_irq(irq_id: usize);
}

/// Performs a volatile MMIO read operation.
#[cfg(not(axvisor_host_riscv64))]
pub(crate) fn perform_mmio_read(addr: HostPhysAddr, width: AccessWidth) -> AxResult<usize> {
    let addr = axvisor_api::memory::phys_to_virt(addr).as_ptr();

    match width {
        AccessWidth::Byte => Ok(unsafe { addr.read_volatile() as _ }),
        AccessWidth::Word => Ok(unsafe { (addr as *const u16).read_volatile() as _ }),
        AccessWidth::Dword => Ok(unsafe { (addr as *const u32).read_volatile() as _ }),
        AccessWidth::Qword => Ok(unsafe { (addr as *const u64).read_volatile() as _ }),
    }
}

/// Performs a volatile MMIO write operation.
#[cfg(not(axvisor_host_riscv64))]
pub(crate) fn perform_mmio_write(
    addr: HostPhysAddr,
    width: AccessWidth,
    val: usize,
) -> AxResult<()> {
    let addr = axvisor_api::memory::phys_to_virt(addr).as_mut_ptr();

    match width {
        AccessWidth::Byte => unsafe {
            addr.write_volatile(val as _);
        },
        AccessWidth::Word => unsafe {
            (addr as *mut u16).write_volatile(val as _);
        },
        AccessWidth::Dword => unsafe {
            (addr as *mut u32).write_volatile(val as _);
        },
        AccessWidth::Qword => unsafe {
            (addr as *mut u64).write_volatile(val as _);
        },
    }

    Ok(())
}

#[cfg(axvisor_host_riscv64)]
pub(crate) fn complete_passthrough_irq(irq_id: usize) -> AxResult<()> {
    unsafe { axvisor_linux_bridge_complete_passthrough_irq(irq_id) };
    Ok(())
}

#[cfg(not(axvisor_host_riscv64))]
pub(crate) fn complete_passthrough_irq(irq_id: usize) -> AxResult<()> {
    let _ = irq_id;
    Ok(())
}
