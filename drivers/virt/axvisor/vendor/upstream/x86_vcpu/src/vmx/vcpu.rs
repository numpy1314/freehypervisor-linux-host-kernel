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

use alloc::{collections::VecDeque, format, vec::Vec};
use core::{
    arch::naked_asm,
    fmt::{Debug, Formatter, Result},
    mem::size_of,
    sync::atomic::{AtomicUsize, Ordering},
};

use ax_errno::{AxResult, ax_err, ax_err_type};
use ax_kspin::SpinNoIrq as Mutex;
use axaddrspace::{
    GuestPhysAddr, GuestVirtAddr, HostPhysAddr, MappingFlags, NestedPageFaultInfo,
    device::{AccessWidth, Port, SysRegAddr, SysRegAddrRange},
};
use ax_memory_addr::AddrRange;
use axdevice_base::BaseDeviceOps;
use axvcpu::{AxArchVCpu, AxVCpuExitReason, InterruptTriggerMode};
use axvisor_api::vmm::{VCpuId, VMId};
use bit_field::BitField;
use raw_cpuid::CpuId;
use x86::{
    bits64::vmx,
    controlregs::Xcr0,
    dtables::{self, DescriptorTablePointer},
    segmentation::SegmentSelector,
};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr4, Cr4Flags, EferFlags};
use x86_vlapic::EmulatedLocalApic;

static VMX_QUEUE_EVENT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_INJECT_EVENT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_BLOCKED_EVENT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_INTERRUPT_WINDOW_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_VECTOR20_QUEUE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_VECTOR20_INJECT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_VECTOR20_BLOCKED_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_COALESCE_EVENT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_EXCEPTION_NMI_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_APIC_ACCESS_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_HLT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_PREEMPTION_TIMER_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_X2APIC_ID_READ_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_VIRTUALIZED_EOI_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_IDT_VECTORING_REPLAY_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_X2APIC_EOI_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VMX_PAUSE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

use super::{
    VmxExitInfo, as_axerr,
    definitions::{VmxExitReason, VmxInterruptionType},
    structs::{IOBitmap, MsrBitmap, VmxMsrEntry, VmxMsrList, VmxRegion},
    vmcs::{
        self, ApicAccessExitType, VmcsControl32, VmcsControl64, VmcsControlNW, VmcsGuest16,
        VmcsGuest32, VmcsGuest64, VmcsGuestNW, VmcsHost16, VmcsHost32, VmcsHost64, VmcsHostNW,
    },
};
use crate::{
    X86KvmCpuidEntry, X86KvmMsrEntry, X86KvmSegment, X86KvmVcpuState, X86VCpuSetupConfig,
    X86_KVM_MAX_CPUID_ENTRIES, X86_KVM_MAX_MSR_ENTRIES, ept::GuestPageWalkInfo, msr::Msr,
    regs::GeneralRegisters, restore_host_interrupt_flag, xstate::XState,
};

const VMX_PREEMPTION_TIMER_SET_VALUE: u32 = 1_000_000;

const QEMU_EXIT_PORT: u16 = 0x604;
const QEMU_RESET_PORT: u16 = 0xcf9;
const QEMU_EXIT_MAGIC: u64 = 0x2000;
const VMX_IO_EXIT_LOG_LIMIT: usize = 128;
const X86_PIT_PORT_BASE: u16 = 0x40;
const X86_PIT_PORT_COUNT: u32 = 4;
const X86_PIT_SPEAKER_PORT: u16 = 0x61;
const X86_COM1_PORT_BASE: u16 = 0x3f8;
const X86_COM1_PORT_COUNT: u32 = 8;
const X2APIC_MSR_BASE: u32 = 0x800;
const X2APIC_MSR_END: u32 = 0x8ff;
const X2APIC_ID_MSR: u32 = X2APIC_MSR_BASE + 0x2;
const X2APIC_EOI_MSR: u32 = X2APIC_MSR_BASE + 0xb;
const X2APIC_ICR_MSR: u32 = X2APIC_MSR_BASE + 0x30;
/// KVM paravirtual clock MSRs (kvm-clock). Writes are forwarded to the host so
/// it can populate the shared pvclock page; without this the guest has no
/// stable clocksource under `tsc=unstable`.
const MSR_KVM_WALL_CLOCK_NEW: u32 = 0x4b56_4d00;
const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
const X86_LOCAL_APIC_EOI_OFFSET: usize = 0xb0;
const X86_APIC_ACCESS_GPA: usize = 0xfee0_0000;
const X86_IOAPIC_BASE: usize = 0xfec0_0000;
const X86_IOAPIC_SIZE: usize = 0x1000;

fn vmx_emerg_write(msg: &str) {
    axvisor_api::host::emerg_write_bytes(msg.as_bytes());
}

fn vmx_should_log_vector(vector: u8, counter: &AtomicUsize, limit: usize) -> Option<usize> {
    if vector != 0x20 {
        return None;
    }
    let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
    (count <= limit || count.is_power_of_two()).then_some(count)
}

fn vmx_log_terminal_exit(tag: &str, exit_info: &VmxExitInfo, regs: &GeneralRegisters) {
    vmx_emerg_write(
        format!(
            "vmx terminal exit {tag}: raw={:#x} reason={:?} qual={:#x} len={} rip={:#x} \
             instr_err={:#x} rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x}\n",
            exit_info.exit_reason_raw,
            exit_info.exit_reason,
            exit_info.exit_qualification,
            exit_info.exit_instruction_length,
            exit_info.guest_rip,
            exit_info.instruction_error_raw,
            regs.rax,
            regs.rbx,
            regs.rcx,
            regs.rdx,
        )
        .as_str(),
    );
}

fn vmx_log_run_err(tag: &str, exit_info: &VmxExitInfo, regs: &GeneralRegisters, err: &ax_errno::AxError) {
    vmx_emerg_write(
        format!(
            "vmx run error {tag}: err={err:?} raw={:#x} reason={:?} qual={:#x} len={} rip={:#x} \
             instr_err={:#x} rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x}\n",
            exit_info.exit_reason_raw,
            exit_info.exit_reason,
            exit_info.exit_qualification,
            exit_info.exit_instruction_length,
            exit_info.guest_rip,
            exit_info.instruction_error_raw,
            regs.rax,
            regs.rbx,
            regs.rcx,
            regs.rdx,
        )
        .as_str(),
    );
}

#[derive(PartialEq, Eq, Debug)]
pub enum VmCpuMode {
    Real,
    Protected,
    Compatibility, // IA-32E mode (CS.L = 0)
    Mode64,        // IA-32E mode (CS.L = 1)
}

const MSR_IA32_EFER_LMA_BIT: u64 = 1 << 10;
const CR0_PE: usize = 1 << 0;
const KVM_CPUID_FLAG_SIGNIFICANT_INDEX: u32 = 1 << 0;
const VMEXIT_INSTR_LEN_RDMSR_WRMSR: u8 = 2;
const VMX_KVM_SWITCH_MSRS: [Msr; 5] = [
    Msr::IA32_STAR,
    Msr::IA32_LSTAR,
    Msr::IA32_CSTAR,
    Msr::IA32_FMASK,
    Msr::IA32_KERNEL_GSBASE,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingEvent {
    vector: u8,
    err_code: Option<u32>,
    level_triggered: bool,
}

fn kvm_segment_access_rights(segment: &X86KvmSegment) -> u32 {
    if segment.unusable {
        return 1 << 16;
    }

    (segment.type_ as u32 & 0xf)
        | ((segment.s as u32) << 4)
        | ((segment.dpl as u32 & 0x3) << 5)
        | ((segment.present as u32) << 7)
        | ((segment.avl as u32) << 12)
        | ((segment.l as u32) << 13)
        | ((segment.db as u32) << 14)
        | ((segment.g as u32) << 15)
}

/// A virtual CPU within a guest.
#[repr(C)]
pub struct VmxVcpu {
    // The order of `guest_regs`, `host_stack_top`, and `host_rflags` is
    // mandatory. They must be the first three fields. If you want to change
    // the order or the type of these fields, you must also change the assembly
    // in this file.
    /// Guest general-purpose registers.
    guest_regs: GeneralRegisters,
    /// The top of the host stack.
    host_stack_top: u64,
    /// Host RFLAGS captured immediately before VM entry.
    host_rflags: u64,

    // The order of the following fields is not mandatory.

    // VCpu states and configurations
    vm_id: VMId,
    vcpu_id: VCpuId,
    /// Whether the VMCS has been launched. Used to determine whether to `vmx_launch` or `vmx_resume`.
    launched: bool,
    /// The guest entry point.
    entry: Option<GuestPhysAddr>,
    /// The EPT root address.
    ept_root: Option<HostPhysAddr>,
    // /// Whether this VCPU is a host VCpu. Used in type 1.5 hypervisor.
    // is_host: bool, temporary removed because we don't care about type 1.5 now

    // VMCS-related fields
    /// The VMCS region.
    vmcs: VmxRegion,
    /// The I/O bitmap for the VMCS.
    io_bitmap: IOBitmap,
    /// The MSR bitmap for the VMCS.
    msr_bitmap: MsrBitmap,
    /// MSRs loaded on VM entry from the KVM-provided guest state.
    vm_entry_msr_load: VmxMsrList,
    /// MSRs stored from the guest on VM exit.
    vm_exit_msr_store: VmxMsrList,
    /// MSRs loaded back to host values on VM exit.
    vm_exit_msr_load: VmxMsrList,

    // Interrupt-related fields
    /// Pending events to be injected to the guest.
    ///
    /// KVM-style multi-vCPU execution lets one host thread run this vCPU while
    /// another vCPU thread injects IPIs into it. Keep the event queue protected;
    /// VMCS state remains owned by the KVM_RUN thread.
    pending_events: Mutex<VecDeque<PendingEvent>>,
    /// Emulated Local APIC.
    vlapic: EmulatedLocalApic,
    /// CPUID table provided by KVM_SET_CPUID2.
    kvm_cpuid_entries: Vec<X86KvmCpuidEntry>,
    /// Number of valid KVM CPUID entries.
    kvm_cpuid_nent: usize,
    /// MSR table provided by KVM_SET_MSRS.
    kvm_msr_entries: Vec<X86KvmMsrEntry>,
    /// Number of valid KVM MSR entries.
    kvm_nmsrs: usize,

    // Extra states
    /// The XState of the VCpu. Both host and guest.
    xstate: XState,
    /// Guest RIP captured at the most recent VM-exit, while the VMCS was still
    /// loaded on the current physical CPU. Cached so that host-side code running
    /// *after* the vCPU is unbound (e.g. the KVM bridge's internal-progress
    /// path) can inspect the last guest RIP without issuing a `vmread` against
    /// an unloaded VMCS (which would fail). Zero until the first VM-exit.
    last_exit_rip: usize,

    // TSC virtualization state (mirrors KVM's per-vCPU TSC offset handling in
    // arch/x86/kvm/x86.c:kvm_arch_vcpu_load). Under CPU oversubscription the
    // vCPU thread migrates freely between host pCPUs between guest entries (the
    // KVM bridge deliberately drops migrate_disable to let CFS load-balance).
    // Since we do NOT intercept RDTSC/RDTSCP, the guest reads the raw host TSC;
    // migrating to a pCPU with an earlier/skewed TSC would make the guest's
    // CLOCK_MONOTONIC go backwards, stalling hrtimer-based sleeps (e.g. the
    // usleep_range poll in cpuhp_wait_for_sync_state) so SMP bringup deadlocks.
    // We keep the guest-visible TSC monotonic by adjusting VMCS TSC_OFFSET on
    // any migration that would otherwise move it backwards.
    /// Current VMCS TSC_OFFSET value (guest_tsc = host_tsc + tsc_offset).
    tsc_offset: i64,
    /// Host pCPU id observed at the previous VM entry; -1 before the first run.
    last_host_cpu: i32,
    /// Guest-visible TSC observed at the previous VM entry (host_tsc + offset).
    last_guest_tsc: u64,

    // Tracing-related fields
    #[cfg(feature = "tracing")]
    /// The guest registers when the VM-exit happens.
    guest_regs_exiting: GeneralRegisters,
}

impl VmxVcpu {
    fn pending_event_count(&self) -> usize {
        self.pending_events.lock().len()
    }

    fn has_pending_events(&self) -> bool {
        !self.pending_events.lock().is_empty()
    }

    #[allow(dead_code)]
    fn pop_pending_event_if_front_matches(&mut self, event: PendingEvent) {
        let mut pending_events = self.pending_events.lock();
        if pending_events.front().copied() == Some(event) {
            pending_events.pop_front();
        }
    }

    /// Remove the first queued event that exactly matches `event`, regardless of
    /// its position in the queue. Used when injection fairness selects an event
    /// that is not at the queue head (e.g. an APIC timer interrupt sitting behind
    /// a freshly re-queued PIT IRQ0).
    fn pop_pending_event_matching(&mut self, event: PendingEvent) {
        let mut pending_events = self.pending_events.lock();
        if let Some(index) = pending_events
            .iter()
            .position(|pending| *pending == event)
        {
            pending_events.remove(index);
        }
    }

    fn push_pending_event(&mut self, event: PendingEvent) -> bool {
        let mut pending_events = self.pending_events.lock();
        // Unconditional trace for high-priority IPI vectors reaching THIS
        // vCPU's queue, so we can tell "IPI never enqueued to target" apart
        // from "enqueued but never injected".
        if event.vector >= 0xf0 {
            vmx_emerg_write(
                format!(
                    "x86_vmx::push_ipi vm={} vcpu={} vector={:#x} qlen_before={}\n",
                    self.vm_id, self.vcpu_id, event.vector, pending_events.len()
                )
                .as_str(),
            );
        }
        if event.vector < 32 {
            // Precise CPU exceptions must be reinjected before maskable
            // interrupts. Otherwise a blocked external/timer interrupt at the
            // queue head can starve a faulting instruction and repeatedly
            // enqueue the same exception.
            pending_events.push_front(event);
            return true;
        }

        if event.vector >= 32 {
            if let Some(index) = pending_events
                .iter()
                .position(|pending| pending.vector == event.vector)
            {
                let existing = &mut pending_events[index];
                existing.level_triggered |= event.level_triggered;
                let level_triggered = existing.level_triggered;
                let pending_len = pending_events.len();
                let count = VMX_COALESCE_EVENT_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if count <= 32 || count.is_power_of_two() {
                    vmx_emerg_write(
                        format!(
                            "x86_vmx::coalesce_event vector={:#x} level={} pending={} count={}\n",
                            event.vector,
                            level_triggered,
                            pending_len,
                            count
                        )
                        .as_str(),
                    );
                }
                return false;
            }
        }

        pending_events.push_back(event);
        true
    }

    fn replay_interrupted_idt_vectoring_event(&mut self) {
        let Ok(info) = vmcs::idt_vectoring_info() else {
            return;
        };
        if !info.valid {
            return;
        }

        let event = PendingEvent {
            vector: info.vector,
            err_code: info.err_code,
            level_triggered: false,
        };
        let pending_len = {
            let mut pending_events = self.pending_events.lock();
            pending_events.push_front(event);
            pending_events.len()
        };

        let count = VMX_IDT_VECTORING_REPLAY_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 32 || count.is_power_of_two() {
            vmx_emerg_write(
                format!(
                    "x86_vmx::idt_vectoring_replay vector={:#x} type={:?} err={:?} pending={} count={}\n",
                    info.vector,
                    info.int_type,
                    info.err_code,
                    pending_len,
                    count
                )
                .as_str(),
            );
        }
    }

    /// Create a new [`VmxVcpu`].
    pub fn new(vm_id: VMId, vcpu_id: VCpuId) -> AxResult<Self> {
        let vmcs_revision_id = super::read_vmcs_revision_id();
        let mut kvm_cpuid_entries = Vec::new();
        kvm_cpuid_entries.resize(X86_KVM_MAX_CPUID_ENTRIES, X86KvmCpuidEntry::default());
        let mut kvm_msr_entries = Vec::new();
        kvm_msr_entries.resize(X86_KVM_MAX_MSR_ENTRIES, X86KvmMsrEntry::default());

        let vcpu = Self {
            guest_regs: GeneralRegisters::default(),
            host_stack_top: 0,
            host_rflags: 0,
            vm_id,
            vcpu_id,
            launched: false,
            entry: None,
            ept_root: None,
            // is_host: false,
            vmcs: VmxRegion::new(vmcs_revision_id, false)?,
            io_bitmap: IOBitmap::passthrough_all()?,
            msr_bitmap: MsrBitmap::passthrough_all()?,
            vm_entry_msr_load: VmxMsrList::new()?,
            vm_exit_msr_store: VmxMsrList::new()?,
            vm_exit_msr_load: VmxMsrList::new()?,
            pending_events: Mutex::new(VecDeque::with_capacity(8)),
            vlapic: EmulatedLocalApic::new(vm_id, vcpu_id),
            kvm_cpuid_entries,
            kvm_cpuid_nent: 0,
            kvm_msr_entries,
            kvm_nmsrs: 0,
            xstate: XState::new(),
            last_exit_rip: 0,
            tsc_offset: 0,
            last_host_cpu: -1,
            last_guest_tsc: 0,
            #[cfg(feature = "tracing")]
            guest_regs_exiting: GeneralRegisters::default(),
        };
        info!("[HV] created VmxVcpu(vmcs: {:#x})", vcpu.vmcs.phys_addr());
        Ok(vcpu)
    }

    /// Set the new [`VmxVcpu`] context from guest OS.
    pub fn setup(&mut self, ept_root: HostPhysAddr, entry: GuestPhysAddr) -> AxResult {
        self.setup_vmcs(entry, ept_root, X86VCpuSetupConfig::default())?;
        Ok(())
    }

    /// Rebuild VMCS for an AP started by SIPI.
    ///
    /// x86 SIPI does not load RIP with the physical entry directly. Hardware starts
    /// the AP in real mode at CS:IP = vector*0x100:0, whose linear address is
    /// vector*0x1000. Linux relies on that exact segment state for its trampoline.
    pub fn setup_sipi(&mut self, ept_root: HostPhysAddr, entry: GuestPhysAddr) -> AxResult {
        self.setup_sipi_with_config(ept_root, entry, X86VCpuSetupConfig::default())
    }

    /// Rebuild VMCS for an AP started by SIPI with the VM's x86 setup policy.
    pub fn setup_sipi_with_config(
        &mut self,
        ept_root: HostPhysAddr,
        entry: GuestPhysAddr,
        config: X86VCpuSetupConfig,
    ) -> AxResult {
        self.setup_vmcs(entry, ept_root, config)?;
        self.bind_to_current_processor()?;
        let vector_base = entry.as_usize() & !0xfff;
        VmcsGuest16::CS_SELECTOR.write((vector_base >> 4) as u16)?;
        VmcsGuestNW::CS_BASE.write(vector_base)?;
        VmcsGuestNW::RIP.write(0)?;
        vmx_emerg_write(
            format!(
                "x86_vmx::setup_sipi vm={} vcpu={} entry={:#x} cs={:#x} cs_base={:#x} rip={:#x} rflags={:#x} cr0={:#x} cr4={:#x}\n",
                self.vm_id,
                self.vcpu_id,
                entry.as_usize(),
                VmcsGuest16::CS_SELECTOR.read().unwrap_or(0),
                VmcsGuestNW::CS_BASE.read().unwrap_or(0),
                VmcsGuestNW::RIP.read().unwrap_or(usize::MAX),
                VmcsGuestNW::RFLAGS.read().unwrap_or(usize::MAX),
                VmcsGuestNW::CR0.read().unwrap_or(usize::MAX),
                VmcsGuestNW::CR4.read().unwrap_or(usize::MAX),
            )
            .as_str(),
        );
        self.unbind_from_current_processor()?;
        Ok(())
    }

    /// Applies KVM ABI vCPU state to the VMCS and saved general registers.
    pub fn apply_kvm_state(&mut self, state: &X86KvmVcpuState) -> AxResult {
        self.guest_regs = state.regs;
        self.kvm_cpuid_nent = core::cmp::min(state.cpuid_nent, X86_KVM_MAX_CPUID_ENTRIES);
        self.kvm_nmsrs = core::cmp::min(state.nmsrs, X86_KVM_MAX_MSR_ENTRIES);
        self.kvm_cpuid_entries[..self.kvm_cpuid_nent]
            .copy_from_slice(&state.cpuid_entries[..self.kvm_cpuid_nent]);
        self.kvm_msr_entries[..self.kvm_nmsrs]
            .copy_from_slice(&state.msr_entries[..self.kvm_nmsrs]);
        self.sync_kvm_msr_bitmap();
        if state.fxsave_valid {
            self.xstate.set_guest_fxsave(&state.fxsave);
        }

        self.bind_to_current_processor()?;
        let result = self.write_kvm_state_to_vmcs(state);
        self.unbind_from_current_processor()?;
        result
    }

    // /// Get the identifier of this [`VmxVcpu`].
    // pub fn vcpu_id(&self) -> usize {
    //     get_current_vcpu::<Self>().unwrap().id()
    // }

    /// Bind this [`VmxVcpu`] to current logical processor.
    pub fn bind_to_current_processor(&self) -> AxResult {
        debug!(
            "VmxVcpu bind to current processor vmcs @ {:#x}",
            self.vmcs.phys_addr()
        );
        unsafe {
            vmx::vmptrld(self.vmcs.phys_addr().as_usize() as u64).map_err(as_axerr)?;
        }
        self.setup_vmcs_host()?;
        Ok(())
    }

    /// Unbind this [`VmxVcpu`] from current logical processor.
    pub fn unbind_from_current_processor(&self) -> AxResult {
        debug!(
            "VmxVcpu unbind from current processor vmcs @ {:#x}",
            self.vmcs.phys_addr()
        );

        unsafe {
            vmx::vmclear(self.vmcs.phys_addr().as_usize() as u64).map_err(as_axerr)?;
        }
        Ok(())
    }

    /// Get CPU mode of the guest.
    pub fn get_cpu_mode(&self) -> VmCpuMode {
        let ia32_efer = VmcsGuest64::IA32_EFER.read().unwrap();
        let cs_access_right = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap();
        let cr0 = VmcsGuestNW::CR0.read().unwrap();
        if (ia32_efer & MSR_IA32_EFER_LMA_BIT) != 0 {
            if (cs_access_right & 0x2000) != 0 {
                // CS.L = 1
                VmCpuMode::Mode64
            } else {
                VmCpuMode::Compatibility
            }
        } else if (cr0 & CR0_PE) != 0 {
            VmCpuMode::Protected
        } else {
            VmCpuMode::Real
        }
    }

    /// Resynchronize the guest-visible TSC on host-pCPU migration.
    ///
    /// Because we do not intercept RDTSC/RDTSCP, the guest reads `host_tsc +
    /// tsc_offset`. When CFS migrates this vCPU thread to a different host pCPU
    /// between guest entries, that pCPU's raw TSC may be earlier than the last
    /// one, which would make the guest's TSC (and therefore CLOCK_MONOTONIC) go
    /// backwards. hrtimer-based sleeps compute an absolute deadline from
    /// `ktime_get()`; if the clock never reaches the deadline the sleeping task
    /// never wakes (observed: the SMP-bringup control task stalls forever in
    /// `usleep_range` inside `cpuhp_wait_for_sync_state`, so no AP is released
    /// and bringup deadlocks with an RCU stall).
    ///
    /// Fix, mirroring KVM's `kvm_arch_vcpu_load` (arch/x86/kvm/x86.c:5209): on a
    /// migration that would move the guest TSC backwards, recompute `tsc_offset`
    /// so the guest TSC continues from `last_guest_tsc + 1` (we drop the missed
    /// interval rather than fast-forward, matching KVM's clamp semantics). When
    /// there is no migration, or the projected TSC is already monotonic, the
    /// offset is left untouched, so non-oversubscribed runs (which rarely
    /// migrate) keep `tsc_offset == 0` and behave exactly as before.
    fn resync_tsc_offset(&mut self) {
        let host_cpu = axvisor_api::host::current_host_cpu_id() as i32;
        let host_tsc = unsafe { core::arch::x86_64::_rdtsc() };

        // Unconditional bounded probe: prove the function runs and observe what
        // host CPU id it sees across the first entries (detects a broken/constant
        // current_host_cpu_id, or genuine no-migration).
        static TSC_PROBE_CNT: AtomicUsize = AtomicUsize::new(0);
        let pc = TSC_PROBE_CNT.fetch_add(1, Ordering::Relaxed) + 1;
        if pc <= 40 || pc.is_power_of_two() {
            vmx_emerg_write(&format!(
                "tsc_probe pc={} host_cpu={} last_host_cpu={} tsc_off={}\n",
                pc, host_cpu, self.last_host_cpu, self.tsc_offset,
            ));
        }

        if self.last_host_cpu < 0 {
            // First entry: start the guest on the raw host TSC (offset 0), which
            // preserves the previous behaviour and lets the guest calibrate its
            // TSC frequency against the real host rate.
            self.last_host_cpu = host_cpu;
            self.last_guest_tsc = host_tsc;
            return;
        }

        let migrated = self.last_host_cpu != host_cpu;
        let projected = host_tsc.wrapping_add(self.tsc_offset as u64);

        // Bounded diagnostic: count total entries / migrations / backward-repairs
        // so we can tell whether this path ever triggers under oversubscription.
        static TSC_ENTRY_CNT: AtomicUsize = AtomicUsize::new(0);
        static TSC_MIGRATE_CNT: AtomicUsize = AtomicUsize::new(0);
        static TSC_REPAIR_CNT: AtomicUsize = AtomicUsize::new(0);
        let ec = TSC_ENTRY_CNT.fetch_add(1, Ordering::Relaxed) + 1;
        if migrated {
            let mc = TSC_MIGRATE_CNT.fetch_add(1, Ordering::Relaxed) + 1;
            if mc <= 16 || mc.is_power_of_two() {
                vmx_emerg_write(&format!(
                    "tsc_resync migrate mc={} ec={} from_cpu={} to_cpu={} host_tsc={} last_g={} proj={} backward={}\n",
                    mc, ec, self.last_host_cpu, host_cpu, host_tsc, self.last_guest_tsc,
                    projected, (projected < self.last_guest_tsc) as u8,
                ));
            }
        }

        if migrated && projected < self.last_guest_tsc {
            let target = self.last_guest_tsc.wrapping_add(1);
            self.tsc_offset = (target.wrapping_sub(host_tsc)) as i64;
            let _ = VmcsControl64::TSC_OFFSET.write(self.tsc_offset as u64);
            self.last_guest_tsc = target;
            let rc = TSC_REPAIR_CNT.fetch_add(1, Ordering::Relaxed) + 1;
            if rc <= 16 || rc.is_power_of_two() {
                vmx_emerg_write(&format!(
                    "tsc_resync REPAIR rc={} ec={} new_offset={} target_g={}\n",
                    rc, ec, self.tsc_offset, target,
                ));
            }
        } else {
            self.last_guest_tsc = projected;
        }
        self.last_host_cpu = host_cpu;
    }

    /// Run the guest. It returns when a vm-exit happens and returns the vm-exit if it cannot be handled by this [`VmxVcpu`] itself.
    pub fn inner_run(&mut self) -> Option<VmxExitInfo> {
        // Keep the guest-visible TSC monotonic across host-pCPU migration.
        // Runs here (VMCS is bound and the vCPU thread is pinned to the current
        // pCPU for this entry) so any TSC_OFFSET adjustment lands on the VMCS
        // we are about to VMLAUNCH/VMRESUME. Mirrors kvm_arch_vcpu_load
        // (arch/x86/kvm/x86.c:5209) recomputing the offset on migration.
        self.resync_tsc_offset();

        // kvm_inject_apic_timer_irqs analog: drain the owed periodic LAPIC timer
        // tick (latched by the host-timer callback, independent of whether this
        // vCPU was on-core) and queue it for injection. This is what keeps the
        // guest tick alive under CPU oversubscription: the tick is injected on
        // the vCPU's own thread at VM-entry, not from the timer callback thread.
        if let Some(vector) = self.vlapic.take_pending_timer_tick() {
            self.queue_event(vector, None);
        }
        self.inject_pending_events().unwrap();
        self.sync_msr_switch_entries().unwrap();

        // Force the guest activity state back to Active before every VM entry.
        //
        // On an HLT VM-exit the CPU records guest activity state = HLT (1) in the
        // VMCS, and our HLT handler advances RIP past the `hlt` instruction. If we
        // then resume with activity state still HLT while RIP points *past* the
        // hlt, the logical processor stays in the halted state and an event
        // injected via the VM-entry interruption-information field is NOT
        // delivered: the guest never vectors to its ISR (observed: after the guest
        // disables its LAPIC timer and idles in safe_halt, we hard-inject PIT IRQ0
        // 0x30 thousands of times but RIP stays at safe_halt, no EOI ever occurs,
        // and guest jiffies freeze). Resetting activity state to Active (0) makes
        // the resumed guest execute at RIP and take the injected interrupt,
        // restoring the periodic tick. Writing 0 is always valid: an idle guest
        // with no pending event simply re-executes `sti; hlt` and exits again.
        let _ = VmcsGuest32::ACTIVITY_STATE.write(0);

        // Run guest
        self.load_guest_xstate();

        #[cfg(feature = "tracing")]
        {
            use crate::regs::GeneralRegistersDiff;
            // Tracing, do a diff of the guest registers before entering the guest
            let diff = GeneralRegistersDiff::new(self.guest_regs_exiting, self.guest_regs);
            if !diff.is_same() {
                debug!("VCpu registers changed during handling VM-exit: {diff:#x?}");
            } else {
                debug!("VCpu registers unchanged during handling VM-exit");
            }
        }

        unsafe {
            if self.launched {
                self.vmx_resume();
            } else {
                self.launched = true;
                VmcsHostNW::RSP
                    .write(&self.host_stack_top as *const _ as usize)
                    .unwrap();

                self.vmx_launch();
            }
        }
        self.load_host_xstate();
        restore_host_interrupt_flag(self.host_rflags);
        self.capture_guest_msr_store_area();

        #[cfg(feature = "tracing")]
        {
            self.guest_regs_exiting = self.guest_regs;
        }

        // Handle vm-exits
        let exit_info = self.exit_info().unwrap();
        self.replay_interrupted_idt_vectoring_event();
        // debug!("VM exit: {:#x?}", exit_info);

        match self.builtin_vmexit_handler(&exit_info) {
            Some(result) => match result {
                Ok(()) => None,
                Err(err) => {
                    vmx_log_terminal_exit("builtin-handler-error", &exit_info, self.regs());
                    vmx_emerg_write(format!("vmx builtin handler error: {err:?}\n").as_str());
                    panic!(
                        "VmxVcpu failed to handle a VM-exit that should be handled by itself: \
                         {:?}, error {:?}, vcpu: {:#x?}",
                        exit_info.exit_reason, err, self
                    );
                }
            },
            None => Some(exit_info),
        }
    }

    /// Basic information about VM exits.
    pub fn exit_info(&self) -> AxResult<vmcs::VmxExitInfo> {
        vmcs::exit_info()
    }

    /// Raw information for VM Exits Due to Vectored Events, See SDM 25.9.2
    pub fn raw_interrupt_exit_info(&self) -> AxResult<u32> {
        vmcs::raw_interrupt_exit_info()
    }

    /// Information for VM exits due to external interrupts.
    pub fn interrupt_exit_info(&self) -> AxResult<vmcs::VmxInterruptInfo> {
        vmcs::interrupt_exit_info()
    }

    /// Information for VM exits due to I/O instructions.
    pub fn io_exit_info(&self) -> AxResult<vmcs::VmxIoExitInfo> {
        vmcs::io_exit_info()
    }

    /// Information for VM exits due to nested page table faults (EPT violation).
    pub fn nested_page_fault_info(&self) -> AxResult<NestedPageFaultInfo> {
        vmcs::ept_violation_info()
    }

    /// Information for VM exits due to APIC access.
    pub fn apic_access_exit_info(&self) -> AxResult<vmcs::ApicAccessExitInfo> {
        vmcs::apic_access_exit_info()
    }

    /// Guest general-purpose registers.
    pub fn regs(&self) -> &GeneralRegisters {
        &self.guest_regs
    }

    /// Mutable reference of guest general-purpose registers.
    pub fn regs_mut(&mut self) -> &mut GeneralRegisters {
        &mut self.guest_regs
    }

    /// Guest stack pointer. (`RSP`)
    pub fn stack_pointer(&self) -> usize {
        VmcsGuestNW::RSP.read().unwrap()
    }

    /// Set guest stack pointer. (`RSP`)
    pub fn set_stack_pointer(&mut self, rsp: usize) {
        VmcsGuestNW::RSP.write(rsp).unwrap()
    }

    /// Translate guest virtual addr to linear addr    
    pub fn gla2gva(&self, guest_rip: GuestVirtAddr) -> GuestVirtAddr {
        let cpu_mode = self.get_cpu_mode();
        let seg_base = if cpu_mode == VmCpuMode::Mode64 {
            0
        } else {
            VmcsGuestNW::CS_BASE.read().unwrap()
        };
        // debug!(
        //     "seg_base: {:#x}, guest_rip: {:#x} cpu mode:{:?}",
        //     seg_base, guest_rip, cpu_mode
        // );
        guest_rip + seg_base
    }

    /// Get Translate guest page table info
    pub fn get_ptw_info(&self) -> GuestPageWalkInfo {
        let top_entry = VmcsGuestNW::CR3.read().unwrap();
        let level = self.get_paging_level();
        let is_write_access = false;
        let is_inst_fetch = false;
        let is_user_mode_access = ((VmcsGuest32::SS_ACCESS_RIGHTS.read().unwrap() >> 5) & 0x3) == 3;
        let mut pse = true;
        let mut nxe =
            (VmcsGuest64::IA32_EFER.read().unwrap() & EferFlags::NO_EXECUTE_ENABLE.bits()) != 0;
        let wp = (VmcsGuestNW::CR0.read().unwrap() & Cr0Flags::WRITE_PROTECT.bits() as usize) != 0;
        let is_smap_on = (VmcsGuestNW::CR4.read().unwrap()
            & Cr4Flags::SUPERVISOR_MODE_ACCESS_PREVENTION.bits() as usize)
            != 0;
        let is_smep_on = (VmcsGuestNW::CR4.read().unwrap()
            & Cr4Flags::SUPERVISOR_MODE_EXECUTION_PROTECTION.bits() as usize)
            != 0;
        let width: u32;
        if level == 4 || level == 3 {
            width = 9;
        } else if level == 2 {
            width = 10;
            pse = VmcsGuestNW::CR4.read().unwrap() & Cr4Flags::PAGE_SIZE_EXTENSION.bits() as usize
                != 0;
            nxe = false;
        } else {
            width = 0;
        }
        GuestPageWalkInfo {
            top_entry,
            level,
            width,
            is_user_mode_access,
            is_write_access,
            is_inst_fetch,
            pse,
            wp,
            nxe,
            is_smap_on,
            is_smep_on,
        }
    }

    /// Guest rip. (`RIP`)
    pub fn rip(&self) -> usize {
        VmcsGuestNW::RIP.read().unwrap()
    }

    /// Guest RIP as of the most recent VM-exit, read from the cached value that
    /// was captured while the VMCS was loaded. Unlike [`Self::rip`], this does
    /// NOT issue a `vmread`, so it is safe to call from host-side code that runs
    /// after the vCPU has been unbound from the physical CPU. Returns 0 before
    /// the first VM-exit.
    pub fn last_exit_rip(&self) -> usize {
        self.last_exit_rip
    }

    /// Guest cs. (`cs`)
    pub fn cs(&self) -> u16 {
        VmcsGuest16::CS_SELECTOR.read().unwrap()
    }

    /// Advance guest `RIP` by `instr_len` bytes.
    pub fn advance_rip(&mut self, instr_len: u8) -> AxResult {
        VmcsGuestNW::RIP.write(VmcsGuestNW::RIP.read()? + instr_len as usize)
    }

    /// Add a virtual interrupt or exception to the pending events list,
    /// and try to inject it before later VM entries.
    pub fn queue_event(&mut self, vector: u8, err_code: Option<u32>) {
        let count = VMX_QUEUE_EVENT_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 32 || count.is_power_of_two() {
            info!(
                "x86 vmx queue_event vector={:#x} err_code={:?} pending_before={} count={}",
                vector,
                err_code,
                self.pending_event_count(),
                count
            );
        }
        self.push_pending_event(PendingEvent {
            vector,
            err_code,
            level_triggered: false,
        });
    }

    /// Add a virtual interrupt or exception with trigger mode metadata.
    pub fn queue_event_with_trigger(
        &mut self,
        vector: u8,
        err_code: Option<u32>,
        level_triggered: bool,
    ) {
        if let Some(count) = vmx_should_log_vector(vector, &VMX_VECTOR20_QUEUE_LOG_COUNT, 32) {
            vmx_emerg_write(
                format!(
                    "x86_vmx::queue_vector20 level={} pending_before={} count={}\n",
                    level_triggered,
                    self.pending_event_count(),
                    count
                )
                .as_str(),
            );
        }
        self.push_pending_event(PendingEvent {
            vector,
            err_code,
            level_triggered,
        });
    }

    /// If enable, a VM exit occurs at the beginning of any instruction if
    /// `RFLAGS.IF` = 1 and there are no other blocking of interrupts.
    /// (see SDM, Vol. 3C, Section 24.4.2)
    pub fn set_interrupt_window(&mut self, enable: bool) -> AxResult {
        let mut ctrl = VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.read()?;
        let bits = vmcs::controls::PrimaryControls::INTERRUPT_WINDOW_EXITING.bits();
        if enable {
            ctrl |= bits
        } else {
            ctrl &= !bits
        }
        VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.write(ctrl)?;
        Ok(())
    }

    /// Set I/O intercept by modifying I/O bitmap.
    pub fn set_io_intercept_of_range(&mut self, port_base: u32, count: u32, intercept: bool) {
        self.io_bitmap
            .set_intercept_of_range(port_base, count, intercept)
    }

    /// Set msr intercept by modifying msr bitmap.
    /// Todo: distinguish read and write.
    pub fn set_msr_intercept_of_range(&mut self, msr: u32, intercept: bool) {
        self.msr_bitmap.set_read_intercept(msr, intercept);
        self.msr_bitmap.set_write_intercept(msr, intercept);
    }

    fn sync_kvm_msr_bitmap(&mut self) {
        let mut i = 0;
        while i < self.kvm_nmsrs {
            self.set_msr_intercept_of_range(self.kvm_msr_entries[i].index, true);
            i += 1;
        }
        self.set_msr_intercept_of_range(Msr::IA32_EFER as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_PAT as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_SYSENTER_CS as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_SYSENTER_ESP as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_SYSENTER_EIP as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_STAR as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_LSTAR as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_CSTAR as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_FMASK as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_FS_BASE as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_GS_BASE as u32, true);
        self.set_msr_intercept_of_range(Msr::IA32_KERNEL_GSBASE as u32, true);
    }

    fn guest_msr_switch_entries(&self) -> [VmxMsrEntry; VMX_KVM_SWITCH_MSRS.len()] {
        let mut entries = [VmxMsrEntry::default(); VMX_KVM_SWITCH_MSRS.len()];
        let mut i = 0;
        while i < VMX_KVM_SWITCH_MSRS.len() {
            let msr = VMX_KVM_SWITCH_MSRS[i];
            entries[i] = VmxMsrEntry::new(msr as u32, self.read_kvm_msr(msr as u32).unwrap_or(0));
            i += 1;
        }
        entries
    }

    fn host_msr_switch_entries(&self) -> [VmxMsrEntry; VMX_KVM_SWITCH_MSRS.len()] {
        let mut entries = [VmxMsrEntry::default(); VMX_KVM_SWITCH_MSRS.len()];
        let mut i = 0;
        while i < VMX_KVM_SWITCH_MSRS.len() {
            let msr = VMX_KVM_SWITCH_MSRS[i];
            entries[i] = VmxMsrEntry::new(msr as u32, msr.read());
            i += 1;
        }
        entries
    }

    fn sync_msr_switch_entries(&mut self) -> AxResult {
        let guest_entries = self.guest_msr_switch_entries();
        let host_entries = self.host_msr_switch_entries();

        self.vm_entry_msr_load.write_entries(&guest_entries)?;
        self.vm_exit_msr_store.write_entries(&guest_entries)?;
        self.vm_exit_msr_load.write_entries(&host_entries)
    }

    fn capture_guest_msr_store_area(&mut self) {
        let mut i = 0;
        while i < VMX_KVM_SWITCH_MSRS.len() {
            let entry = self.vm_exit_msr_store.read_entry(i);
            self.upsert_kvm_msr(entry.index, entry.data);
            i += 1;
        }
    }
}

// Implementation of private methods
impl VmxVcpu {
    fn setup_io_bitmap(&mut self, config: X86VCpuSetupConfig) -> AxResult {
        // 0x604 is part of the x86 QEMU test contract for reporting test
        // completion. Do not intercept 0xcf9 here: dword PCI config writes to
        // 0xcf8 cover 0xcf9 and must pass through to the q35 host bridge.
        self.io_bitmap
            .set_intercept_of_range(QEMU_EXIT_PORT as _, 2, true);
        self.io_bitmap
            .set_intercept_of_range(X86_PIT_PORT_BASE as _, X86_PIT_PORT_COUNT, true);
        self.io_bitmap
            .set_intercept(X86_PIT_SPEAKER_PORT as _, true);
        if config.emulate_com1 {
            self.io_bitmap.set_intercept_of_range(
                X86_COM1_PORT_BASE as _,
                X86_COM1_PORT_COUNT,
                true,
            );
        }
        let range_count = config
            .pio_intercept_range_count
            .min(config.pio_intercept_ranges.len());
        for range in &config.pio_intercept_ranges[..range_count] {
            self.io_bitmap
                .set_intercept_of_range(range.base as _, range.len as _, true);
            info!(
                "VMX setup pio intercept range base={:#x} len={:#x}",
                range.base, range.len
            );
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn setup_msr_bitmap(&mut self) -> AxResult {
        // Intercept IA32_APIC_BASE MSR accesses.
        const IA32_APIC_BASE: u32 = 0x1b;
        self.msr_bitmap.set_read_intercept(IA32_APIC_BASE, true);
        self.msr_bitmap.set_write_intercept(IA32_APIC_BASE, true);

        // This is strange, guest Linux's access to `IA32_UMWAIT_CONTROL` will cause an exception.
        // But if we intercept it, it seems okay.
        const IA32_UMWAIT_CONTROL: u32 = 0xe1;
        self.msr_bitmap
            .set_write_intercept(IA32_UMWAIT_CONTROL, true);
        self.msr_bitmap
            .set_read_intercept(IA32_UMWAIT_CONTROL, true);

        // Intercept all x2APIC MSR accesses
        for msr in 0x800..=0x83f {
            self.msr_bitmap.set_read_intercept(msr, true);
            self.msr_bitmap.set_write_intercept(msr, true);
        }
        Ok(())
    }

    fn setup_vmcs(
        &mut self,
        entry: GuestPhysAddr,
        ept_root: HostPhysAddr,
        config: X86VCpuSetupConfig,
    ) -> AxResult {
        self.launched = false;
        let paddr = self.vmcs.phys_addr().as_usize() as u64;
        unsafe {
            vmx::vmclear(paddr).map_err(as_axerr)?;
        }
        self.bind_to_current_processor()?;
        self.setup_msr_bitmap()?;
        self.setup_vmcs_guest(entry)?;
        self.setup_vmcs_control(ept_root, true, config)?;
        self.unbind_from_current_processor()?;
        Ok(())
    }

    fn setup_vmcs_host(&self) -> AxResult {
        VmcsHost64::IA32_PAT.write(Msr::IA32_PAT.read())?;
        VmcsHost64::IA32_EFER.write(Msr::IA32_EFER.read())?;

        VmcsHostNW::CR0.write(Cr0::read_raw() as _)?;
        VmcsHostNW::CR3.write(Cr3::read_raw().0.start_address().as_u64() as _)?;
        VmcsHostNW::CR4.write(Cr4::read_raw() as _)?;

        VmcsHost16::ES_SELECTOR.write(x86::segmentation::es().bits())?;
        VmcsHost16::CS_SELECTOR.write(x86::segmentation::cs().bits())?;
        VmcsHost16::SS_SELECTOR.write(x86::segmentation::ss().bits())?;
        VmcsHost16::DS_SELECTOR.write(x86::segmentation::ds().bits())?;
        VmcsHost16::FS_SELECTOR.write(x86::segmentation::fs().bits())?;
        VmcsHost16::GS_SELECTOR.write(x86::segmentation::gs().bits())?;
        VmcsHostNW::FS_BASE.write(Msr::IA32_FS_BASE.read() as _)?;
        VmcsHostNW::GS_BASE.write(Msr::IA32_GS_BASE.read() as _)?;

        let tr = unsafe { x86::task::tr() };
        let mut gdtp = DescriptorTablePointer::<u64>::default();
        let mut idtp = DescriptorTablePointer::<u64>::default();
        unsafe {
            dtables::sgdt(&mut gdtp);
            dtables::sidt(&mut idtp);
        }
        VmcsHost16::TR_SELECTOR.write(tr.bits())?;
        VmcsHostNW::TR_BASE.write(get_tr_base(tr, &gdtp) as _)?;
        VmcsHostNW::GDTR_BASE.write(gdtp.base as _)?;
        VmcsHostNW::IDTR_BASE.write(idtp.base as _)?;
        VmcsHostNW::RIP.write(Self::vmx_exit as *const () as usize)?;

        VmcsHostNW::IA32_SYSENTER_ESP.write(0)?;
        VmcsHostNW::IA32_SYSENTER_EIP.write(0)?;
        VmcsHost32::IA32_SYSENTER_CS.write(0)?;

        Ok(())
    }

    fn setup_vmcs_guest(&mut self, entry: GuestPhysAddr) -> AxResult {
        let cr0_val: Cr0Flags =
            Cr0Flags::NOT_WRITE_THROUGH | Cr0Flags::CACHE_DISABLE | Cr0Flags::EXTENSION_TYPE;
        self.set_cr(0, cr0_val.bits());
        self.set_cr(4, 0);

        macro_rules! set_guest_segment {
            ($seg:ident, $access_rights:expr) => {{
                use VmcsGuest16::*;
                use VmcsGuest32::*;
                use VmcsGuestNW::*;
                paste::paste! {
                    [<$seg _SELECTOR>].write(0)?;
                    [<$seg _BASE>].write(0)?;
                    [<$seg _LIMIT>].write(0xffff)?;
                    [<$seg _ACCESS_RIGHTS>].write($access_rights)?;
                }
            }};
        }

        set_guest_segment!(ES, 0x93); // 16-bit, present, data, read/write, accessed
        set_guest_segment!(CS, 0x9b); // 16-bit, present, code, exec/read, accessed
        set_guest_segment!(SS, 0x93);
        set_guest_segment!(DS, 0x93);
        set_guest_segment!(FS, 0x93);
        set_guest_segment!(GS, 0x93);
        set_guest_segment!(TR, 0x8b); // present, system, 32-bit TSS busy
        set_guest_segment!(LDTR, 0x82); // present, system, LDT

        VmcsGuestNW::GDTR_BASE.write(0)?;
        VmcsGuest32::GDTR_LIMIT.write(0xffff)?;
        VmcsGuestNW::IDTR_BASE.write(0)?;
        VmcsGuest32::IDTR_LIMIT.write(0xffff)?;

        VmcsGuestNW::CR3.write(0)?;
        VmcsGuestNW::DR7.write(0x400)?;
        VmcsGuestNW::RSP.write(0)?;
        VmcsGuestNW::RIP.write(entry.as_usize())?;
        VmcsGuestNW::RFLAGS.write(0x2)?;
        VmcsGuestNW::PENDING_DBG_EXCEPTIONS.write(0)?;
        VmcsGuestNW::IA32_SYSENTER_ESP.write(0)?;
        VmcsGuestNW::IA32_SYSENTER_EIP.write(0)?;
        VmcsGuest32::IA32_SYSENTER_CS.write(0)?;

        VmcsGuest32::INTERRUPTIBILITY_STATE.write(0)?;
        VmcsGuest32::ACTIVITY_STATE.write(0)?;

        VmcsGuest32::VMX_PREEMPTION_TIMER_VALUE.write(VMX_PREEMPTION_TIMER_SET_VALUE)?;

        VmcsGuest64::LINK_PTR.write(u64::MAX)?; // SDM Vol. 3C, Section 24.4.2
        VmcsGuest64::IA32_DEBUGCTL.write(0)?;
        VmcsGuest64::IA32_PAT.write(Msr::IA32_PAT.read())?;
        VmcsGuest64::IA32_EFER.write(0)?;
        Ok(())
    }

    fn write_kvm_state_to_vmcs(&mut self, state: &X86KvmVcpuState) -> AxResult {
        self.set_cr(0, state.cr0);
        self.set_cr(3, state.cr3);
        self.set_cr(4, state.cr4);
        self.upsert_kvm_msr(Msr::IA32_EFER as u32, state.efer);
        self.upsert_kvm_msr(0x1b, state.apic_base);
        self.vlapic.set_apic_base(state.apic_base)?;
        self.write_kvm_segment(0, &state.segments[0])?;
        self.write_kvm_segment(1, &state.segments[1])?;
        self.write_kvm_segment(2, &state.segments[2])?;
        self.write_kvm_segment(3, &state.segments[3])?;
        self.write_kvm_segment(4, &state.segments[4])?;
        self.write_kvm_segment(5, &state.segments[5])?;
        self.write_kvm_segment(6, &state.segments[6])?;
        self.write_kvm_segment(7, &state.segments[7])?;
        VmcsGuestNW::GDTR_BASE.write(state.dtables[0].base as usize)?;
        VmcsGuest32::GDTR_LIMIT.write(state.dtables[0].limit as u32)?;
        VmcsGuestNW::IDTR_BASE.write(state.dtables[1].base as usize)?;
        VmcsGuest32::IDTR_LIMIT.write(state.dtables[1].limit as u32)?;
        VmcsGuestNW::RSP.write(state.rsp as usize)?;
        VmcsGuestNW::RIP.write(state.rip as usize)?;
        VmcsGuestNW::RFLAGS.write((state.rflags | 0x2) as usize)?;
        VmcsGuest64::IA32_EFER.write(state.efer)?;
        self.xstate.guest_xcr0 = state.xcr0;
        let mut i = 0;
        while i < self.kvm_nmsrs {
            let entry = self.kvm_msr_entries[i];
            self.write_kvm_msr(entry.index, entry.data)?;
            i += 1;
        }
        self.sync_entry_controls_for_guest_efer()?;
        Ok(())
    }

    fn sync_entry_controls_for_guest_efer(&self) -> AxResult {
        use super::vmcs::controls::EntryControls as EntryCtrl;

        let efer = VmcsGuest64::IA32_EFER.read()?;
        let is_ia32e_guest = (efer & EferFlags::LONG_MODE_ACTIVE.bits()) != 0;
        let ia32e_guest_bit = EntryCtrl::IA32E_MODE_GUEST.bits();
        let old_value = VmcsControl32::VMENTRY_CONTROLS.read()?;
        let (set, clear) = if is_ia32e_guest {
            (ia32e_guest_bit, 0)
        } else {
            (0, ia32e_guest_bit)
        };

        vmcs::set_control(
            VmcsControl32::VMENTRY_CONTROLS,
            Msr::IA32_VMX_TRUE_ENTRY_CTLS,
            old_value,
            set,
            clear,
        )
    }

    fn write_kvm_segment(&self, segment_id: usize, segment: &X86KvmSegment) -> AxResult {
        let access = kvm_segment_access_rights(segment);
        match segment_id {
            0 => {
                VmcsGuest16::CS_SELECTOR.write(segment.selector)?;
                VmcsGuestNW::CS_BASE.write(segment.base as usize)?;
                VmcsGuest32::CS_LIMIT.write(segment.limit)?;
                VmcsGuest32::CS_ACCESS_RIGHTS.write(access)?;
            }
            1 => {
                VmcsGuest16::DS_SELECTOR.write(segment.selector)?;
                VmcsGuestNW::DS_BASE.write(segment.base as usize)?;
                VmcsGuest32::DS_LIMIT.write(segment.limit)?;
                VmcsGuest32::DS_ACCESS_RIGHTS.write(access)?;
            }
            2 => {
                VmcsGuest16::ES_SELECTOR.write(segment.selector)?;
                VmcsGuestNW::ES_BASE.write(segment.base as usize)?;
                VmcsGuest32::ES_LIMIT.write(segment.limit)?;
                VmcsGuest32::ES_ACCESS_RIGHTS.write(access)?;
            }
            3 => {
                VmcsGuest16::FS_SELECTOR.write(segment.selector)?;
                VmcsGuestNW::FS_BASE.write(segment.base as usize)?;
                VmcsGuest32::FS_LIMIT.write(segment.limit)?;
                VmcsGuest32::FS_ACCESS_RIGHTS.write(access)?;
            }
            4 => {
                VmcsGuest16::GS_SELECTOR.write(segment.selector)?;
                VmcsGuestNW::GS_BASE.write(segment.base as usize)?;
                VmcsGuest32::GS_LIMIT.write(segment.limit)?;
                VmcsGuest32::GS_ACCESS_RIGHTS.write(access)?;
            }
            5 => {
                VmcsGuest16::SS_SELECTOR.write(segment.selector)?;
                VmcsGuestNW::SS_BASE.write(segment.base as usize)?;
                VmcsGuest32::SS_LIMIT.write(segment.limit)?;
                VmcsGuest32::SS_ACCESS_RIGHTS.write(access)?;
            }
            6 => {
                VmcsGuest16::TR_SELECTOR.write(segment.selector)?;
                VmcsGuestNW::TR_BASE.write(segment.base as usize)?;
                VmcsGuest32::TR_LIMIT.write(segment.limit)?;
                VmcsGuest32::TR_ACCESS_RIGHTS.write(access)?;
            }
            7 => {
                VmcsGuest16::LDTR_SELECTOR.write(segment.selector)?;
                VmcsGuestNW::LDTR_BASE.write(segment.base as usize)?;
                VmcsGuest32::LDTR_LIMIT.write(segment.limit)?;
                VmcsGuest32::LDTR_ACCESS_RIGHTS.write(access)?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn setup_vmcs_control(
        &mut self,
        ept_root: HostPhysAddr,
        is_guest: bool,
        config: X86VCpuSetupConfig,
    ) -> AxResult {
        // Intercept NMI and external interrupts.
        use PinbasedControls as PinCtrl;

        use super::vmcs::controls::*;
        let raw_cpuid = CpuId::new();

        vmcs::set_control(
            VmcsControl32::PINBASED_EXEC_CONTROLS,
            Msr::IA32_VMX_TRUE_PINBASED_CTLS,
            Msr::IA32_VMX_PINBASED_CTLS.read() as u32,
            (PinCtrl::NMI_EXITING
                | PinCtrl::EXTERNAL_INTERRUPT_EXITING
                | PinCtrl::VMX_PREEMPTION_TIMER)
                .bits(),
            0,
        )?;

        // Intercept all I/O instructions, use MSR bitmaps, activate secondary controls,
        // disable CR3 load/store interception.
        use PrimaryControls as CpuCtrl;
        vmcs::set_control(
            VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS,
            Msr::IA32_VMX_TRUE_PROCBASED_CTLS,
            Msr::IA32_VMX_PROCBASED_CTLS.read() as u32,
            (CpuCtrl::USE_IO_BITMAPS
                | CpuCtrl::USE_MSR_BITMAPS
                | CpuCtrl::HLT_EXITING
                | CpuCtrl::USE_TPR_SHADOW
                | CpuCtrl::SECONDARY_CONTROLS)
                .bits(),
            (CpuCtrl::CR3_LOAD_EXITING
                | CpuCtrl::CR3_STORE_EXITING
                | CpuCtrl::CR8_LOAD_EXITING
                | CpuCtrl::CR8_STORE_EXITING)
                .bits(),
        )?;

        // Enable EPT, RDTSCP, INVPCID, and unrestricted guest.
        //
        // Deliberately do NOT enable VIRTUAL_INTERRUPT_DELIVERY (VID). We keep
        // VIRTUALIZE_APIC (APIC-access page interception) so guest MMIO reads and
        // writes to the local APIC still VM-exit into the software vLAPIC model,
        // but interrupt injection stays fully software-driven via
        // `vmcs::inject_event` (VM-entry interruption-information field) and EOIs
        // are handled by `handle_apic_access` (which advances RIP past the EOI
        // store). Enabling VID alongside software VM-entry injection is broken:
        // VID's EOI-virtualization consults SVI (GUEST_INTR_STATUS[15:8]), which
        // software injection never populates, so a guest EOI produces a
        // VIRTUALIZED_EOI exit with an empty VISR that never advances the guest.
        // Observed under Firecracker: the guest spins forever in
        // native_apic_mem_eoi (RIP pinned just past the APIC_EOI store) while the
        // periodic LAPIC timer (vector 0xec) is re-injected millions of times.
        // Without VID, the EOI store instead takes an APIC_ACCESS exit that the
        // software path completes and advances past, so the guest leaves its ISR.
        use SecondaryControls as CpuCtrl2;
        let mut val =
            CpuCtrl2::VIRTUALIZE_APIC | CpuCtrl2::ENABLE_EPT | CpuCtrl2::UNRESTRICTED_GUEST;
        // Enable PAUSE-loop exiting *only if the platform allows it*. Under
        // nested virtualization the L0 hypervisor (e.g. QEMU/KVM) may not expose
        // the PLE allowed-1 bit in IA32_VMX_PROCBASED_CTLS2; forcing the bit into
        // set_control would then abort the whole VMCS setup (Unsupported/ENOSYS)
        // and break every VM, oversubscribed or not. So we probe allowed-1 first
        // and fall back silently when PLE is unavailable (the software-spin
        // directed-yield path covers that case). When enabled, the resulting
        // PAUSE_INSTRUCTION exit drives directed-yield, mirroring KVM's PLE.
        let ctls2_cap = Msr::IA32_VMX_PROCBASED_CTLS2.read();
        let ctls2_allowed1 = (ctls2_cap >> 32) as u32;
        let ple_hw_supported = ctls2_allowed1 & CpuCtrl2::PAUSE_LOOP_EXITING.bits() != 0;
        if ple_hw_supported {
            val |= CpuCtrl2::PAUSE_LOOP_EXITING;
        }
        if let Some(features) = raw_cpuid.get_extended_processor_and_feature_identifiers()
            && features.has_rdtscp()
        {
            val |= CpuCtrl2::ENABLE_RDTSCP;
        }
        if let Some(features) = raw_cpuid.get_extended_feature_info()
            && features.has_invpcid()
        {
            val |= CpuCtrl2::ENABLE_INVPCID;
        }
        if let Some(features) = raw_cpuid.get_extended_state_info()
            && features.has_xsaves_xrstors()
        {
            val |= CpuCtrl2::ENABLE_XSAVES_XRSTORS;
        }
        vmcs::set_control(
            VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS,
            Msr::IA32_VMX_PROCBASED_CTLS2,
            Msr::IA32_VMX_PROCBASED_CTLS2.read() as u32,
            val.bits(),
            0,
        )?;

        // Program the PAUSE-loop-exiting window only when PLE was actually
        // enabled above. PLE_GAP is the max cycles between two PAUSEs still
        // counted as part of the same loop; PLE_WINDOW is the loop duration
        // after which a PAUSE takes a VM-exit. Values mirror KVM's defaults
        // (ple_gap=128, ple_window=4096). Fixed (no adaptive grow/shrink).
        let sec_readback = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS
            .read()
            .unwrap_or(0);
        let ple_enabled = sec_readback & CpuCtrl2::PAUSE_LOOP_EXITING.bits() != 0;
        vmx_emerg_write(
            format!(
                "x86_vmx::ple_setup vm={} vcpu={} ctls2_cap={:#x} hw_supported={} sec_readback={:#x} enabled={}\n",
                self.vm_id, self.vcpu_id, ctls2_cap, ple_hw_supported, sec_readback, ple_enabled,
            )
            .as_str(),
        );
        if ple_enabled {
            const PLE_GAP: u32 = 128;
            const PLE_WINDOW: u32 = 4096;
            VmcsControl32::PLE_GAP.write(PLE_GAP)?;
            VmcsControl32::PLE_WINDOW.write(PLE_WINDOW)?;
        }

        // Switch to 64-bit host, acknowledge interrupt info, switch IA32_PAT/IA32_EFER on VM exit.
        use ExitControls as ExitCtrl;
        vmcs::set_control(
            VmcsControl32::VMEXIT_CONTROLS,
            Msr::IA32_VMX_TRUE_EXIT_CTLS,
            Msr::IA32_VMX_EXIT_CTLS.read() as u32,
            (ExitCtrl::HOST_ADDRESS_SPACE_SIZE
                | ExitCtrl::ACK_INTERRUPT_ON_EXIT
                | ExitCtrl::SAVE_IA32_PAT
                | ExitCtrl::LOAD_IA32_PAT
                | ExitCtrl::SAVE_IA32_EFER
                | ExitCtrl::LOAD_IA32_EFER)
                .bits(),
            0,
        )?;

        let mut val = EntryCtrl::LOAD_IA32_PAT | EntryCtrl::LOAD_IA32_EFER;

        if !is_guest {
            // IA-32e mode guest
            // On processors that support Intel 64 architecture, this control determines whether the logical processor is in IA-32e mode after VM entry.
            // Its value is loaded into IA32_EFER.LMA as part of VM entry.
            val |= EntryCtrl::IA32E_MODE_GUEST;
        }

        // Load guest IA32_PAT/IA32_EFER on VM entry.
        use EntryControls as EntryCtrl;
        vmcs::set_control(
            VmcsControl32::VMENTRY_CONTROLS,
            Msr::IA32_VMX_TRUE_ENTRY_CTLS,
            Msr::IA32_VMX_ENTRY_CTLS.read() as u32,
            val.bits(),
            0,
        )?;

        vmcs::set_ept_pointer(ept_root)?;

        VmcsControl64::VMEXIT_MSR_STORE_ADDR
            .write(self.vm_exit_msr_store.phys_addr().as_usize() as _)?;
        VmcsControl64::VMEXIT_MSR_LOAD_ADDR
            .write(self.vm_exit_msr_load.phys_addr().as_usize() as _)?;
        VmcsControl64::VMENTRY_MSR_LOAD_ADDR
            .write(self.vm_entry_msr_load.phys_addr().as_usize() as _)?;
        VmcsControl32::VMEXIT_MSR_STORE_COUNT.write(VMX_KVM_SWITCH_MSRS.len() as _)?;
        VmcsControl32::VMEXIT_MSR_LOAD_COUNT.write(VMX_KVM_SWITCH_MSRS.len() as _)?;
        VmcsControl32::VMENTRY_MSR_LOAD_COUNT.write(VMX_KVM_SWITCH_MSRS.len() as _)?;

        // VmcsControlNW::CR4_GUEST_HOST_MASK.write(0)?;
        VmcsControl32::CR3_TARGET_COUNT.write(0)?;

        // Pass-through exceptions (except #UD(6)), don't use I/O bitmap, set MSR bitmaps.
        let exception_bitmap: u32 = 1 << 6;

        self.setup_io_bitmap(config)?;

        VmcsControl32::EXCEPTION_BITMAP.write(exception_bitmap)?;
        VmcsControl64::IO_BITMAP_A_ADDR.write(self.io_bitmap.phys_addr().0.as_usize() as _)?;
        VmcsControl64::IO_BITMAP_B_ADDR.write(self.io_bitmap.phys_addr().1.as_usize() as _)?;
        VmcsControl64::MSR_BITMAPS_ADDR.write(self.msr_bitmap.phys_addr().as_usize() as _)?;

        VmcsControl64::VIRT_APIC_ADDR.write(self.vlapic.virtual_apic_page_addr().as_usize() as _)?;
        VmcsControl64::APIC_ACCESS_ADDR
            .write(EmulatedLocalApic::virtual_apic_access_addr().as_usize() as _)?;
        if config.enable_eoi_exits {
            // KVM backend mode keeps the software vLAPIC model authoritative.
            // Without EOI exits, APICv may consume guest EOI writes in hardware
            // while the software ISR/TMR state remains stale.
            VmcsControl64::EOI_EXIT0.write(u64::MAX)?;
            VmcsControl64::EOI_EXIT1.write(u64::MAX)?;
            VmcsControl64::EOI_EXIT2.write(u64::MAX)?;
            VmcsControl64::EOI_EXIT3.write(u64::MAX)?;
        } else {
            VmcsControl64::EOI_EXIT0.write(0)?;
            VmcsControl64::EOI_EXIT1.write(0)?;
            VmcsControl64::EOI_EXIT2.write(0)?;
            VmcsControl64::EOI_EXIT3.write(0)?;
        }
        Ok(())
    }

    fn get_paging_level(&self) -> usize {
        let mut level: u32 = 0; // non-paging
        let cr0 = VmcsGuestNW::CR0.read().unwrap();
        let cr4 = VmcsGuestNW::CR4.read().unwrap();
        let efer = VmcsGuest64::IA32_EFER.read().unwrap();
        // paging is enabled
        if cr0 & Cr0Flags::PAGING.bits() as usize != 0 {
            if cr4 & Cr4Flags::PHYSICAL_ADDRESS_EXTENSION.bits() as usize != 0 {
                // is long mode
                if efer & EferFlags::LONG_MODE_ACTIVE.bits() != 0 {
                    level = 4;
                } else {
                    level = 3;
                }
            } else {
                level = 2;
            }
        }
        level as usize
    }
}

// Implementaton for type1.5 hypervisor
// #[cfg(feature = "type1_5")]
impl VmxVcpu {
    fn set_cr(&mut self, cr_idx: usize, val: u64) {
        (|| -> AxResult {
            // debug!("set guest CR{} to val {:#x}", cr_idx, val);
            match cr_idx {
                0 => {
                    // Retrieve/validate restrictions on CR0
                    //
                    // In addition to what the VMX MSRs tell us, make sure that
                    // - NW and CD are kept off as they are not updated on VM exit and we
                    //   don't want them enabled for performance reasons while in root mode
                    // - PE and PG can be freely chosen (by the guest) because we demand
                    //   unrestricted guest mode support anyway
                    // - ET is ignored
                    let must0 = Msr::IA32_VMX_CR0_FIXED1.read()
                        & !(Cr0Flags::NOT_WRITE_THROUGH | Cr0Flags::CACHE_DISABLE).bits();
                    let must1 = Msr::IA32_VMX_CR0_FIXED0.read()
                        & !(Cr0Flags::PAGING | Cr0Flags::PROTECTED_MODE_ENABLE).bits();
                    VmcsGuestNW::CR0.write(((val & must0) | must1) as _)?;
                    VmcsControlNW::CR0_READ_SHADOW.write(val as _)?;
                    VmcsControlNW::CR0_GUEST_HOST_MASK.write((must1 | !must0) as _)?;
                }
                3 => VmcsGuestNW::CR3.write(val as _)?,
                4 => {
                    // Retrieve/validate restrictions on CR4
                    let must0 = Msr::IA32_VMX_CR4_FIXED1.read();
                    let must1 = Msr::IA32_VMX_CR4_FIXED0.read();
                    let val = val | Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS.bits();
                    VmcsGuestNW::CR4.write(((val & must0) | must1) as _)?;
                    VmcsControlNW::CR4_READ_SHADOW.write(val as _)?;
                    VmcsControlNW::CR4_GUEST_HOST_MASK.write((must1 | !must0) as _)?;
                }
                _ => unreachable!(),
            };
            Ok(())
        })()
        .expect("Failed to write guest control register")
    }

    #[allow(dead_code)]
    fn cr(&self, cr_idx: usize) -> usize {
        (|| -> AxResult<usize> {
            Ok(match cr_idx {
                0 => VmcsGuestNW::CR0.read()?,
                3 => VmcsGuestNW::CR3.read()?,
                4 => {
                    let host_mask = VmcsControlNW::CR4_GUEST_HOST_MASK.read()?;
                    (VmcsControlNW::CR4_READ_SHADOW.read()? & host_mask)
                        | (VmcsGuestNW::CR4.read()? & !host_mask)
                }
                _ => unreachable!(),
            })
        })()
        .expect("Failed to read guest control register")
    }
}

/// Get ready then vmlaunch or vmresume.
macro_rules! vmx_entry_with {
    ($instr:literal) => {
        naked_asm!(
            "pushfq",                                  // save host RFLAGS, including IF
            "pop    qword ptr [rdi + {host_rflags}]",
            save_regs_to_stack!(),                      // save host status
            "mov    [rdi + {host_stack_size}], rsp",    // save current RSP to Vcpu::host_stack_top
            "mov    rsp, rdi",                          // set RSP to guest regs area
            restore_regs_from_stack!(),                 // restore guest status
            $instr,                                     // let's go!
            "jmp    {failed}",
            host_stack_size = const size_of::<GeneralRegisters>(),
            host_rflags = const size_of::<GeneralRegisters>() + size_of::<u64>(),
            failed = sym Self::vmx_entry_failed,
            // options(noreturn),
        )
    }
}

impl VmxVcpu {
    #[unsafe(naked)]
    /// Enter guest with vmlaunch.
    ///
    /// `#[naked]` is essential here, without it the rust compiler will think `&mut self` is not used and won't give us correct %rdi.
    ///
    /// This function itself never returns, but [`Self::vmx_exit`] will do the return for this.
    ///
    /// The return value is a dummy value.
    unsafe extern "C" fn vmx_launch(&mut self) -> usize {
        vmx_entry_with!("vmlaunch")
    }

    #[unsafe(naked)]
    /// Enter guest with vmresume.
    ///
    /// See [`Self::vmx_launch`] for detail.
    unsafe extern "C" fn vmx_resume(&mut self) -> usize {
        vmx_entry_with!("vmresume")
    }

    #[unsafe(naked)]
    /// Return after vm-exit. This function is used only for returning from [`Self::vmx_launch`] or [`Self::vmx_resume`].
    ///
    /// NEVER call this function directly.
    ///
    /// The return value is a dummy value.
    unsafe extern "C" fn vmx_exit(&mut self) -> usize {
        // it's not necessary to use another `unsafe` here, as Rust now do not require it in naked functions.
        naked_asm!(
            "cli",                                  // keep host IRQs off until host xstate is restored
            save_regs_to_stack!(),                  // save guest status, after this, rsp points to the `VmxVcpu`
            "mov    rsp, [rsp + {host_stack_top}]", // set RSP to Vcpu::host_stack_top
            restore_regs_from_stack!(),             // restore host status
            "ret",
            host_stack_top = const size_of::<GeneralRegisters>(),
        );
    }

    fn vmx_entry_failed() -> ! {
        panic!("{}", vmcs::instruction_error().as_str())
    }

    /// Whether the guest interrupts are blocked. (SDM Vol. 3C, Section 24.4.2, Table 24-3)
    fn allow_interrupt(&self) -> bool {
        let rflags = VmcsGuestNW::RFLAGS.read().unwrap();
        let block_state = VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap();
        rflags as u64 & x86_64::registers::rflags::RFlags::INTERRUPT_FLAG.bits() != 0
            && block_state == 0
    }

    /// Try to inject a pending event before next VM entry.
    fn inject_pending_events(&mut self) -> AxResult {
        vmcs::clear_injected_event()?;
        // Pick which queued event to deliver this entry. Only one event can be
        // injected per VM entry, so a naive front()-only policy lets a vector
        // that keeps re-arriving (e.g. PIT IRQ0 0x30, re-queued on every
        // preemption-timer exit) permanently sit ahead of an equally-pending
        // higher-priority interrupt (e.g. the LAPIC timer 0xec that
        // `calibrate_APIC_clock` spins on). That starves 0xec and hangs guest
        // calibration. Select by x86 interrupt priority instead: a pending
        // exception (vector < 32) always wins and is already kept at the queue
        // head; otherwise, when interrupts are allowed, deliver the highest
        // vector pending, which matches LAPIC priority and keeps 0x30/0xec fair.
        let interruptible = self.allow_interrupt();
        let selected = {
            let pending_events = self.pending_events.lock();
            let pending_len = pending_events.len();
            let front = pending_events.front().copied();
            let choice = match front {
                // A queued exception is always the head and must go first.
                Some(front_event) if front_event.vector < 32 => Some(front_event),
                // Maskable interrupts: only selectable when interrupts are open.
                _ if interruptible => pending_events
                    .iter()
                    .copied()
                    .max_by_key(|event| event.vector),
                // Interrupts blocked: keep the head so it drives interrupt-window
                // exiting below.
                _ => front,
            };
            choice.map(|event| (event, pending_len))
        };
        if let Some((event, pending_len)) = selected {
            if event.vector < 32 || self.allow_interrupt() {
                // if it's an exception, or an interrupt that is not blocked, inject it directly.
                let count = VMX_INJECT_EVENT_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if count <= 32 || count.is_power_of_two() {
                    vmx_emerg_write(
                        format!(
                            "x86_vmx::inject_event vector={:#x} level={} pending_before={} rflags={:#x} int_state={:#x} count={}\n",
                            event.vector,
                            event.level_triggered,
                            pending_len,
                            VmcsGuestNW::RFLAGS.read().unwrap_or(0),
                            VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap_or(0),
                            count
                        )
                        .as_str(),
                    );
                    info!(
                        "x86 vmx inject_pending_event vector={:#x} err_code={:?} level_triggered={} pending_before={} count={}",
                        event.vector,
                        event.err_code,
                        event.level_triggered,
                        pending_len,
                        count
                    );
                }
                if let Some(vector_count) =
                    vmx_should_log_vector(event.vector, &VMX_VECTOR20_INJECT_LOG_COUNT, 32)
                {
                    vmx_emerg_write(
                        format!(
                            "x86_vmx::inject_vector20 level={} pending_before={} rflags={:#x} int_state={:#x} global_count={} vector_count={}\n",
                            event.level_triggered,
                            pending_len,
                            VmcsGuestNW::RFLAGS.read().unwrap_or(0),
                            VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap_or(0),
                            count,
                            vector_count
                        )
                        .as_str(),
                    );
                }
                // Unconditional trace for high-priority IPI vectors (>=0xf0:
                // RESCHEDULE 0xfd, CALL_FUNCTION 0xfb/0xf9, TLB flush, etc.).
                // The gated logging above (count<=32||pow2) undersamples: the
                // runtime CSD/cpuhp freeze happens thousands of injects in, so
                // we must be able to answer "was the CALL_FUNCTION IPI ever
                // actually injected into the target vCPU?" without sampling.
                if event.vector >= 0xf0 {
                    vmx_emerg_write(
                        format!(
                            "x86_vmx::inject_ipi vm={} vcpu={} vector={:#x} pending_before={}\n",
                            self.vm_id, self.vcpu_id, event.vector, pending_len
                        )
                        .as_str(),
                    );
                }
                vmcs::inject_event(event.vector, event.err_code)?;
                if event.vector >= 32 {
                    self.vlapic
                        .accept_interrupt(event.vector, event.level_triggered);
                }
                // The selected event may not be at the queue head (priority
                // selection above can pick a higher-vector interrupt sitting
                // behind a re-queued lower-vector one), so remove by match.
                self.pop_pending_event_matching(event);
            } else {
                // interrupts are blocked, enable interrupt-window exiting.
                let count = VMX_BLOCKED_EVENT_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
                let block_state = VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap_or(0);
                if count <= 32 || count.is_power_of_two() {
                    vmx_emerg_write(
                        format!(
                            "x86_vmx::event_blocked vector={:#x} pending={} rflags={:#x} int_state={:#x} count={}\n",
                            event.vector,
                            pending_len,
                            rflags,
                            block_state,
                            count
                        )
                        .as_str(),
                    );
                    info!(
                        "x86 vmx pending_event_blocked vector={:#x} pending={} rflags={:#x} interruptibility={:#x} count={}",
                        event.vector,
                        pending_len,
                        rflags,
                        block_state,
                        count
                    );
                }
                if let Some(vector_count) =
                    vmx_should_log_vector(event.vector, &VMX_VECTOR20_BLOCKED_LOG_COUNT, 32)
                {
                    vmx_emerg_write(
                        format!(
                            "x86_vmx::blocked_vector20 pending={} rflags={:#x} int_state={:#x} global_count={} vector_count={}\n",
                            pending_len,
                            rflags,
                            block_state,
                            count,
                            vector_count
                        )
                        .as_str(),
                    );
                }
                self.set_interrupt_window(true)?;
            }
        }
        Ok(())
    }

    fn handle_interrupt_window(&mut self) -> AxResult {
        let count = VMX_INTERRUPT_WINDOW_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 32 || count.is_power_of_two() {
            vmx_emerg_write(
                format!(
                    "x86_vmx::interrupt_window vm={} vcpu={} rflags={:#x} int_state={:#x} pending={} count={}\n",
                    self.vm_id,
                    self.vcpu_id,
                    VmcsGuestNW::RFLAGS.read().unwrap_or(usize::MAX),
                    VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap_or(u32::MAX),
                    self.pending_event_count(),
                    count,
                )
                .as_str(),
            );
        }
        self.set_interrupt_window(false)?;
        // Do NOT inject here. An interrupt-window exit is handled internally and
        // makes `inner_run` return `Nothing`, after which the vCPU run loop calls
        // `inner_run` again; its own `inject_pending_events()` (top of inner_run)
        // performs the single, canonical injection for the upcoming VM entry.
        //
        // If we injected here as well, we would arm VMENTRY_INTERRUPTION_INFO with
        // the selected vector AND pop it from the pending queue / mark it in-service
        // in the vlapic -- but the subsequent `inner_run` re-runs
        // `inject_pending_events`, which starts with `clear_injected_event()` and
        // then re-selects from the (now shorter) queue. The event committed here is
        // silently discarded: its VMENTRY field is wiped and a different pending
        // vector is injected instead, yet the guest already "consumed" the first
        // event from the software queue. This is exactly how the Firecracker
        // 2-vCPU CALL_FUNCTION IPI (0xfb) was lost: injected in the interrupt-window
        // handler, popped + accepted, then overwritten by the timer 0xec on the
        // immediately following entry, so CPU1 never ran its CSD handler and CPU0
        // spun forever in csd_lock_wait.
        Ok(())
    }

    /// Handle vm-exits than can and should be handled by [`VmxVcpu`] itself.
    ///
    /// Return the result or None if the vm-exit was not handled.
    fn builtin_vmexit_handler(&mut self, exit_info: &VmxExitInfo) -> Option<AxResult> {
        const APIC_BASE_MSR: u32 = 0x1b;
        const AMD64_DE_CFG: u32 = 0xc001_1029;
        // Following vm-exits are handled here:
        // - interrupt window: turn off interrupt window;
        // - xsetbv: set guest xcr;
        // - cr access: just panic;
        match exit_info.exit_reason {
            VmxExitReason::INTERRUPT_WINDOW => Some(self.handle_interrupt_window()),
            VmxExitReason::XSETBV => Some(self.handle_xsetbv()),
            VmxExitReason::CR_ACCESS => Some(self.handle_cr()),
            VmxExitReason::CPUID => Some(self.handle_cpuid()),
            msr_rw @ (VmxExitReason::MSR_READ | VmxExitReason::MSR_WRITE)
                if self.regs().rcx as u32 == APIC_BASE_MSR =>
            {
                Some(self.handle_apic_base_msr_access(msr_rw == VmxExitReason::MSR_WRITE))
            }
            msr_rw @ (VmxExitReason::MSR_READ | VmxExitReason::MSR_WRITE)
                if {
                    let msr = self.regs().rcx as u32;
                    (X2APIC_MSR_BASE..=X2APIC_MSR_END).contains(&msr)
                } =>
            {
                if msr_rw == VmxExitReason::MSR_WRITE
                    && (self.regs().rcx as u32 == X2APIC_EOI_MSR
                        || self.regs().rcx as u32 == X2APIC_ICR_MSR)
                {
                    return None;
                }
                Some(self.handle_apic_msr_access(
                    msr_rw == VmxExitReason::MSR_WRITE,
                    self.regs().rcx as u32,
                ))
            }
            msr_rw @ (VmxExitReason::MSR_READ | VmxExitReason::MSR_WRITE)
                if self.regs().rcx as u32 == AMD64_DE_CFG =>
            {
                Some(self.handle_amd64_de_cfg_msr_access(msr_rw == VmxExitReason::MSR_WRITE))
            }
            VmxExitReason::MSR_READ => Some(self.handle_kvm_msr_read()),
            VmxExitReason::MSR_WRITE => Some(self.handle_kvm_msr_write()),
            _ => None,
        }
    }

    /// Read a 64-bit value from EDX:EAX.
    fn read_edx_eax(&self) -> u64 {
        ((self.regs().rdx & 0xffff_ffff) << 32) | (self.regs().rax & 0xffff_ffff)
    }

    /// Write a 64-bit value to EDX:EAX.
    fn write_edx_eax(&mut self, val: u64) {
        self.regs_mut().rax = val & 0xffff_ffff;
        self.regs_mut().rdx = val >> 32;
    }

    fn find_kvm_msr_index(&self, msr: u32) -> Option<usize> {
        let mut i = 0;
        while i < self.kvm_nmsrs {
            if self.kvm_msr_entries[i].index == msr {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    fn read_kvm_msr(&self, msr: u32) -> AxResult<u64> {
        if let Some(index) = self.find_kvm_msr_index(msr) {
            return Ok(self.kvm_msr_entries[index].data);
        }

        match msr {
            x if x == Msr::IA32_EFER as u32 => VmcsGuest64::IA32_EFER.read(),
            x if x == Msr::IA32_PAT as u32 => VmcsGuest64::IA32_PAT.read(),
            x if x == Msr::IA32_FS_BASE as u32 => Ok(VmcsGuestNW::FS_BASE.read()? as u64),
            x if x == Msr::IA32_GS_BASE as u32 => Ok(VmcsGuestNW::GS_BASE.read()? as u64),
            x if x == Msr::IA32_SYSENTER_CS as u32 => {
                Ok(VmcsGuest32::IA32_SYSENTER_CS.read()? as u64)
            }
            x if x == Msr::IA32_SYSENTER_ESP as u32 => {
                Ok(VmcsGuestNW::IA32_SYSENTER_ESP.read()? as u64)
            }
            x if x == Msr::IA32_SYSENTER_EIP as u32 => {
                Ok(VmcsGuestNW::IA32_SYSENTER_EIP.read()? as u64)
            }
            _ => Ok(0),
        }
    }

    fn upsert_kvm_msr(&mut self, msr: u32, value: u64) {
        if let Some(index) = self.find_kvm_msr_index(msr) {
            self.kvm_msr_entries[index].data = value;
            return;
        }
        if self.kvm_nmsrs < X86_KVM_MAX_MSR_ENTRIES {
            self.kvm_msr_entries[self.kvm_nmsrs] = X86KvmMsrEntry {
                index: msr,
                data: value,
            };
            self.kvm_nmsrs += 1;
        }
    }

    fn write_kvm_msr(&mut self, msr: u32, value: u64) -> AxResult {
        match msr {
            x if x == Msr::IA32_EFER as u32 => VmcsGuest64::IA32_EFER.write(value)?,
            x if x == Msr::IA32_PAT as u32 => VmcsGuest64::IA32_PAT.write(value)?,
            x if x == Msr::IA32_FS_BASE as u32 => VmcsGuestNW::FS_BASE.write(value as usize)?,
            x if x == Msr::IA32_GS_BASE as u32 => VmcsGuestNW::GS_BASE.write(value as usize)?,
            x if x == Msr::IA32_SYSENTER_CS as u32 => {
                VmcsGuest32::IA32_SYSENTER_CS.write(value as u32)?
            }
            x if x == Msr::IA32_SYSENTER_ESP as u32 => {
                VmcsGuestNW::IA32_SYSENTER_ESP.write(value as usize)?
            }
            x if x == Msr::IA32_SYSENTER_EIP as u32 => {
                VmcsGuestNW::IA32_SYSENTER_EIP.write(value as usize)?
            }
            MSR_KVM_SYSTEM_TIME_NEW | MSR_KVM_WALL_CLOCK_NEW => {
                // Forward to the host so it records the shared-page GPA and
                // populates it. The guest reads time from that page, immune to
                // per-tick timer-IRQ starvation under oversubscription.
                axvisor_api::vmm::pvclock_write(self.vm_id, self.vcpu_id, msr, value);
            }
            _ => {}
        }
        self.upsert_kvm_msr(msr, value);
        Ok(())
    }

    fn handle_kvm_msr_read(&mut self) -> AxResult {
        let msr = self.regs().rcx as u32;
        let value = self.read_kvm_msr(msr)?;
        self.write_edx_eax(value);
        self.advance_rip(VMEXIT_INSTR_LEN_RDMSR_WRMSR)
    }

    fn handle_kvm_msr_write(&mut self) -> AxResult {
        let msr = self.regs().rcx as u32;
        let value = self.read_edx_eax();
        self.write_kvm_msr(msr, value)?;
        self.advance_rip(VMEXIT_INSTR_LEN_RDMSR_WRMSR)
    }

    fn handle_apic_base_msr_access(&mut self, write: bool) -> AxResult {
        self.advance_rip(VMEXIT_INSTR_LEN_RDMSR_WRMSR)?;

        if write {
            let value = self.read_edx_eax();
            trace!("handle_vlapic_apic_base_write: value={value:#x}");
            self.vlapic.set_apic_base(value)
        } else {
            let value = self.vlapic.apic_base();
            trace!("handle_vlapic_apic_base_read: value={value:#x}");
            self.write_edx_eax(value);
            Ok(())
        }
    }

    fn handle_apic_msr_access(&mut self, write: bool, msr: u32) -> AxResult {
        self.advance_rip(VMEXIT_INSTR_LEN_RDMSR_WRMSR)?;

        let msr = msr as _;
        if write {
            let value = self.read_edx_eax() as usize;

            trace!("handle_vlapic_msr_write: msr={msr:#x}, value={value:#x}");

            <EmulatedLocalApic as BaseDeviceOps<SysRegAddrRange>>::handle_write(
                &self.vlapic,
                SysRegAddr::new(msr),
                AccessWidth::Qword,
                value,
            )
        } else {
            let value = <EmulatedLocalApic as BaseDeviceOps<SysRegAddrRange>>::handle_read(
                &self.vlapic,
                SysRegAddr::new(msr),
                AccessWidth::Qword,
            )? as u64;
            if msr == X2APIC_ID_MSR as usize {
                let count = VMX_X2APIC_ID_READ_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if count <= 16 || count.is_power_of_two() {
                    vmx_emerg_write(
                        format!(
                            "x86_vmx::x2apic_id_read vm={} vcpu={} value={:#x} apic_base={:#x} count={}\n",
                            self.vm_id,
                            self.vcpu_id,
                            value,
                            self.vlapic.apic_base(),
                            count
                        )
                        .as_str(),
                    );
                }
            }

            trace!("handle_vlapic_msr_read: msr={msr:#x}, value={value:#x}");

            self.write_edx_eax(value);
            Ok(())
        }
    }

    fn handle_amd64_de_cfg_msr_access(&mut self, write: bool) -> AxResult {
        self.advance_rip(VMEXIT_INSTR_LEN_RDMSR_WRMSR)?;
        if !write {
            self.write_edx_eax(0);
        }
        Ok(())
    }

    fn handle_apic_access(&mut self, exit_info: &VmxExitInfo) -> AxResult<AxVCpuExitReason> {
        let apic_access_exit_info = self.apic_access_exit_info()?;

        let write = match apic_access_exit_info.access_type {
            ApicAccessExitType::LinearDataWrite => true,
            ApicAccessExitType::LinearDataRead => false,
            _ => {
                warn!(
                    "Unsupported APIC access type: {:?}",
                    apic_access_exit_info.access_type
                );
                return ax_err!(BadState, "Unsupported APIC access type");
            }
        };

        let reg = apic_access_exit_info.offset as usize;
        let addr = GuestPhysAddr::from(X86_APIC_ACCESS_GPA + reg);
        let mut exit_reason = AxVCpuExitReason::Nothing;
        if write {
            let value = self.decode_apic_mmio_write_value(exit_info)?;
            let count = VMX_APIC_ACCESS_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if count <= 128 || count.is_power_of_two() {
                vmx_emerg_write(
                    format!(
                        "x86_vmx::apic_access vm={} vcpu={} write offset={:#x} value={:#x} rip={:#x} len={} count={}\n",
                        self.vm_id,
                        self.vcpu_id,
                        reg,
                        value,
                        exit_info.guest_rip,
                        exit_info.exit_instruction_length,
                        count,
                    )
                    .as_str(),
                );
            }
            if reg == X86_LOCAL_APIC_EOI_OFFSET {
                exit_reason = AxVCpuExitReason::InterruptEnd {
                    vector: self.vlapic.handle_eoi(),
                };
            } else {
                <EmulatedLocalApic as BaseDeviceOps<AddrRange<GuestPhysAddr>>>::handle_write(
                    &self.vlapic,
                    addr,
                    AccessWidth::Dword,
                    value,
                )?;
                if let Some(cpu_up) = self.vlapic.take_pending_cpu_up() {
                    exit_reason = AxVCpuExitReason::CpuUp {
                        target_cpu: cpu_up.target_cpu,
                        entry_point: cpu_up.entry_point,
                        arg: 0,
                    };
                }
            }
        } else {
            let value =
                <EmulatedLocalApic as BaseDeviceOps<AddrRange<GuestPhysAddr>>>::handle_read(
                    &self.vlapic,
                    addr,
                    AccessWidth::Dword,
                )?;
            let count = VMX_APIC_ACCESS_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if count <= 128 || count.is_power_of_two() {
                vmx_emerg_write(
                    format!(
                        "x86_vmx::apic_access vm={} vcpu={} read offset={:#x} value={:#x} rip={:#x} len={} count={}\n",
                        self.vm_id,
                        self.vcpu_id,
                        reg,
                        value,
                        exit_info.guest_rip,
                        exit_info.exit_instruction_length,
                        count,
                    )
                    .as_str(),
                );
            }
            self.regs_mut().rax = value as u64;
        }

        self.advance_rip(exit_info.exit_instruction_length as _)?;
        Ok(exit_reason)
    }

    fn decode_apic_mmio_write_value(&self, exit_info: &VmxExitInfo) -> AxResult<usize> {
        let mut rip = self.gla2gva(GuestVirtAddr::from(exit_info.guest_rip));
        let mut rex = 0u8;

        Self::skip_simple_prefixes(self, &mut rip, &mut rex)?;

        let opcode = self.read_guest_u8(rip)?;
        rip += 1;
        let modrm = self.read_guest_u8(rip)?;
        rip += 1;
        let mode = modrm >> 6;
        if mode == 0b11 {
            return ax_err!(Unsupported, "APIC MMIO write destination is not memory");
        }

        if opcode == 0x89 {
            let reg = ((modrm >> 3) & 0x7) | ((rex & 0x4) << 1);
            return Ok(self.guest_regs.get_reg_of_index(reg) as u32 as usize);
        }

        if opcode == 0xc7 && (modrm >> 3) & 0x7 == 0 {
            let imm_addr = self.skip_modrm_memory_operand(rip, modrm, rex)?;
            let mut value = 0u32;
            for i in 0..size_of::<u32>() {
                value |= (self.read_guest_u8(imm_addr + i)? as u32) << (i * 8);
            }
            return Ok(value as usize);
        }

        ax_err!(
            Unsupported,
            format_args!("unsupported APIC MMIO write opcode {opcode:#x}")
        )
    }

    fn decode_ept_mmio_access(
        &self,
        exit_info: &VmxExitInfo,
        addr: GuestPhysAddr,
        write: bool,
    ) -> Option<AxVCpuExitReason> {
        if !(X86_IOAPIC_BASE..X86_IOAPIC_BASE + X86_IOAPIC_SIZE).contains(&addr.as_usize()) {
            return None;
        }

        let mut rip = self.gla2gva(GuestVirtAddr::from(exit_info.guest_rip));
        let mut rex = 0u8;
        if let Err(err) = Self::skip_simple_prefixes(self, &mut rip, &mut rex) {
            debug!("failed to decode EPT MMIO prefixes: {err:?}");
            return None;
        }

        let opcode = self.read_guest_u8(rip).ok()?;
        rip += 1;
        let modrm = self.read_guest_u8(rip).ok()?;
        rip += 1;
        if modrm >> 6 == 0b11 {
            debug!("EPT MMIO access did not use a memory operand");
            return None;
        }

        match (write, opcode) {
            (true, 0x89) => {
                let reg = ((modrm >> 3) & 0x7) | ((rex & 0x4) << 1);
                Some(AxVCpuExitReason::MmioWrite {
                    addr,
                    width: AccessWidth::Dword,
                    data: self.guest_regs.get_reg_of_index(reg) as u32 as u64,
                })
            }
            (true, 0xc7) if (modrm >> 3) & 0x7 == 0 => {
                let imm_addr = self.skip_modrm_memory_operand(rip, modrm, rex).ok()?;
                let mut data = 0u32;
                for i in 0..size_of::<u32>() {
                    data |= (self.read_guest_u8(imm_addr + i).ok()? as u32) << (i * 8);
                }
                Some(AxVCpuExitReason::MmioWrite {
                    addr,
                    width: AccessWidth::Dword,
                    data: data as u64,
                })
            }
            (false, 0x8b) => {
                let reg = (((modrm >> 3) & 0x7) | ((rex & 0x4) << 1)) as usize;
                Some(AxVCpuExitReason::MmioRead {
                    addr,
                    width: AccessWidth::Dword,
                    reg,
                    reg_width: AccessWidth::Dword,
                    signed_ext: false,
                })
            }
            _ => {
                debug!("unsupported EPT MMIO opcode {opcode:#x}, write={write}");
                None
            }
        }
    }

    fn skip_simple_prefixes(&self, rip: &mut GuestVirtAddr, rex: &mut u8) -> AxResult {
        loop {
            let byte = self.read_guest_u8(*rip)?;
            if byte == 0x66 {
                *rip += 1;
            } else if (0x40..=0x4f).contains(&byte) {
                *rex = byte;
                *rip += 1;
            } else {
                return Ok(());
            }
        }
    }

    fn skip_modrm_memory_operand(
        &self,
        mut cursor: GuestVirtAddr,
        modrm: u8,
        rex: u8,
    ) -> AxResult<GuestVirtAddr> {
        let mode = modrm >> 6;
        let rm = modrm & 0x7;

        if rm == 0b100 {
            let sib = self.read_guest_u8(cursor)?;
            cursor += 1;
            let base = sib & 0x7;
            if mode == 0 && base == 0b101 {
                cursor += size_of::<u32>();
            }
        } else if mode == 0 && rm == 0b101 && rex & 0x1 == 0 {
            cursor += size_of::<u32>();
        }

        match mode {
            0 => {}
            1 => cursor += size_of::<u8>(),
            2 => cursor += size_of::<u32>(),
            _ => return ax_err!(InvalidInput, "ModRM register operand is not memory"),
        }

        Ok(cursor)
    }

    fn read_guest_u8(&self, gva: GuestVirtAddr) -> AxResult<u8> {
        let gpa = self.translate_guest_linear(gva)?;
        let hpa = self.translate_guest_phys_to_host_phys(gpa)?;
        let hva = axvisor_api::memory::phys_to_virt(hpa);
        Ok(unsafe { core::ptr::read_volatile(hva.as_ptr()) })
    }

    fn read_guest_phys_u64(&self, gpa: usize) -> AxResult<u64> {
        let hpa = self.translate_guest_phys_to_host_phys(GuestPhysAddr::from(gpa))?;
        let hva = axvisor_api::memory::phys_to_virt(hpa);
        Ok(unsafe { core::ptr::read_volatile(hva.as_ptr() as *const u64) })
    }

    fn translate_guest_phys_to_host_phys(&self, gpa: GuestPhysAddr) -> AxResult<HostPhysAddr> {
        const EPT_PRESENT: u64 = 0x7;
        const HUGE_PAGE: u64 = 1 << 7;
        const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
        const PAGE_4K_MASK: usize = 0xfff;
        const PAGE_2M_MASK: usize = 0x1f_ffff;
        const PAGE_1G_MASK: usize = 0x3fff_ffff;

        let mut table = self
            .ept_root
            .ok_or_else(|| ax_err_type!(BadState, "EPT root is not configured"))?
            .as_usize();
        let addr = gpa.as_usize();
        let indexes = [
            (addr >> 39) & 0x1ff,
            (addr >> 30) & 0x1ff,
            (addr >> 21) & 0x1ff,
            (addr >> 12) & 0x1ff,
        ];

        for (level, index) in indexes.into_iter().enumerate() {
            let entry_hpa = HostPhysAddr::from(table + index * size_of::<u64>());
            let entry_hva = axvisor_api::memory::phys_to_virt(entry_hpa);
            let entry = unsafe { core::ptr::read_volatile(entry_hva.as_ptr() as *const u64) };
            if entry & EPT_PRESENT == 0 {
                return ax_err!(
                    InvalidInput,
                    format_args!(
                        "EPT entry is not present at level {level}, gpa={:#x}",
                        addr
                    )
                );
            }

            let hpa = (entry & ADDR_MASK) as usize;
            match level {
                1 if entry & HUGE_PAGE != 0 => {
                    return Ok(HostPhysAddr::from(hpa + (addr & PAGE_1G_MASK)));
                }
                2 if entry & HUGE_PAGE != 0 => {
                    return Ok(HostPhysAddr::from(hpa + (addr & PAGE_2M_MASK)));
                }
                3 => return Ok(HostPhysAddr::from(hpa + (addr & PAGE_4K_MASK))),
                _ => table = hpa,
            }
        }

        ax_err!(InvalidInput, "failed to translate guest physical address through EPT")
    }

    fn translate_guest_linear(&self, gva: GuestVirtAddr) -> AxResult<GuestPhysAddr> {
        let addr = gva.as_usize();
        match self.get_paging_level() {
            0 => Ok(GuestPhysAddr::from(addr)),
            4 => self.walk_guest_page_table_4level(addr),
            level => ax_err!(
                Unsupported,
                format_args!("unsupported MMIO decode paging level {level}")
            ),
        }
    }

    fn walk_guest_page_table_4level(&self, gva: usize) -> AxResult<GuestPhysAddr> {
        const PRESENT: u64 = 1 << 0;
        const HUGE_PAGE: u64 = 1 << 7;
        const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
        const PAGE_4K_MASK: usize = 0xfff;
        const PAGE_2M_MASK: usize = 0x1f_ffff;
        const PAGE_1G_MASK: usize = 0x3fff_ffff;

        let mut table = VmcsGuestNW::CR3.read()? & ADDR_MASK as usize;
        let indexes = [
            (gva >> 39) & 0x1ff,
            (gva >> 30) & 0x1ff,
            (gva >> 21) & 0x1ff,
            (gva >> 12) & 0x1ff,
        ];

        for (level, index) in indexes.into_iter().enumerate() {
            let entry = self.read_guest_phys_u64(table + index * size_of::<u64>())?;
            if entry & PRESENT == 0 {
                return ax_err!(
                    InvalidInput,
                    format_args!("guest RIP page table entry is not present at level {level}")
                );
            }

            let paddr = (entry & ADDR_MASK) as usize;
            match level {
                1 if entry & HUGE_PAGE != 0 => {
                    return Ok(GuestPhysAddr::from(paddr + (gva & PAGE_1G_MASK)));
                }
                2 if entry & HUGE_PAGE != 0 => {
                    return Ok(GuestPhysAddr::from(paddr + (gva & PAGE_2M_MASK)));
                }
                3 => return Ok(GuestPhysAddr::from(paddr + (gva & PAGE_4K_MASK))),
                _ => table = paddr,
            }
        }

        ax_err!(InvalidInput, "failed to translate guest RIP")
    }

    fn handle_vmx_preemption_timer(&mut self) -> AxResult {
        // The VMX-preemption timer counts down at rate proportional to that of the timestamp counter (TSC).
        // Specifically, the timer counts down by 1 every time bit X in the TSC changes due to a TSC increment.
        // The value of X is in the range 0–31 and can be determined by consulting the VMX capability MSR IA32_VMX_MISC (see Appendix A.6).
        VmcsGuest32::VMX_PREEMPTION_TIMER_VALUE.write(VMX_PREEMPTION_TIMER_SET_VALUE)?;
        Ok(())
    }

    #[allow(clippy::single_match)]
    fn handle_cr(&mut self) -> AxResult {
        const VM_EXIT_INSTR_LEN_MV_TO_CR: u8 = 3;

        let cr_access_info = vmcs::cr_access_info()?;

        let reg = cr_access_info.gpr;
        let cr = cr_access_info.cr_number;

        match cr_access_info.access_type {
            // move to cr
            0 => {
                let val = if reg == 4 {
                    self.stack_pointer() as u64
                } else {
                    self.guest_regs.get_reg_of_index(reg)
                };
                if cr == 0 || cr == 4 {
                    self.advance_rip(VM_EXIT_INSTR_LEN_MV_TO_CR)?;
                    // TODO: check for #GP reasons
                    self.set_cr(cr as usize, val);

                    if cr == 0 && Cr0Flags::from_bits_truncate(val).contains(Cr0Flags::PAGING) {
                        vmcs::update_efer()?;
                    }
                    return Ok(());
                }
            }
            _ => {}
        };

        panic!(
            "Guest's access to cr not allowed: {:#x?}, {:#x?}",
            self, cr_access_info
        );
    }

    fn find_kvm_cpuid_entry(&self, function: u32, index: u32) -> Option<X86KvmCpuidEntry> {
        let mut fallback = None;
        let mut i = 0;
        while i < self.kvm_cpuid_nent {
            let entry = self.kvm_cpuid_entries[i];
            if entry.function == function {
                if entry.flags & KVM_CPUID_FLAG_SIGNIFICANT_INDEX != 0 {
                    if entry.index == index {
                        return Some(entry);
                    }
                } else if fallback.is_none() {
                    fallback = Some(entry);
                }
            }
            i += 1;
        }
        fallback
    }

    fn fixup_kvm_cpuid_identity(
        &self,
        function: u32,
        index: u32,
        res: &mut raw_cpuid::CpuIdResult,
    ) {
        const LEAF_FEATURE_INFO: u32 = 0x1;
        const LEAF_X2APIC_TOPOLOGY: u32 = 0xb;
        const LEAF_V2_EXTENDED_TOPOLOGY: u32 = 0x1f;
        const INITIAL_APIC_ID_MASK: u32 = 0xff << 24;
        const FEATURE_TSC_DEADLINE_TIMER: u32 = 1 << 24;

        let apic_id = self.vcpu_id as u32;
        match function {
            LEAF_FEATURE_INFO => {
                // The KVM ABI path may receive host CPUID leaves from Firecracker.
                // Do not advertise TSC-deadline timer until IA32_TSC_DEADLINE is virtualized.
                res.ecx &= !FEATURE_TSC_DEADLINE_TIMER;
                res.ebx = (res.ebx & !INITIAL_APIC_ID_MASK) | ((apic_id & 0xff) << 24);
            }
            LEAF_X2APIC_TOPOLOGY | LEAF_V2_EXTENDED_TOPOLOGY => {
                if index == 0 || res.ebx != 0 {
                    res.edx = apic_id;
                }
            }
            _ => {}
        }
    }

    fn handle_cpuid(&mut self) -> AxResult {
        use raw_cpuid::{CpuIdResult, cpuid};

        const VM_EXIT_INSTR_LEN_CPUID: u8 = 2;
        const LEAF_FEATURE_INFO: u32 = 0x1;
        const LEAF_STRUCTURED_EXTENDED_FEATURE_FLAGS_ENUMERATION: u32 = 0x7;
        const LEAF_PROCESSOR_EXTENDED_STATE_ENUMERATION: u32 = 0xd;
        const EAX_FREQUENCY_INFO: u32 = 0x16;
        const LEAF_HYPERVISOR_INFO: u32 = 0x4000_0000;
        const LEAF_HYPERVISOR_FEATURE: u32 = 0x4000_0001;
        const VENDOR_STR: &[u8; 12] = b"RVMRVMRVMRVM";
        let vendor_regs = unsafe { &*(VENDOR_STR.as_ptr() as *const [u32; 3]) };

        let regs_clone = *self.regs_mut();
        let function = regs_clone.rax as u32;
        let mut res = if let Some(entry) =
            self.find_kvm_cpuid_entry(function, regs_clone.rcx as u32)
        {
            CpuIdResult {
                eax: entry.eax,
                ebx: entry.ebx,
                ecx: entry.ecx,
                edx: entry.edx,
            }
        } else {
            match function {
            LEAF_FEATURE_INFO => {
                const FEATURE_VMX: u32 = 1 << 5;
                const FEATURE_HYPERVISOR: u32 = 1 << 31;
                const FEATURE_TSC_DEADLINE_TIMER: u32 = 1 << 24;
                const FEATURE_MCE: u32 = 1 << 7;
                const FEATURE_X2APIC: u32 = 1 << 21;
                const FEATURE_APIC: u32 = 1 << 9;
                const MAX_LOGICAL_PROCESSORS_MASK: u32 = 0xff << 16;
                const INITIAL_APIC_ID_MASK: u32 = 0xff << 24;
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                res.ecx &= !FEATURE_VMX;
                res.ecx |= FEATURE_X2APIC;
                res.ecx &= !FEATURE_TSC_DEADLINE_TIMER;
                res.ecx |= FEATURE_HYPERVISOR;
                res.edx &= !FEATURE_MCE;
                res.edx |= FEATURE_APIC;
                res.ebx &= !(MAX_LOGICAL_PROCESSORS_MASK | INITIAL_APIC_ID_MASK);
                res.ebx |= 1 << 16;
                res.ebx |= ((self.vcpu_id as u32) & 0xff) << 24;
                res
            }
            0xb | 0x1f => CpuIdResult {
                eax: 0,
                ebx: 0,
                ecx: regs_clone.rcx as u32,
                edx: 0,
            },
            // See SDM Table 3-8. Information Returned by CPUID Instruction (Contd.)
            LEAF_STRUCTURED_EXTENDED_FEATURE_FLAGS_ENUMERATION => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                if regs_clone.rcx == 0 {
                    // Bit 05: WAITPKG.
                    res.ecx.set_bit(5, false); // clear waitpkg
                    // Bit 16: LA57. Supports 57-bit linear addresses and five-level paging if 1.
                    res.ecx.set_bit(16, false); // clear LA57
                }

                res
            }
            LEAF_PROCESSOR_EXTENDED_STATE_ENUMERATION => {
                self.xstate.switch_xcrs_to_guest();
                let res = cpuid!(regs_clone.rax, regs_clone.rcx);
                self.xstate.switch_xcrs_to_host();

                res
            }
            LEAF_HYPERVISOR_INFO => CpuIdResult {
                eax: LEAF_HYPERVISOR_FEATURE,
                ebx: vendor_regs[0],
                ecx: vendor_regs[1],
                edx: vendor_regs[2],
            },
            LEAF_HYPERVISOR_FEATURE => CpuIdResult {
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            },
            EAX_FREQUENCY_INFO => {
                /// Timer interrupt frequencyin Hz.
                /// Todo: this should be the same as `ax_config::TIMER_FREQUENCY` defined in ArceOS's config file.
                const TIMER_FREQUENCY_MHZ: u32 = 3_000;
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                if res.eax == 0 {
                    warn!(
                        "handle_cpuid: Failed to get TSC frequency by CPUID, default to \
                         {TIMER_FREQUENCY_MHZ} MHz"
                    );
                    res.eax = TIMER_FREQUENCY_MHZ;
                }
                res
            }
            _ => cpuid!(regs_clone.rax, regs_clone.rcx),
            }
        };
        self.fixup_kvm_cpuid_identity(function, regs_clone.rcx as u32, &mut res);

        trace!(
            "VM exit: CPUID({:#x}, {:#x}): {:?}",
            regs_clone.rax, regs_clone.rcx, res
        );

        let regs = self.regs_mut();
        regs.rax = res.eax as _;
        regs.rbx = res.ebx as _;
        regs.rcx = res.ecx as _;
        regs.rdx = res.edx as _;
        self.advance_rip(VM_EXIT_INSTR_LEN_CPUID)?;

        Ok(())
    }

    fn handle_xsetbv(&mut self) -> AxResult {
        const XCR_XCR0: u64 = 0;
        const VM_EXIT_INSTR_LEN_XSETBV: u8 = 3;

        let index = self.guest_regs.rcx.get_bits(0..32);
        let value = self.guest_regs.rdx.get_bits(0..32) << 32 | self.guest_regs.rax.get_bits(0..32);

        // TODO: get host-supported xcr0 mask by cpuid and reject any guest-xsetbv violating that
        if index == XCR_XCR0 {
            Xcr0::from_bits(value)
                .and_then(|x| {
                    if !x.contains(Xcr0::XCR0_FPU_MMX_STATE) {
                        return None;
                    }

                    if x.contains(Xcr0::XCR0_AVX_STATE) && !x.contains(Xcr0::XCR0_SSE_STATE) {
                        return None;
                    }

                    if x.contains(Xcr0::XCR0_BNDCSR_STATE) ^ x.contains(Xcr0::XCR0_BNDREG_STATE) {
                        return None;
                    }

                    let has_opmask = x.contains(Xcr0::XCR0_OPMASK_STATE);
                    let has_zmm_hi256 = x.contains(Xcr0::XCR0_ZMM_HI256_STATE);
                    let has_hi16_zmm = x.contains(Xcr0::XCR0_HI16_ZMM_STATE);
                    let has_any_avx512 = has_opmask || has_zmm_hi256 || has_hi16_zmm;
                    let has_all_avx512 = has_opmask && has_zmm_hi256 && has_hi16_zmm;
                    if has_any_avx512
                        && (!has_all_avx512
                            || !x.contains(Xcr0::XCR0_SSE_STATE)
                            || !x.contains(Xcr0::XCR0_AVX_STATE))
                    {
                        return None;
                    }

                    Some(x)
                })
                .ok_or(ax_err_type!(InvalidInput))
                .and_then(|x| {
                    self.xstate.guest_xcr0 = x.bits();
                    self.advance_rip(VM_EXIT_INSTR_LEN_XSETBV)
                })
        } else {
            // xcr0 only
            ax_err!(Unsupported, "only xcr0 is supported")
        }
    }

    fn load_guest_xstate(&mut self) {
        self.xstate.switch_to_guest();
    }

    fn load_host_xstate(&mut self) {
        self.xstate.switch_to_host();
    }
}

impl Drop for VmxVcpu {
    fn drop(&mut self) {
        info!("[HV] dropped VmxVcpu(vmcs: {:#x})", self.vmcs.phys_addr());
    }
}

fn get_tr_base(tr: SegmentSelector, gdt: &DescriptorTablePointer<u64>) -> u64 {
    let index = tr.index() as usize;
    let table_len = (gdt.limit as usize + 1) / core::mem::size_of::<u64>();
    let table = unsafe { core::slice::from_raw_parts(gdt.base, table_len) };
    let entry = table[index];
    if entry & (1 << 47) != 0 {
        // present
        let base_low = entry.get_bits(16..40) | entry.get_bits(56..64) << 24;
        let base_high = table[index + 1] & 0xffff_ffff;
        base_low | base_high << 32
    } else {
        // no present
        0
    }
}

impl Debug for VmxVcpu {
    fn fmt(&self, f: &mut Formatter) -> Result {
        (|| -> AxResult<Result> {
            Ok(f.debug_struct("VmxVcpu")
                .field("guest_regs", &self.guest_regs)
                .field("rip", &VmcsGuestNW::RIP.read()?)
                .field("rsp", &VmcsGuestNW::RSP.read()?)
                .field("rflags", &VmcsGuestNW::RFLAGS.read()?)
                .field("cr0", &VmcsGuestNW::CR0.read()?)
                .field("cr3", &VmcsGuestNW::CR3.read()?)
                .field("cr4", &VmcsGuestNW::CR4.read()?)
                .field("cs", &VmcsGuest16::CS_SELECTOR.read()?)
                .field("fs_base", &VmcsGuestNW::FS_BASE.read()?)
                .field("gs_base", &VmcsGuestNW::GS_BASE.read()?)
                .field("tss", &VmcsGuest16::TR_SELECTOR.read()?)
                .finish())
        })()
        .unwrap()
    }
}

impl AxArchVCpu for VmxVcpu {
    type CreateConfig = ();

    type SetupConfig = X86VCpuSetupConfig;

    fn new(vm_id: VMId, vcpu_id: VCpuId, _config: Self::CreateConfig) -> AxResult<Self> {
        Self::new(vm_id, vcpu_id)
    }

    fn set_entry(&mut self, entry: GuestPhysAddr) -> AxResult {
        self.entry = Some(entry);
        Ok(())
    }

    fn set_ept_root(&mut self, ept_root: HostPhysAddr) -> AxResult {
        self.ept_root = Some(ept_root);
        Ok(())
    }

    fn setup(&mut self, config: Self::SetupConfig) -> AxResult {
        self.setup_vmcs(self.entry.unwrap(), self.ept_root.unwrap(), config)
    }

    fn run(&mut self) -> AxResult<AxVCpuExitReason> {
        match self.inner_run() {
            Some(exit_info) => {
                // gvisor bring-up diagnostic: unconditionally log the first N raw
                // VM-exits (reason + guest RIP) so we can see exactly where the
                // sentry vCPU wedges after entering the guest.
                static GV_EXIT_PROBE: AtomicUsize = AtomicUsize::new(0);
                let gvp = GV_EXIT_PROBE.fetch_add(1, Ordering::Relaxed) + 1;
                if gvp <= 60 {
                    vmx_emerg_write(&format!(
                        "gv_exit n={} vcpu={} reason={:?} raw={:#x} rip={:#x} qual={:#x}\n",
                        gvp, self.vcpu_id, exit_info.exit_reason,
                        exit_info.exit_reason_raw, exit_info.guest_rip,
                        exit_info.exit_qualification,
                    ));
                }
                // Cache the guest RIP captured (while the VMCS was loaded) inside
                // `inner_run`, so host-side code that runs after this vCPU is
                // unbound can read it without a `vmread` against an unloaded VMCS.
                self.last_exit_rip = exit_info.guest_rip;
                Ok(if exit_info.entry_failure {
                warn!(
                    "VMX entry failure: exit_reason_raw={:#x} basic_reason={:?} \
                     exit_qualification={:#x} instruction_error={:#x} guest_rip={:#x}",
                    exit_info.exit_reason_raw,
                    exit_info.exit_reason,
                    exit_info.exit_qualification,
                    exit_info.instruction_error_raw,
                    exit_info.guest_rip,
                );
                AxVCpuExitReason::FailEntry {
                    hardware_entry_failure_reason: exit_info.exit_reason_raw as u64,
                }
            } else {
                match exit_info.exit_reason {
                    VmxExitReason::VMCALL => {
                        self.advance_rip(exit_info.exit_instruction_length as _)?;
                        AxVCpuExitReason::Hypercall {
                            nr: self.regs().rax,
                            args: [
                                self.regs().rdi,
                                self.regs().rsi,
                                self.regs().rdx,
                                self.regs().rcx,
                                self.regs().r8,
                                self.regs().r9,
                            ],
                        }
                    }
                    VmxExitReason::IO_INSTRUCTION => {
                        let io_info = self.io_exit_info().unwrap();
                        self.advance_rip(exit_info.exit_instruction_length as _)?;

                        let port = io_info.port;

                        if io_info.is_repeat || io_info.is_string {
                            warn!("VMX unsupported IO-Exit: {io_info:#x?} of {exit_info:#x?}");
                            warn!("VCpu {self:#x?}");
                            AxVCpuExitReason::Halt
                        } else {
                            let width = match AccessWidth::try_from(io_info.access_size as usize) {
                                Ok(width) => width,
                                Err(_) => {
                                    warn!("VMX invalid IO-Exit: {io_info:#x?} of {exit_info:#x?}");
                                    warn!("VCpu {self:#x?}");
                                    return Ok(AxVCpuExitReason::Halt);
                                }
                            };

                            log_vmx_io_exit(
                                port,
                                width,
                                io_info.is_in,
                                self.regs().rax.get_bits(width.bits_range()),
                                exit_info.guest_rip,
                            );
                            if io_info.is_in {
                                AxVCpuExitReason::IoRead {
                                    port: Port(port),
                                    width,
                                }
                            } else if port == QEMU_EXIT_PORT
                                && width == AccessWidth::Word
                                && self.regs().rax == QEMU_EXIT_MAGIC
                            {
                                AxVCpuExitReason::SystemDown
                            } else if port == QEMU_RESET_PORT {
                                warn!(
                                    "VMX guest wrote QEMU reset port {port:#x} with data {:#x}",
                                    self.regs().rax.get_bits(width.bits_range())
                                );
                                AxVCpuExitReason::SystemDown
                            } else {
                                AxVCpuExitReason::IoWrite {
                                    port: Port(port),
                                    width,
                                    data: self.regs().rax.get_bits(width.bits_range()),
                                }
                            }
                        }
                    }
                    VmxExitReason::EXTERNAL_INTERRUPT => {
                        let int_info = self.interrupt_exit_info().map_err(|err| {
                            vmx_log_run_err("interrupt_exit_info", &exit_info, self.regs(), &err);
                            err
                        })?;
                        assert!(int_info.valid);
                        AxVCpuExitReason::ExternalInterrupt {
                            vector: int_info.vector as _,
                        }
                    }
                    VmxExitReason::EXCEPTION_NMI => {
                        let int_info = self.interrupt_exit_info().map_err(|err| {
                            vmx_log_run_err("exception_nmi_info", &exit_info, self.regs(), &err);
                            err
                        })?;
                        let count = VMX_EXCEPTION_NMI_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        if count <= 32 || count.is_power_of_two() {
                            vmx_emerg_write(
                                format!(
                                    "vmx exception_nmi: valid={} vector={:#x} type={:?} err={:?} rip={:#x} count={}\n",
                                    int_info.valid,
                                    int_info.vector,
                                    int_info.int_type,
                                    int_info.err_code,
                                    exit_info.guest_rip,
                                    count
                                )
                                .as_str(),
                            );
                        }
                        if !int_info.valid {
                            vmx_log_terminal_exit("exception-nmi", &exit_info, self.regs());
                            AxVCpuExitReason::Halt
                        } else if int_info.int_type == VmxInterruptionType::NMI {
                            AxVCpuExitReason::PreemptionTimer
                        } else if int_info.int_type == VmxInterruptionType::HardException {
                            self.queue_event(int_info.vector, int_info.err_code);
                            AxVCpuExitReason::Nothing
                        } else {
                            vmx_log_terminal_exit("exception-nmi-unhandled-type", &exit_info, self.regs());
                            AxVCpuExitReason::Halt
                        }
                    }
                    VmxExitReason::PREEMPTION_TIMER => {
                        self.handle_vmx_preemption_timer().map_err(|err| {
                            vmx_log_run_err("preemption_timer", &exit_info, self.regs(), &err);
                            err
                        })?;
                        let count = VMX_PREEMPTION_TIMER_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        if count <= 64 || count.is_power_of_two() {
                            vmx_emerg_write(
                                format!(
                                    "x86_vmx::preemption_timer vm={} vcpu={} rip={:#x} cs={:#x} cs_base={:#x} rflags={:#x} cr0={:#x} cr3={:#x} cr4={:#x} pending={} count={}\n",
                                    self.vm_id,
                                    self.vcpu_id,
                                    exit_info.guest_rip,
                                    VmcsGuest16::CS_SELECTOR.read().unwrap_or(0),
                                    VmcsGuestNW::CS_BASE.read().unwrap_or(0),
                                    VmcsGuestNW::RFLAGS.read().unwrap_or(usize::MAX),
                                    VmcsGuestNW::CR0.read().unwrap_or(usize::MAX),
                                    VmcsGuestNW::CR3.read().unwrap_or(usize::MAX),
                                    VmcsGuestNW::CR4.read().unwrap_or(usize::MAX),
                                    self.pending_event_count(),
                                    count,
                                )
                                .as_str(),
                            );
                        }
                        AxVCpuExitReason::PreemptionTimer
                    }
                    VmxExitReason::HLT => {
                        let count = VMX_HLT_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        if count <= 64 || count.is_power_of_two() {
                            vmx_emerg_write(
                                format!(
                                    "x86_vmx::hlt vm={} vcpu={} rip={:#x} cs={:#x} cs_base={:#x} rflags={:#x} cr0={:#x} cr3={:#x} cr4={:#x} pending={} preempt_val={:#x} pin={:#x} count={}\n",
                                    self.vm_id,
                                    self.vcpu_id,
                                    exit_info.guest_rip,
                                    VmcsGuest16::CS_SELECTOR.read().unwrap_or(0),
                                    VmcsGuestNW::CS_BASE.read().unwrap_or(0),
                                    VmcsGuestNW::RFLAGS.read().unwrap_or(usize::MAX),
                                    VmcsGuestNW::CR0.read().unwrap_or(usize::MAX),
                                    VmcsGuestNW::CR3.read().unwrap_or(usize::MAX),
                                    VmcsGuestNW::CR4.read().unwrap_or(usize::MAX),
                                    self.pending_event_count(),
                                    VmcsGuest32::VMX_PREEMPTION_TIMER_VALUE.read().unwrap_or(u32::MAX),
                                    VmcsControl32::PINBASED_EXEC_CONTROLS.read().unwrap_or(u32::MAX),
                                    count,
                                )
                                .as_str(),
                            );
                        }
                        self.advance_rip(exit_info.exit_instruction_length as _)
                            .map_err(|err| {
                                vmx_log_run_err("hlt_advance_rip", &exit_info, self.regs(), &err);
                                err
                            })?;
                        // safe_halt is `sti; hlt`. The STI instruction sets the
                        // blocking-by-STI shadow in INTERRUPTIBILITY_STATE (bit 0)
                        // to protect the immediately-following `hlt` from being
                        // interrupted. But we just advanced RIP PAST the `hlt`, so
                        // that STI shadow is now stale. If left set, allow_interrupt()
                        // returns false on the next VM-entry and any pending injected
                        // interrupt (e.g. the hard PIT IRQ0 0x30 after the guest
                        // disabled its LAPIC timer) is routed to the event_blocked /
                        // interrupt-window path and never delivered — RIP stays pinned
                        // at safe_halt, no ISR runs, no EOI, guest jiffies freeze.
                        // Clear the STI (bit0) and MOV-SS (bit1) blocking bits so the
                        // resumed guest actually takes the interrupt.
                        if let Ok(int_state) = VmcsGuest32::INTERRUPTIBILITY_STATE.read() {
                            if int_state & 0b11 != 0 {
                                let _ = VmcsGuest32::INTERRUPTIBILITY_STATE.write(int_state & !0b11);
                            }
                        }
                        if self.has_pending_events() {
                            AxVCpuExitReason::Nothing
                        } else {
                        AxVCpuExitReason::Halt
                        }
                    }
                    VmxExitReason::VIRTUALIZED_EOI => {
                        let vector = (exit_info.exit_qualification & 0xff) as u8;
                        let broadcast_vector = self.vlapic.handle_eoi();
                        let count = VMX_VIRTUALIZED_EOI_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        if count <= 64 || count.is_power_of_two() {
                            vmx_emerg_write(
                                format!(
                                    "x86_vmx::virtualized_eoi vm={} vcpu={} vector={:#x} broadcast={:?} qual={:#x} rip={:#x} pending={} count={}\n",
                                    self.vm_id,
                                    self.vcpu_id,
                                    vector,
                                    broadcast_vector,
                                    exit_info.exit_qualification,
                                    exit_info.guest_rip,
                                    self.pending_event_count(),
                                    count,
                                )
                                .as_str(),
                            );
                        }
                        AxVCpuExitReason::InterruptEnd {
                            vector: broadcast_vector,
                        }
                    }
                    VmxExitReason::APIC_WRITE => {
                        let offset = self
                            .apic_access_exit_info()
                            .map_err(|err| {
                                vmx_log_run_err("apic_write_exit_info", &exit_info, self.regs(), &err);
                                err
                            })?
                            .offset as usize;
                        let eoi_vector = self.vlapic.handle_apic_write_exit(offset).map_err(|err| {
                            vmx_log_run_err("apic_write_handle", &exit_info, self.regs(), &err);
                            err
                        })?;
                        if let Some(cpu_up) = self.vlapic.take_pending_cpu_up() {
                            AxVCpuExitReason::CpuUp {
                                target_cpu: cpu_up.target_cpu,
                                entry_point: cpu_up.entry_point,
                                arg: 0,
                            }
                        } else if offset == X86_LOCAL_APIC_EOI_OFFSET {
                            AxVCpuExitReason::InterruptEnd {
                                vector: eoi_vector,
                            }
                        } else {
                            AxVCpuExitReason::Nothing
                        }
                    }
                    VmxExitReason::APIC_ACCESS => self.handle_apic_access(&exit_info).map_or_else(
                        |err| {
                            vmx_log_run_err("apic_access", &exit_info, self.regs(), &err);
                            AxVCpuExitReason::Halt
                        },
                        |exit_reason| exit_reason,
                    ),
                    VmxExitReason::MSR_READ => {
                        // `reg` is unused here.
                        AxVCpuExitReason::SysRegRead {
                            addr: SysRegAddr::new(self.regs().rcx as _),
                            reg: 0,
                        }
                    }
                    VmxExitReason::MSR_WRITE => {
                        let value = (self.regs().rax & 0xffff_ffff)
                            | ((self.regs().rdx & 0xffff_ffff) << 32);
                        if self.regs().rcx as u32 == X2APIC_EOI_MSR {
                            self.advance_rip(exit_info.exit_instruction_length as _)
                                .map_err(|err| {
                                    vmx_log_run_err("x2apic_eoi_advance_rip", &exit_info, self.regs(), &err);
                                    err
                                })?;
                            let eoi_vector = self.vlapic.handle_eoi();
                            let count = VMX_X2APIC_EOI_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                            if count <= 64 || count.is_power_of_two() {
                                vmx_emerg_write(
                                    format!(
                                        "x86_vmx::x2apic_eoi vm={} vcpu={} broadcast={:?} rip={:#x} pending={} count={}\n",
                                        self.vm_id,
                                        self.vcpu_id,
                                        eoi_vector,
                                        exit_info.guest_rip,
                                        self.pending_event_count(),
                                        count,
                                    )
                                    .as_str(),
                                );
                            }
                            AxVCpuExitReason::InterruptEnd {
                                vector: eoi_vector,
                            }
                        } else if self.regs().rcx as u32 == X2APIC_ICR_MSR {
                            self.advance_rip(exit_info.exit_instruction_length as _)
                                .map_err(|err| {
                                    vmx_log_run_err("x2apic_icr_advance_rip", &exit_info, self.regs(), &err);
                                    err
                                })?;
                            <EmulatedLocalApic as BaseDeviceOps<SysRegAddrRange>>::handle_write(
                                &self.vlapic,
                                SysRegAddr::new(self.regs().rcx as _),
                                AccessWidth::Qword,
                                value as usize,
                            )
                            .map_err(|err| {
                                vmx_log_run_err("x2apic_icr_write", &exit_info, self.regs(), &err);
                                err
                            })?;
                            if let Some(cpu_up) = self.vlapic.take_pending_cpu_up() {
                                AxVCpuExitReason::CpuUp {
                                    target_cpu: cpu_up.target_cpu,
                                    entry_point: cpu_up.entry_point,
                                    arg: 0,
                                }
                            } else {
                                AxVCpuExitReason::Nothing
                            }
                        } else {
                            AxVCpuExitReason::SysRegWrite {
                                addr: SysRegAddr::new(self.regs().rcx as _),
                                value,
                            }
                        }
                    }
                    VmxExitReason::EPT_VIOLATION => {
                        let info = self.nested_page_fault_info().map_err(|err| {
                            vmx_log_run_err("ept_violation_info", &exit_info, self.regs(), &err);
                            err
                        })?;
                        let write = info.access_flags.contains(MappingFlags::WRITE);
                        let read = info.access_flags.contains(MappingFlags::READ);
                        if (read || write)
                            && let Some(mmio_exit) = self.decode_ept_mmio_access(
                                &exit_info,
                                info.fault_guest_paddr,
                                write,
                            )
                        {
                            self.advance_rip(exit_info.exit_instruction_length as _)
                                .map_err(|err| {
                                    vmx_log_run_err("ept_mmio_advance_rip", &exit_info, self.regs(), &err);
                                    err
                                })?;
                            mmio_exit
                        } else {
                            AxVCpuExitReason::NestedPageFault {
                                addr: info.fault_guest_paddr,
                                access_flags: info.access_flags,
                            }
                        }
                    }
                    VmxExitReason::PAUSE_INSTRUCTION => {
                        // PLE fired: the guest spun in a PAUSE loop past the
                        // window, almost certainly waiting on another vCPU that
                        // the host preempted (lock-holder preemption / AP-bringup
                        // handshake). Skip the PAUSE and ask the run loop to
                        // directed-yield the physical CPU to a runnable-but-
                        // preempted sibling vCPU, then resume. Mirrors KVM's
                        // handle_pause -> kvm_vcpu_on_spin. Handled in-host only.
                        let count = VMX_PAUSE_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        if count <= 32 || count.is_power_of_two() {
                            vmx_emerg_write(
                                format!(
                                    "x86_vmx::pause vm={} vcpu={} rip={:#x} count={}\n",
                                    self.vm_id,
                                    self.vcpu_id,
                                    exit_info.guest_rip,
                                    count,
                                )
                                .as_str(),
                            );
                        }
                        self.advance_rip(exit_info.exit_instruction_length as _)
                            .map_err(|err| {
                                vmx_log_run_err("pause_advance_rip", &exit_info, self.regs(), &err);
                                err
                            })?;
                        AxVCpuExitReason::Yield
                    }
                    _ => {
                        vmx_log_terminal_exit("unsupported", &exit_info, self.regs());
                        warn!("VMX unsupported VM-Exit: {exit_info:#x?}");
                        warn!("VCpu {self:#x?}");
                        AxVCpuExitReason::Halt
                    }
                }
            })
            }
            None => Ok(AxVCpuExitReason::Nothing),
        }
    }

    fn bind(&mut self) -> AxResult {
        self.bind_to_current_processor()
    }

    fn unbind(&mut self) -> AxResult {
        self.launched = false;
        self.unbind_from_current_processor()
    }

    fn set_gpr(&mut self, reg: usize, val: usize) {
        self.regs_mut().set_reg_of_index(reg as u8, val as u64);
    }

    fn inject_interrupt(&mut self, vector: usize) -> AxResult {
        if vector != 0 {
            // warn!("interrupt queued in inject_interrupt: vector {:#x}", vector);
        } else {
            warn!("interrupt queued in inject_interrupt: vector 0");
            panic!()
        }
        self.queue_event(vector as u8, None);
        Ok(())
    }

    fn inject_interrupt_with_trigger(
        &mut self,
        vector: usize,
        trigger: InterruptTriggerMode,
    ) -> AxResult {
        if vector == 0 {
            warn!("interrupt queued in inject_interrupt_with_trigger: vector 0");
            panic!()
        }
        self.queue_event_with_trigger(
            vector as u8,
            None,
            trigger == InterruptTriggerMode::LevelTriggered,
        );
        Ok(())
    }

    fn set_return_value(&mut self, val: usize) {
        self.regs_mut().rax = val as u64;
    }

    fn complete_io_read(&mut self, val: usize, width: usize) {
        let mask = match width {
            1 => 0xff,
            2 => 0xffff,
            4 => 0xffff_ffff,
            8 => u64::MAX,
            _ => 0,
        };
        if mask != 0 {
            let regs = self.regs_mut();
            regs.rax = (regs.rax & !mask) | ((val as u64) & mask);
        }
    }
}

fn log_vmx_io_exit(port: u16, width: AccessWidth, is_in: bool, data: u64, rip: usize) {
    use core::sync::atomic::{AtomicUsize, Ordering};

    static VMX_IO_EXIT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

    let count = VMX_IO_EXIT_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= VMX_IO_EXIT_LOG_LIMIT || count.is_power_of_two() {
        info!(
            "VMX io_exit count={} rip={:#x} dir={} port={:#x} width={:?} data={:#x}",
            count,
            rip,
            if is_in { "in" } else { "out" },
            port,
            width,
            data
        );
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    #[test]
    fn test_vm_cpu_mode_enum() {
        // Test VmCpuMode enum values
        assert_ne!(VmCpuMode::Real, VmCpuMode::Protected);
        assert_ne!(VmCpuMode::Protected, VmCpuMode::Compatibility);
        assert_ne!(VmCpuMode::Compatibility, VmCpuMode::Mode64);

        // Test Debug formatting
        let debug_str = format!("{:?}", VmCpuMode::Mode64);
        assert!(debug_str.contains("Mode64"));
    }

    #[test]
    fn test_general_registers_operations() {
        let mut regs = GeneralRegisters::default();

        // Test initial state
        assert_eq!(regs.rax, 0);
        assert_eq!(regs.rbx, 0);

        // Test setting and getting values
        regs.rax = 0x1234567890abcdef;
        regs.rbx = 0xfedcba0987654321;

        assert_eq!(regs.rax, 0x1234567890abcdef);
        assert_eq!(regs.rbx, 0xfedcba0987654321);

        // Test register access by index
        regs.set_reg_of_index(0, 0x1111111111111111); // RAX
        assert_eq!(regs.get_reg_of_index(0), 0x1111111111111111);

        regs.set_reg_of_index(1, 0x2222222222222222); // RCX  
        assert_eq!(regs.get_reg_of_index(1), 0x2222222222222222);
    }

    #[test]
    fn test_constants() {
        // Test that constants have expected values
        assert_eq!(VMX_PREEMPTION_TIMER_SET_VALUE, 1_000_000);
        assert_eq!(QEMU_EXIT_PORT, 0x604);
        assert_eq!(QEMU_EXIT_MAGIC, 0x2000);
        assert_eq!(MSR_IA32_EFER_LMA_BIT, 1 << 10);
        assert_eq!(CR0_PE, 1 << 0);
    }

    #[test]
    fn test_bit_operations() {
        use bit_field::BitField;

        let mut value = 0u64;
        value.set_bits(0..32, 0x12345678);
        value.set_bits(32..64, 0xabcdef00);

        assert_eq!(value.get_bits(0..32), 0x12345678);
        assert_eq!(value.get_bits(32..64), 0xabcdef00);
    }

    // Mock tests for VmxVcpu (limited to safe operations)
    mod vmx_vcpu_tests {
        use super::*;

        // Helper function to create a test VmxVcpu (this would normally require VMX hardware)
        fn create_test_vcpu_regs() -> GeneralRegisters {
            let mut regs = GeneralRegisters::default();
            regs.rax = 0x1000;
            regs.rbx = 0x2000;
            regs.rcx = 0x3000;
            regs.rdx = 0x4000;
            regs
        }

        #[test]
        fn test_general_registers_clone() {
            let regs = create_test_vcpu_regs();
            let cloned_regs = regs.clone();

            assert_eq!(regs.rax, cloned_regs.rax);
            assert_eq!(regs.rbx, cloned_regs.rbx);
            assert_eq!(regs.rcx, cloned_regs.rcx);
            assert_eq!(regs.rdx, cloned_regs.rdx);
        }

        #[test]
        fn test_edx_eax_operations() {
            // Test the logic for combining EDX:EAX
            let rax = 0x12345678u64;
            let rdx = 0xabcdef00u64;

            // Simulate read_edx_eax logic
            let combined = ((rdx & 0xffff_ffff) << 32) | (rax & 0xffff_ffff);
            assert_eq!(combined, 0xabcdef0012345678);

            // Simulate write_edx_eax logic
            let val = 0xfedcba0987654321u64;
            let new_rax = val & 0xffff_ffff;
            let new_rdx = val >> 32;

            assert_eq!(new_rax, 0x87654321);
            assert_eq!(new_rdx, 0xfedcba09);
        }

        #[test]
        fn test_register_bit_operations() {
            let mut regs = GeneralRegisters::default();

            // Test setting specific bits in registers
            regs.rcx = 0;
            regs.rcx.set_bits(0..32, 0x12345678);
            assert_eq!(regs.rcx.get_bits(0..32), 0x12345678);

            regs.rdx = 0xffffffffffffffff;
            regs.rdx.set_bits(32..64, 0);
            assert_eq!(regs.rdx.get_bits(32..64), 0);
            assert_eq!(regs.rdx.get_bits(0..32), 0xffffffff);
        }

        #[test]
        fn test_gla2gva_logic() {
            // Test the address translation logic (without actual VMX hardware)
            let guest_rip = 0x1000usize;
            let seg_base_64bit = 0; // In 64-bit mode, segment base is 0
            let seg_base_other = 0x10000; // In other modes, segment base matters

            // 64-bit mode calculation
            let gva_64bit = guest_rip + seg_base_64bit;
            assert_eq!(gva_64bit, 0x1000);

            // Other mode calculation
            let gva_other = guest_rip + seg_base_other;
            assert_eq!(gva_other, 0x11000);
        }

        #[test]
        fn test_interrupt_vector_validation() {
            // Test interrupt vector validation logic
            let valid_exception = 6; // #UD exception
            let valid_interrupt = 0x20;
            let invalid_vector = 0;

            assert!(valid_exception < 32); // Exceptions are < 32
            assert!(valid_interrupt >= 32); // Interrupts are >= 32
            assert_eq!(invalid_vector, 0); // Vector 0 should be handled specially
        }

        #[test]
        fn test_page_walk_info_struct() {
            let ptw_info = GuestPageWalkInfo {
                top_entry: 0x1000,
                level: 4,
                width: 9,
                is_user_mode_access: false,
                is_write_access: false,
                is_inst_fetch: false,
                pse: true,
                wp: true,
                nxe: true,
                is_smap_on: false,
                is_smep_on: false,
            };

            assert_eq!(ptw_info.level, 4);
            assert_eq!(ptw_info.width, 9);
            assert_eq!(ptw_info.top_entry, 0x1000);
        }

        #[test]
        fn test_cpuid_constants() {
            // Test CPUID-related constants used in handle_cpuid
            const LEAF_FEATURE_INFO: u32 = 0x1;
            const LEAF_HYPERVISOR_INFO: u32 = 0x4000_0000;
            const FEATURE_VMX: u32 = 1 << 5;
            const FEATURE_HYPERVISOR: u32 = 1 << 31;

            assert_eq!(LEAF_FEATURE_INFO, 1);
            assert_eq!(LEAF_HYPERVISOR_INFO, 0x40000000);
            assert_eq!(FEATURE_VMX, 32);
            assert_eq!(FEATURE_HYPERVISOR, 0x80000000);
        }

        #[test]
        fn test_cr_flags_operations() {
            use x86_64::registers::control::{Cr0Flags, Cr4Flags};

            // Test CR0 flags
            let cr0_flags = Cr0Flags::PAGING | Cr0Flags::PROTECTED_MODE_ENABLE;
            assert!(cr0_flags.contains(Cr0Flags::PAGING));
            assert!(cr0_flags.contains(Cr0Flags::PROTECTED_MODE_ENABLE));
            assert!(!cr0_flags.contains(Cr0Flags::CACHE_DISABLE));

            // Test CR4 flags
            let cr4_flags = Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS | Cr4Flags::PAGE_SIZE_EXTENSION;
            assert!(cr4_flags.contains(Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS));
            assert!(cr4_flags.contains(Cr4Flags::PAGE_SIZE_EXTENSION));
        }

        #[test]
        fn test_access_width_operations() {
            // Test access width enumeration
            use axaddrspace::device::AccessWidth;

            assert_eq!(AccessWidth::Byte as usize, 0);
            assert_eq!(AccessWidth::Word as usize, 1);
            assert_eq!(AccessWidth::Dword as usize, 2);
            assert_eq!(AccessWidth::Qword as usize, 3);

            // Test conversion
            assert_eq!(AccessWidth::try_from(1), Ok(AccessWidth::Byte));
            assert_eq!(AccessWidth::try_from(2), Ok(AccessWidth::Word));
            assert_eq!(AccessWidth::try_from(4), Ok(AccessWidth::Dword));
            assert_eq!(AccessWidth::try_from(8), Ok(AccessWidth::Qword));
        }
    }

    // Tests for utility functions that don't require hardware
    #[test]
    fn test_get_tr_base_logic() {
        let mut test_entry = 0u64;
        test_entry |= 1u64 << 47; // Present bit
        test_entry |= (0x1000u64 & 0xFFFFFF) << 16; // Base address bits 16-39

        // Present bit check
        let present = test_entry & (1 << 47) != 0;
        assert!(present);

        // Base address extraction
        let base_low = (test_entry >> 16) & 0xFFFFFF;
        let base_high = (test_entry >> 56) & 0xFF;
        let base_addr = base_low | (base_high << 24);

        assert_eq!(base_addr, 0x1000);
    }

    #[test]
    fn test_vmx_exit_reason_enum() {
        // Test that VmxExitReason enum can be used in match statements
        let test_reason = VmxExitReason::VMCALL;
        match test_reason {
            VmxExitReason::VMCALL => assert!(true),
            _ => assert!(false),
        }
    }

    #[test]
    fn test_debug_implementations() {
        // Test Debug implementations for various types
        let cpu_mode = VmCpuMode::Mode64;
        let debug_str = format!("{:?}", cpu_mode);
        assert!(!debug_str.is_empty());

        let regs = GeneralRegisters::default();
        let debug_str = format!("{:?}", regs);
        assert!(!debug_str.is_empty());
    }

    // Note: Most VmxVcpu methods require actual VMX hardware support and cannot be unit tested
    // without either:
    // 1. Running on VMX-capable hardware with appropriate privileges
    // 2. Extensive mocking of the entire VMX infrastructure
    //
    // For comprehensive testing of VmxVcpu, integration tests on actual hardware
    // or hardware simulators would be more appropriate.
}
