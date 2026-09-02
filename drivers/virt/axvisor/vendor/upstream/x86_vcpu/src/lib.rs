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

#![no_std]
#![doc = include_str!("../README.md")]

#[cfg(any(feature = "vmx", feature = "svm"))]
#[macro_use]
extern crate log;
#[cfg(not(any(feature = "vmx", feature = "svm")))]
extern crate log;

extern crate alloc;

#[cfg(all(feature = "vmx", feature = "svm"))]
compile_error!("features `vmx` and `svm` are mutually exclusive");

#[cfg(test)]
mod test_utils;

pub(crate) mod msr;
#[cfg(feature = "vmx")]
#[macro_use]
pub(crate) mod regs;
mod ept;
#[cfg(not(feature = "vmx"))]
pub(crate) mod regs;
#[cfg(any(feature = "vmx", feature = "svm"))]
pub(crate) mod xstate;

cfg_if::cfg_if! {
    if #[cfg(feature = "vmx")] {
        mod vmx;
        use vmx as vendor;
        pub use vmx::{VmxExitInfo, VmxExitReason, VmxInterruptInfo, VmxIoExitInfo};

        pub use vendor::{
            VmxArchPerCpuState, VmxArchPerCpuState as X86ArchPerCpuState, VmxArchVCpu,
            VmxArchVCpu as X86ArchVCpu,
        };
    } else if #[cfg(feature = "svm")] {
        mod svm;
        use svm as vendor;

        pub use svm::{SvmExitCode, SvmExitInfo, SvmIntercept};
        pub use vendor::{
            SvmArchPerCpuState, SvmArchPerCpuState as X86ArchPerCpuState, SvmArchVCpu,
            SvmArchVCpu as X86ArchVCpu,
        };
    }
}

pub use ept::GuestPageWalkInfo;
pub use regs::GeneralRegisters;
#[cfg(any(feature = "vmx", feature = "svm"))]
pub use vendor::has_hardware_support;

pub const X86_MAX_PIO_INTERCEPT_RANGES: usize = 16;

/// Inclusive-start/exclusive-end x86 port I/O range to trap into the device model.
#[derive(Clone, Copy, Debug, Default)]
pub struct X86PioInterceptRange {
    pub base: u16,
    pub len: u16,
}

/// Setup configuration consumed by the current x86 vCPU implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct X86VCpuSetupConfig {
    /// Whether COM1 is emulated by the VM device model and should trap to the hypervisor.
    pub emulate_com1: bool,
    /// Whether guest EOI operations should trap so the software vLAPIC can keep
    /// in-kernel irqchip state authoritative.
    pub enable_eoi_exits: bool,
    pub pio_intercept_ranges: [X86PioInterceptRange; X86_MAX_PIO_INTERCEPT_RANGES],
    pub pio_intercept_range_count: usize,
}

impl X86VCpuSetupConfig {
    pub fn add_pio_intercept_range(&mut self, base: u16, len: u16) -> bool {
        if len == 0 || self.pio_intercept_range_count >= X86_MAX_PIO_INTERCEPT_RANGES {
            return false;
        }
        self.pio_intercept_ranges[self.pio_intercept_range_count] =
            X86PioInterceptRange { base, len };
        self.pio_intercept_range_count += 1;
        true
    }
}

/// KVM-compatible x86 segment register state.
#[derive(Clone, Copy, Debug, Default)]
pub struct X86KvmSegment {
    /// Segment base address.
    pub base: u64,
    /// Segment limit.
    pub limit: u32,
    /// Segment selector.
    pub selector: u16,
    /// Segment type.
    pub type_: u8,
    /// Whether the segment is present.
    pub present: bool,
    /// Descriptor privilege level.
    pub dpl: u8,
    /// Default operand size.
    pub db: bool,
    /// Descriptor type, system or code/data.
    pub s: bool,
    /// Long mode code segment bit.
    pub l: bool,
    /// Granularity bit.
    pub g: bool,
    /// Available bit.
    pub avl: bool,
    /// Whether the segment is unusable.
    pub unusable: bool,
}

/// KVM-compatible descriptor table state.
#[derive(Clone, Copy, Debug, Default)]
pub struct X86KvmDtable {
    /// Table base address.
    pub base: u64,
    /// Table limit.
    pub limit: u16,
}

/// Maximum CPUID entries copied from KVM_SET_CPUID2.
pub const X86_KVM_MAX_CPUID_ENTRIES: usize = 256;

/// Maximum MSR entries copied from KVM_SET_MSRS.
pub const X86_KVM_MAX_MSR_ENTRIES: usize = 256;

/// Size of the legacy FXSAVE/FXRSTOR state area.
pub const X86_KVM_FXSAVE_SIZE: usize = 512;

/// KVM-compatible legacy x87/SSE state in FXSAVE format.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug)]
pub struct X86KvmFxsave {
    /// Raw 512-byte FXSAVE image.
    pub bytes: [u8; X86_KVM_FXSAVE_SIZE],
}

impl X86KvmFxsave {
    /// Returns a zeroed FXSAVE image.
    pub const fn zeroed() -> Self {
        Self {
            bytes: [0; X86_KVM_FXSAVE_SIZE],
        }
    }
}

impl Default for X86KvmFxsave {
    fn default() -> Self {
        Self::zeroed()
    }
}

/// KVM-compatible CPUID entry consumed by Linux-host KVM ABI bridges.
#[derive(Clone, Copy, Debug, Default)]
pub struct X86KvmCpuidEntry {
    /// CPUID function leaf.
    pub function: u32,
    /// CPUID subleaf.
    pub index: u32,
    /// KVM_CPUID_FLAG_* bits.
    pub flags: u32,
    /// Result EAX.
    pub eax: u32,
    /// Result EBX.
    pub ebx: u32,
    /// Result ECX.
    pub ecx: u32,
    /// Result EDX.
    pub edx: u32,
}

/// KVM-compatible MSR entry consumed by Linux-host KVM ABI bridges.
#[derive(Clone, Copy, Debug, Default)]
pub struct X86KvmMsrEntry {
    /// MSR index.
    pub index: u32,
    /// MSR value.
    pub data: u64,
}

/// KVM-compatible x86 vCPU state consumed by Linux-host KVM ABI bridges.
#[derive(Clone, Copy, Debug)]
pub struct X86KvmVcpuState {
    /// General-purpose registers.
    pub regs: GeneralRegisters,
    /// Guest RIP.
    pub rip: u64,
    /// Guest RSP.
    pub rsp: u64,
    /// Guest RFLAGS.
    pub rflags: u64,
    /// Guest CR0.
    pub cr0: u64,
    /// Guest CR2.
    pub cr2: u64,
    /// Guest CR3.
    pub cr3: u64,
    /// Guest CR4.
    pub cr4: u64,
    /// Guest CR8.
    pub cr8: u64,
    /// Guest EFER.
    pub efer: u64,
    /// Guest APIC base MSR value.
    pub apic_base: u64,
    /// Guest XCR0 value previously provided by KVM_SET_XCRS.
    pub xcr0: u64,
    /// Whether `fxsave` contains valid guest x87/SSE state.
    pub fxsave_valid: bool,
    /// Guest x87/SSE state imported from KVM_SET_FPU or KVM_SET_XSAVE.
    pub fxsave: X86KvmFxsave,
    /// Segment state in KVM order: CS, DS, ES, FS, GS, SS, TR, LDT.
    pub segments: [X86KvmSegment; 8],
    /// Descriptor tables in KVM order: GDT, IDT.
    pub dtables: [X86KvmDtable; 2],
    /// CPUID entries previously provided by KVM_SET_CPUID2.
    pub cpuid_entries: [X86KvmCpuidEntry; X86_KVM_MAX_CPUID_ENTRIES],
    /// Number of valid CPUID entries.
    pub cpuid_nent: usize,
    /// MSR entries previously provided by KVM_SET_MSRS.
    pub msr_entries: [X86KvmMsrEntry; X86_KVM_MAX_MSR_ENTRIES],
    /// Number of valid MSR entries.
    pub nmsrs: usize,
}

impl Default for X86KvmVcpuState {
    fn default() -> Self {
        Self {
            regs: GeneralRegisters::default(),
            rip: 0,
            rsp: 0,
            rflags: 0,
            cr0: 0,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            cr8: 0,
            efer: 0,
            apic_base: 0,
            xcr0: 1,
            fxsave_valid: false,
            fxsave: X86KvmFxsave::zeroed(),
            segments: [X86KvmSegment::default(); 8],
            dtables: [X86KvmDtable::default(); 2],
            cpuid_entries: [X86KvmCpuidEntry::default(); X86_KVM_MAX_CPUID_ENTRIES],
            cpuid_nent: 0,
            msr_entries: [X86KvmMsrEntry::default(); X86_KVM_MAX_MSR_ENTRIES],
            nmsrs: 0,
        }
    }
}

/// Guest physical address used for APIC-access virtualization when APICv is enabled.
pub const X86_APIC_ACCESS_GPA: usize = 0xfee0_0000;

/// Whether the current x86 backend enables APIC-access virtualization.
pub fn supports_apicv() -> bool {
    true
}

/// Host physical address of the APIC-access page.
pub fn x86_apic_access_page_addr() -> axaddrspace::HostPhysAddr {
    x86_vlapic::EmulatedLocalApic::virtual_apic_access_addr()
}

#[cfg(not(any(feature = "vmx", feature = "svm")))]
pub fn has_hardware_support() -> bool {
    false
}

#[cfg(any(feature = "vmx", feature = "svm"))]
pub(crate) fn restore_host_interrupt_flag(host_rflags: u64) {
    if host_rflags & x86_64::registers::rflags::RFlags::INTERRUPT_FLAG.bits() != 0 {
        x86_64::instructions::interrupts::enable();
    } else {
        x86_64::instructions::interrupts::disable();
    }
}
