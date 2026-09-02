// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use axaddrspace::{GuestPhysAddr, GuestVirtAddr};
use riscv::asm::hfence_vvma;
use riscv_h::register::vsatp::Vsatp;

use crate::trap::Exception;

core::arch::global_asm!(
    include_str!("mem_extable.S"),
    options(raw),
);

unsafe extern "C" {
    fn _copy_from_guest(dst: *mut u8, src: *const u8, len: usize) -> usize;
    fn _copy_to_guest(dst: *mut u8, src: *const u8, len: usize) -> usize;
    fn _fetch_guest_instruction(
        gva: *const u8,
        instruction: *mut u32,
        fault: *mut GuestInstructionFetchFaultRaw,
    ) -> usize;
}

#[inline(always)]
unsafe fn hfence_vvma_all() {
    hfence_vvma(0, 0);
}

#[derive(Debug, Default)]
#[repr(C)]
/// Raw trap CSR snapshot returned by the HLVX fetch helper on fault.
struct GuestInstructionFetchFaultRaw {
    scause: usize,
    stval: usize,
    htval: usize,
}

/// Fault categories produced while fetching a guest instruction with HLVX.
///
/// HLVX checks execute permission, but architecturally reports load-class
/// exceptions. These variants carry the guest-facing fetch semantics that the
/// vCPU code should inject or forward.
#[derive(Debug)]
pub(crate) enum GuestInstructionFetchFault {
    /// Guest VS-stage translation denied or missed the instruction address.
    PageFault { addr: GuestVirtAddr },
    /// Guest instruction access fault after translation.
    AccessFault { addr: GuestVirtAddr },
    /// Guest instruction address was misaligned.
    Misaligned { addr: GuestVirtAddr },
    /// G-stage translation fault while resolving the instruction access.
    GuestPageFault { addr: GuestPhysAddr },
    /// A trap cause that is not expected from HLVX instruction fetching.
    Unhandled {
        scause: usize,
        stval: usize,
        htval: usize,
    },
}

impl GuestInstructionFetchFaultRaw {
    fn into_fetch_fault(self, gva: GuestVirtAddr) -> GuestInstructionFetchFault {
        let exception = self.scause & !(1usize << (usize::BITS - 1));
        let fault_gva = GuestVirtAddr::from_usize(self.stval);

        match exception {
            // HLVX reports these as load faults even though the guest-visible
            // operation is an instruction fetch.
            x if x == Exception::InstructionPageFault as usize
                || x == Exception::LoadPageFault as usize =>
            {
                GuestInstructionFetchFault::PageFault { addr: fault_gva }
            }
            x if x == Exception::InstructionFault as usize
                || x == Exception::LoadFault as usize =>
            {
                GuestInstructionFetchFault::AccessFault { addr: fault_gva }
            }
            x if x == Exception::InstructionMisaligned as usize
                || x == Exception::LoadMisaligned as usize =>
            {
                GuestInstructionFetchFault::Misaligned { addr: gva }
            }
            x if x == Exception::InstructionGuestPageFault as usize
                || x == Exception::LoadGuestPageFault as usize =>
            {
                // For guest-page faults, htval holds GPA[XLEN-1:2] and stval
                // supplies the low two bits of the faulting guest physical address.
                let fault_gpa = GuestPhysAddr::from((self.htval << 2) | (self.stval & 0b11));
                GuestInstructionFetchFault::GuestPageFault { addr: fault_gpa }
            }
            _ => GuestInstructionFetchFault::Unhandled {
                scause: self.scause,
                stval: self.stval,
                htval: self.htval,
            },
        }
    }
}

/// Copies data from guest virtual address to host memory.
#[inline(always)]
pub(crate) fn copy_from_guest_va(dst: &mut [u8], gva: GuestVirtAddr) -> usize {
    if dst.is_empty() {
        return 0;
    }

    unsafe { _copy_from_guest(dst.as_mut_ptr(), gva.as_usize() as *const u8, dst.len()) }
}

/// Copies data from host memory to guest virtual address.
#[inline(always)]
pub(crate) fn copy_to_guest_va(src: &[u8], gva: GuestVirtAddr) -> usize {
    if src.is_empty() {
        return 0;
    }

    unsafe { _copy_to_guest(gva.as_usize() as *mut u8, src.as_ptr(), src.len()) }
}

/// Copies data from guest physical address to host memory.
#[inline(always)]
pub(crate) fn copy_from_guest(dst: &mut [u8], gpa: GuestPhysAddr) -> usize {
    let old_vsatp = riscv_h::register::vsatp::read().bits();
    unsafe {
        // Set vsatp to 0 to disable guest virtual address translation.
        Vsatp::from_bits(0).write();
        hfence_vvma_all();
        // Now GVA is the same as GPA.
        let ret = copy_from_guest_va(dst, GuestVirtAddr::from(gpa.as_usize()));
        // Restore the original vsatp.
        Vsatp::from_bits(old_vsatp).write();
        hfence_vvma_all();
        ret
    }
}

///  Copies data from host memory to guest physical address.
#[inline(always)]
pub(crate) fn copy_to_guest(src: &[u8], gpa: GuestPhysAddr) -> usize {
    let old_vsatp = riscv_h::register::vsatp::read().bits();
    unsafe {
        // Set vsatp to 0 to disable guest virtual address translation.
        Vsatp::from_bits(0).write();
        hfence_vvma_all();
        // Now GVA is the same as GPA.
        let ret = copy_to_guest_va(src, GuestVirtAddr::from_usize(gpa.as_usize()));
        // Restore the original vsatp.
        Vsatp::from_bits(old_vsatp).write();
        hfence_vvma_all();
        ret
    }
}

/// Fetches the guest instruction at the given guest virtual address.
///
/// The assembly helper uses HLVX so execute permission is checked as a guest
/// instruction fetch, while this wrapper converts the load-class trap CSRs back
/// into guest instruction-fetch categories.
#[inline(always)]
pub(crate) fn fetch_guest_instruction(
    gva: GuestVirtAddr,
) -> Result<u32, GuestInstructionFetchFault> {
    let mut instruction = 0u32;
    let mut fault = GuestInstructionFetchFaultRaw::default();
    let ret = unsafe {
        _fetch_guest_instruction(
            gva.as_usize() as *const u8,
            &mut instruction,
            &mut fault,
        )
    };
    if ret != 0 {
        return Err(fault.into_fetch_fault(gva));
    }
    Ok(instruction)
}
