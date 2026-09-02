// SPDX-License-Identifier: GPL-2.0

//! no_std x86 AxVisor backend bridge for the KVM ABI provider.
//!
//! This bridge is compiled as a plain object and linked into `axvisor_kvm.ko`.
//! It deliberately avoids `alloc` in the first revision so Kbuild does not pull
//! the whole Rust sysroot archive through `--whole-archive`.

#![no_std]
#![feature(alloc_error_handler)]
#![allow(missing_docs)]
#![allow(unused_extern_crates)]

extern crate alloc;
extern crate ax_kspin;
extern crate axaddrspace;
extern crate axdevice;
extern crate axdevice_base;
extern crate ax_errno;
extern crate ax_memory_addr;
extern crate axvisor_api;
extern crate axvcpu;
extern crate axvm;
extern crate axvmconfig;
extern crate x86_vcpu;
extern crate x86_vlapic;

use core::{
    alloc::{GlobalAlloc, Layout},
    ffi::c_void,
    ptr,
    ptr::null_mut,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use alloc::{boxed::Box, format, string::String, vec::Vec};

use ax_kspin::SpinNoIrq;
use ax_memory_addr::PAGE_SIZE_4K;
use axaddrspace::{
    GuestPhysAddr, HostPhysAddr, MappingFlags,
    device::{AccessWidth, Port},
};
use axvcpu::{AxVCpuExitReason, InterruptTriggerMode};
use axvm::{AxVM, AxVMPerCpu, AxVMRef};
use axvmconfig::{
    AxVMCrateConfig, EmulatedDeviceConfig, EmulatedDeviceType, VMInterruptMode, VMType,
};
use x86_vcpu::{
    GeneralRegisters, X86KvmCpuidEntry, X86KvmDtable, X86KvmFxsave, X86KvmMsrEntry,
    X86KvmSegment, X86KvmVcpuState, X86_KVM_MAX_CPUID_ENTRIES, X86_KVM_MAX_MSR_ENTRIES,
};
use axvisor_api::{
    api_impl, console, host, memory, sync, task, time,
    vmm::{self, VCpuSet},
};

const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;
const ENOSPC: i32 = 28;
const AXKVM_BACKEND_EXIT_MMIO_READ: u32 = 1;
const AXKVM_BACKEND_EXIT_MMIO_WRITE: u32 = 2;
const AXKVM_BACKEND_EXIT_IO_READ: u32 = 3;
const AXKVM_BACKEND_EXIT_IO_WRITE: u32 = 4;
const AXKVM_BACKEND_EXIT_HLT: u32 = 5;
const AXKVM_BACKEND_EXIT_SHUTDOWN: u32 = 6;
const AXKVM_BACKEND_EXIT_FAIL_ENTRY: u32 = 7;
const AXKVM_BACKEND_EXIT_INTERNAL_ERROR: u32 = 8;
const AXKVM_BACKEND_EXIT_CPU_UP: u32 = 9;
const X86_PIT_TIMER_GSI: usize = 0;
const X86_PIT_TIMER_IRQ: u8 = 0;
const X86_COM1_GSI: usize = 4;
const X86_COM1_IRQ: u8 = 4;

const MAX_VMS: usize = 16;
const MAX_VCPUS: usize = 64;
const MAX_HOST_CPUS: usize = 256;
const MAX_RUN_CONTEXTS: usize = MAX_VCPUS * 2;
const MAX_CPUID_ENTRIES: usize = 256;
const MAX_MSR_ENTRIES: usize = 256;
const MAX_PAGE_MAPPINGS: usize = 262_144;
const MAX_TIMERS_PER_VCPU: usize = 64;
const KVM_RUN_INTERNAL_EXIT_LOG_INTERVAL: usize = 4096;

// Software PLE (Pause-Loop-Exiting) substitute. Hardware PLE is unavailable in
// this nested setup: the L0 hypervisor does not expose PAUSE_LOOP_EXITING in
// IA32_VMX_PROCBASED_CTLS2 allowed-1, so setup_vmcs cannot enable it and no
// PAUSE VM-exit is ever produced. Instead we approximate it in software: when a
// vCPU keeps spinning inside the same KVM_RUN (its internal-progress retry
// counter crosses this threshold, e.g. the BSP polling an AP for online, or a
// guest spinlock), we treat it as a lock-holder-preemption hint and issue a
// *directed* yield to another RUNNABLE-but-not-progressing vCPU, mirroring
// KVM's kvm_vcpu_on_spin. Chosen well below the log interval so it fires early
// and repeatedly while spinning, without being so small that every transient
// internal exit triggers it. At ~21ms/retry a spinning AP must directed-yield
// to the boot controller many times inside the guest's ~10s
// cpuhp_bp_sync_alive() release window, so keep this small (16 => yield roughly
// every ~0.3s of spinning) -- otherwise the AP misses the window and bringup
// wedges permanently under oversubscription.
#[allow(dead_code)]
const SOFT_PLE_DIRECTED_YIELD_INTERVAL: usize = 16;

// Software-PLE spin/idle park gating. Hardware PLE is unavailable in this
// nested setup, so under oversubscription (more guest vCPUs than host cores) a
// vCPU that never breaks back to C keeps its host core forever via yield()
// (which is a no-op when the thread is alone on its runqueue, rq->nr_running==1,
// kernel/sched/fair.c yield_task_fair) -- starving the sibling vCPU threads that
// still need a core (e.g. the last 2 APs finishing cpuhp_ap_sync_alive during
// SMP bringup, or a halted vCPU whose tick must advance). Real KVM never hits
// this because each vCPU is an ordinary preemptible CFS task and the host tick
// rotates all runnable threads across the cores; our yield()-spin defeats that.
//
// Two shapes of core-pinning vCPU exist under oversubscription:
//   * a spinner stuck at ONE fixed RIP region (interrupt-disabled cpu_relax
//     loop: csd_lock_wait, cpuhp_ap_sync_alive), and
//   * an idle vCPU oscillating between TWO windows (hlt <-> native_apic_mem_eoi)
//     taking a timer IRQ, EOIing, and re-halting forever.
// soft_ple_maybe_park catches BOTH by counting a streak of consecutive internal
// exits confined to a stable set of at most TWO 64-byte RIP windows. A third
// distinct window is genuine forward progress and resets the streak -- which is
// what keeps single-threaded early BSP boot (advancing through code) and the
// 1/2/4/8/16 baseline (never oversubscribed, returns early) completely
// unperturbed. On maturity it directed-yields toward a runnable sibling / the
// boot controller and, when yield_to() cannot hand off the core (the common
// case under CFS spread, returning 0/-ESRCH), parks this vCPU for one tick via
// schedule_timeout(1) -- the only reliable way to actually free the core and
// let CFS run a starved sibling.
//
// Threshold: low enough that an idle/spinning vCPU is demoted to SCHED_IDLE
// while it is still being scheduled. Under heavy oversubscription (20 vCPU @ 18
// cores) CFS abandons most starved AP threads after only a handful of exits:
// measured run 62Iogm, 17 of 19 APs got ~9 preemption-timer exits at
// cpuhp_ap_sync_alive (rip=0x812b02f4) and were then never rescheduled for 220s
// while 2 APs monopolized cores. A threshold of 128 in-window exits could never
// mature for the starved majority, so the SCHED_IDLE hand-off never fired. A
// low threshold demotes a confirmed spinner within its first few exits, so the
// instant it spins it sinks below every runnable NORMAL sibling and CFS rotates
// the core to a starved AP or the BSP. A single same-region blip in a healthy
// vCPU cannot reach it because early boot advances through distinct RIP windows
// (third-window branch resets the streak). Non-oversubscribed VMs never reach
// here (the online-count gate returns early).
const SOFT_PLE_RIP_STREAK_THRESHOLD: usize = 3;
// Once a vCPU is confirmed spinning/idle, re-demote this often (in in-window
// exits) rather than waiting a full threshold window again.
const SOFT_PLE_REPARK_INTERVAL: usize = 2;
const KVM_IOAPIC_NUM_PINS: usize = 24;
const NUM_SEGMENTS: usize = 8;
const NUM_DTABLES: usize = 2;
const FXSAVE_SIZE: usize = 512;
const FXSAVE_FCW_OFFSET: usize = 0;
const FXSAVE_FSW_OFFSET: usize = 2;
const FXSAVE_FTW_OFFSET: usize = 4;
const FXSAVE_FOP_OFFSET: usize = 6;
const FXSAVE_RIP_OFFSET: usize = 8;
const FXSAVE_RDP_OFFSET: usize = 16;
const FXSAVE_MXCSR_OFFSET: usize = 24;
const FXSAVE_MXCSR_MASK_OFFSET: usize = 28;
const FXSAVE_ST_OFFSET: usize = 32;
const FXSAVE_XMM_OFFSET: usize = 160;
const FXSAVE_ST_SIZE: usize = 128;
const FXSAVE_XMM_SIZE: usize = 256;
const X86_MXCSR_VALID_MASK: u32 = 0x0000_ffbf;

struct BridgeAllocator;
struct HostIfImpl;
struct ConsoleIfImpl;
struct TimeIfImpl;
struct SyncIfImpl;
struct TaskIfImpl;
struct MemoryIfImpl;
struct VmmIfImpl;

unsafe extern "C" {
    fn axvisor_kvm_x86_bridge_alloc(size: usize, align: usize) -> *mut c_void;
    fn axvisor_kvm_x86_bridge_realloc(
        ptr: *mut c_void,
        new_size: usize,
        align: usize,
    ) -> *mut c_void;
    fn axvisor_kvm_x86_bridge_dealloc(ptr: *mut c_void, align: usize);
    fn axvisor_kvm_x86_bridge_log(bytes: *const u8, len: usize);
    fn axvisor_kvm_x86_bridge_get_cpu_num() -> usize;
    fn axvisor_kvm_x86_bridge_current_cpu_id() -> usize;
    fn axvisor_kvm_x86_bridge_current_task_id() -> usize;
    fn axvisor_kvm_x86_bridge_migrate_disable();
    fn axvisor_kvm_x86_bridge_migrate_enable();
    fn axvisor_kvm_x86_bridge_current_time_nanos() -> u64;
    fn axvisor_kvm_x86_bridge_alloc_frame() -> u64;
    fn axvisor_kvm_x86_bridge_dealloc_frame(paddr: u64);
    fn axvisor_kvm_x86_bridge_phys_to_virt(paddr: u64) -> u64;
    fn axvisor_kvm_x86_bridge_virt_to_phys(vaddr: u64) -> u64;
    fn axvisor_kvm_x86_bridge_yield_now();
    fn axvisor_kvm_x86_bridge_park_now();
    fn axvisor_kvm_x86_bridge_schedule_now();
    fn axvisor_kvm_x86_bridge_cond_resched() -> i32;
    fn axvisor_kvm_x86_bridge_guest_fpu_begin() -> i32;
    fn axvisor_kvm_x86_bridge_guest_fpu_end();
    fn axvisor_kvm_x86_bridge_wake_vcpu(backend_vm: u64, vcpu_id: u32);
    fn axvisor_kvm_x86_bridge_boost_vcpu(backend_vm: u64, vcpu_id: u32);
    fn axvisor_kvm_x86_bridge_directed_yield(backend_vm: u64, cur_vcpu_id: u32) -> bool;
    fn axvisor_kvm_x86_bridge_spin_demote(backend_vm: u64, vcpu_id: u32);
    fn axvisor_kvm_x86_bridge_spin_restore(backend_vm: u64, vcpu_id: u32);
    fn axvisor_kvm_x86_bridge_spin_park(backend_vm: u64, vcpu_id: u32) -> i32;
    fn axvisor_kvm_x86_bridge_fault_in_gpa(backend_vm: u64, gpa: u64, write: u32) -> i32;
    /// DIAG (gvisor signal-interruptibility): 1 if the calling thread has a
    /// pending signal, else 0. Bounded probe only; remove after verification.
    fn axvisor_kvm_x86_bridge_signal_pending() -> i32;
    fn axvisor_kvm_x86_bridge_pvclock_write(backend_vm: u64, vcpu_id: u32, msr: u32, value: u64);
    fn axvisor_kvm_x86_bridge_pvclock_refresh(backend_vm: u64, vcpu_id: u32);
    fn axvisor_kvm_x86_bridge_note_ap_alive_spin(backend_vm: u64, vcpu_id: u32, rip: u64);
    fn axvisor_kvm_x86_bridge_program_timer(deadline_ns: u64);
    fn axvisor_kvm_x86_bridge_reprogram_timer(deadline_ns: u64);
    fn axvisor_kvm_x86_bridge_cancel_timer();
}

#[global_allocator]
static GLOBAL_ALLOCATOR: BridgeAllocator = BridgeAllocator;

unsafe impl GlobalAlloc for BridgeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            return layout.align() as *mut u8;
        }
        unsafe { axvisor_kvm_x86_bridge_alloc(layout.size(), layout.align()).cast::<u8>() }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        unsafe { axvisor_kvm_x86_bridge_dealloc(ptr.cast::<c_void>(), layout.align()) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.size() == 0 {
            if new_size == 0 {
                return layout.align() as *mut u8;
            }
            return unsafe {
                axvisor_kvm_x86_bridge_alloc(new_size, layout.align()).cast::<u8>()
            };
        }
        if new_size == 0 {
            unsafe { axvisor_kvm_x86_bridge_dealloc(ptr.cast::<c_void>(), layout.align()) };
            return null_mut();
        }
        unsafe {
            axvisor_kvm_x86_bridge_realloc(ptr.cast::<c_void>(), new_size, layout.align())
                .cast::<u8>()
        }
    }
}

fn bridge_log(msg: &str) {
    unsafe { axvisor_kvm_x86_bridge_log(msg.as_ptr(), msg.len()) };
}

fn inject_x86_ioapic_irq(axvm: &AxVMRef, target_vcpu_id: usize, vector: u8, level_triggered: bool) {
    let Some(vcpu) = axvm.vcpu(target_vcpu_id) else {
        return;
    };
    let trigger = if level_triggered {
        InterruptTriggerMode::LevelTriggered
    } else {
        InterruptTriggerMode::EdgeTriggered
    };
    if let Err(err) = vcpu.inject_interrupt_with_trigger(vector as usize, trigger) {
        bridge_log(&format!(
            "axvisor_kvm_x86_bridge inject_irq failed vcpu={} vector={:#x} err={err:?}",
            target_vcpu_id, vector
        ));
        return;
    }
    // Wake the target so a HLT-blocked vCPU resumes and takes the injected IRQ.
    // The HLT wait now blocks indefinitely (KVM kvm_vcpu_block model), so any
    // cross-vCPU device IRQ must wake its target or the vCPU sleeps forever.
    unsafe {
        axvisor_kvm_x86_bridge_wake_vcpu(
            <VmmIfImpl as vmm::VmmIf>::current_vm_id() as u64,
            target_vcpu_id as u32,
        );
    }
}

fn inject_x86_pic_irq(axvm: &AxVMRef, vcpu_id: usize, irq: u8) -> bool {
    let Some(vector) = axvm.get_devices().x86_pic_assert_irq(irq) else {
        return false;
    };
    let Some(vcpu) = axvm.vcpu(vcpu_id) else {
        return false;
    };
    if let Err(err) = vcpu.inject_interrupt_with_trigger(
        vector as usize,
        InterruptTriggerMode::EdgeTriggered,
    ) {
        bridge_log(&format!(
            "axvisor_kvm_x86_bridge inject_pic_irq failed vcpu={} irq={} vector={:#x} err={err:?}",
            vcpu_id, irq, vector
        ));
        return false;
    }
    // Wake the target (HLT now blocks indefinitely; see inject_x86_ioapic_irq).
    unsafe {
        axvisor_kvm_x86_bridge_wake_vcpu(<VmmIfImpl as vmm::VmmIf>::current_vm_id() as u64, vcpu_id as u32);
    }
    true
}

fn inject_due_x86_pit_irq0(axvm: &AxVMRef, vcpu_id: usize) {
    let now_ns = time::current_time_nanos();
    if !axvm.get_devices().x86_pit_consume_irq0_if_due(now_ns) {
        return;
    }

    if inject_x86_pic_irq(axvm, vcpu_id, X86_PIT_TIMER_IRQ) {
        return;
    }

    let Some(irq) = axvm.get_devices().x86_ioapic_assert_gsi(X86_PIT_TIMER_GSI) else {
        return;
    };
    let target_vcpu_id = irq.target_vcpu_id.unwrap_or(0);
    inject_x86_ioapic_irq(axvm, target_vcpu_id, irq.vector, irq.level_triggered);
}

fn complete_x86_external_eoi(axvm: &AxVMRef, vcpu_id: usize, vector: Option<u8>) {
    let Some(vector) = vector else {
        return;
    };
    axvm.get_devices().x86_pic_end_of_interrupt(vector);
    let Some(eoi) = axvm.get_devices().x86_ioapic_end_of_interrupt(vector) else {
        return;
    };
    let Some(irq) = eoi.pending else {
        return;
    };
    let target_vcpu_id = irq.target_vcpu_id.unwrap_or(vcpu_id);
    inject_x86_ioapic_irq(axvm, target_vcpu_id, irq.vector, irq.level_triggered);
}

fn inject_pending_x86_serial_irq(axvm: &AxVMRef, vcpu_id: usize) {
    if !axvm.get_devices().x86_serial_poll_irq() {
        return;
    }

    if inject_x86_pic_irq(axvm, vcpu_id, X86_COM1_IRQ) {
        return;
    }

    let Some(irq) = axvm.get_devices().x86_ioapic_assert_gsi(X86_COM1_GSI) else {
        return;
    };
    let target_vcpu_id = irq.target_vcpu_id.unwrap_or(0);
    inject_x86_ioapic_irq(axvm, target_vcpu_id, irq.vector, irq.level_triggered);
}

fn progress_x86_virtual_irqs(axvm: &AxVMRef, vcpu_id: usize) {
    if vcpu_id != 0 {
        return;
    }
    inject_due_x86_pit_irq0(axvm, vcpu_id);
    inject_pending_x86_serial_irq(axvm, vcpu_id);
}

/// Arm the host one-shot hrtimer at the guest PIT's next IRQ0 deadline so an
/// idle (HLT) guest is woken periodically even though the VMX preemption timer
/// is unavailable under nested virtualization.
///
/// `program_timer` only re-arms when the requested deadline is earlier than the
/// currently pending one (or none is pending), so this never clobbers an earlier
/// LAPIC deadline already registered through `reprogram_next_kvm_timer`.
fn arm_x86_idle_wakeup_timer(axvm: &AxVMRef) {
    let Some(deadline_ns) = axvm.get_devices().x86_pit_next_irq0_deadline_ns() else {
        return;
    };
    if deadline_ns == 0 {
        return;
    }

    unsafe { axvisor_kvm_x86_bridge_program_timer(deadline_ns) };
}

fn reprogram_next_kvm_timer() {
    let mut selected_deadline = u64::MAX;

    // Scan every per-vCPU table for the globally earliest in-use deadline.
    // Each table is locked independently and released before moving on, so we
    // never hold two locks at once and cannot deadlock.
    let mut v = 0;
    while v < MAX_VCPUS {
        let tbl = KVM_TIMERS[v].lock();
        let mut i = 0;
        while i < MAX_TIMERS_PER_VCPU {
            if tbl.entries[i].in_use {
                let deadline_ns = tbl.entries[i].deadline.as_nanos();
                let deadline_ns = if deadline_ns > u64::MAX as u128 {
                    u64::MAX
                } else {
                    deadline_ns as u64
                };
                selected_deadline = selected_deadline.min(deadline_ns);
            }
            i += 1;
        }
        drop(tbl);
        v += 1;
    }

    unsafe {
        if selected_deadline == u64::MAX {
            axvisor_kvm_x86_bridge_cancel_timer();
        } else {
            // Only-earlier re-arm (see the C-side comment on reprogram_timer):
            // the one-shot per-deadline hrtimer is just a low-latency wake for
            // HLT/PIT/earlier deadlines. Global drain-all liveness is provided
            // by the independent periodic hrtimer, so we do not need exact or
            // overdue-clamped re-arm here.
            axvisor_kvm_x86_bridge_reprogram_timer(selected_deadline);
        }
    }
}

fn expire_due_kvm_timers() {
    // Freeze the due-cutoff at pass entry. A periodic vLAPIC callback registers
    // its next-period timer from inside this drain; that new deadline is always
    // strictly in the future relative to when the callback ran. If we re-read
    // `now` every iteration, the fresh timer becomes "due" again the moment
    // wall-clock creeps past its (near-future) deadline, so a single expiry
    // re-consumes its own re-armed successor in the same pass. Under 8-vCPU
    // nested delivery the per-timer service cost exceeds the clamped period, so
    // this turns into an unbounded CPU-bound storm (~385k re-arms/s observed,
    // table saturates regardless of size). Bounding the pass to timers that were
    // already due at entry lets the next host hrtimer fire drive the next tick,
    // restoring the intended period throttle.
    let pass_cutoff = time::current_time();
    // Only touch this vCPU's own table: expire runs on the vCPU's KVM_RUN
    // thread, so `current_vcpu_id()` identifies the table that this thread may
    // safely drain. Callbacks are removed under the lock and invoked *after*
    // the lock is released, so a periodic LAPIC callback that re-registers its
    // next deadline (taking the same per-vCPU lock) does not re-enter and
    // deadlock, and no callback `Box` can be aliased/double-freed by another
    // thread.
    let vcpu_id = current_run_context()
        .map(|(_, vcpu_id)| vcpu_id)
        .unwrap_or_else(|| FALLBACK_CURRENT_VCPU_ID.load(Ordering::Acquire));
    if vcpu_id >= MAX_VCPUS {
        return;
    }
    loop {
        let taken = {
            let mut tbl = KVM_TIMERS[vcpu_id].lock();
            let mut selected = MAX_TIMERS_PER_VCPU;
            let mut i = 0;
            while i < MAX_TIMERS_PER_VCPU {
                if tbl.entries[i].in_use && tbl.entries[i].deadline <= pass_cutoff {
                    selected = i;
                    break;
                }
                i += 1;
            }

            if selected == MAX_TIMERS_PER_VCPU {
                None
            } else {
                let callback = tbl.entries[selected].callback.take();
                tbl.entries[selected] = KvmTimerEntry::empty();
                Some(callback)
            }
        };

        match taken {
            None => break,
            Some(callback) => {
                if let Some(callback) = callback {
                    callback(pass_cutoff);
                }
            }
        }
    }
    reprogram_next_kvm_timer();
}

/// Drain due timers across *every* per-vCPU table, not just the caller's own.
///
/// `expire_due_kvm_timers` only touches `current_vcpu_id()`'s table and runs on
/// a vCPU's KVM_RUN thread, so a vCPU that is starved off-core under CPU
/// oversubscription (RUNNABLE but never scheduled, and not HLT-halted so the
/// host hrtimer's `wake_halted_vcpus` can't reach it) never drains, re-arms, or
/// injects its own periodic LAPIC tick. Its guest CPU's timekeeping then freezes
/// and any kthread pinned there (e.g. rcu_preempt) starves -> RCU stall -> hang.
///
/// This variant is called from the host hrtimer workqueue (process context, may
/// sleep/lock/allocate) so timer delivery no longer depends on the target vCPU
/// being on-core. Each timer callback carries its own captured `vm_id/vcpu_id`
/// and injects via `vmm::inject_interrupt` (a lock-protected software queue, not
/// a VMCS write), so it is safe to run from an unrelated thread. Because we scan
/// all tables, a periodic timer that re-arms into the "wrong" table (the workfn
/// thread's fallback current vCPU) is still found and injected with the correct
/// captured id — that is what lets us avoid touching the API trait / vlapic.
///
/// Locking mirrors `expire_due_kvm_timers`: a single frozen `pass_cutoff`, one
/// table lock held at a time, `callback.take()` + entry cleared under the lock,
/// callback invoked *after* the lock is released (it re-arms via the same
/// per-table lock), and `reprogram_next_kvm_timer()` called once at the end
/// while holding no table lock.
fn expire_all_due_timers() {
    // Freeze the due-cutoff once for the whole sweep. A periodic callback
    // re-registers its next-period timer (always strictly in the future); a
    // fixed cutoff prevents this pass from re-consuming its own re-armed
    // successor and turning into an unbounded re-arm storm (see the note on
    // `expire_due_kvm_timers`).
    let pass_cutoff = time::current_time();
    let mut found_this_pass: usize = 0;
    let mut v = 0;
    while v < MAX_VCPUS {
        loop {
            let taken = {
                let mut tbl = KVM_TIMERS[v].lock();
                let mut selected = MAX_TIMERS_PER_VCPU;
                let mut i = 0;
                while i < MAX_TIMERS_PER_VCPU {
                    if tbl.entries[i].in_use && tbl.entries[i].deadline <= pass_cutoff {
                        selected = i;
                        break;
                    }
                    i += 1;
                }

                if selected == MAX_TIMERS_PER_VCPU {
                    None
                } else {
                    let callback = tbl.entries[selected].callback.take();
                    tbl.entries[selected] = KvmTimerEntry::empty();
                    Some(callback)
                }
            };

            match taken {
                None => break,
                Some(callback) => {
                    if let Some(callback) = callback {
                        found_this_pass += 1;
                        callback(pass_cutoff);
                    }
                }
            }
        }
        v += 1;
    }
    let _ = found_this_pass;
    reprogram_next_kvm_timer();
}

fn cancel_kvm_timers_for_vm(vm_id: vmm::VMId) {
    // Called during serialized VM destruction. Sweep every per-vCPU table.
    let mut v = 0;
    while v < MAX_VCPUS {
        let mut tbl = KVM_TIMERS[v].lock();
        let mut i = 0;
        while i < MAX_TIMERS_PER_VCPU {
            if tbl.entries[i].in_use && tbl.entries[i].vm_id == vm_id {
                tbl.entries[i] = KvmTimerEntry::empty();
            }
            i += 1;
        }
        drop(tbl);
        v += 1;
    }
    reprogram_next_kvm_timer();
}

fn register_kvm_timer_on_table(
    vm_id: vmm::VMId,
    vcpu_id: vmm::VCpuId,
    deadline: time::TimeValue,
    callback: alloc::boxed::Box<dyn FnOnce(time::TimeValue) + Send + 'static>,
) -> vmm::CancelToken {
    let token = NEXT_TIMER_TOKEN.fetch_add(1, Ordering::Relaxed).max(1);

    if vcpu_id >= MAX_VCPUS {
        return token;
    }

    // Try the owning vCPU's table first, then linear-probe every other table for
    // a free slot. LAPIC periodic re-arms can run on the host hrtimer workqueue,
    // outside the target vCPU's run context; table ownership must therefore come
    // from the captured target vCPU, not from current_vcpu_id().
    let mut probe = 0;
    let mut carried = Some(callback);
    while probe < MAX_VCPUS {
        let table = (vcpu_id + probe) % MAX_VCPUS;
        let mut tbl = KVM_TIMERS[table].lock();
        let mut i = 0;
        while i < MAX_TIMERS_PER_VCPU {
            if !tbl.entries[i].in_use {
                tbl.entries[i] = KvmTimerEntry {
                    in_use: true,
                    token,
                    vm_id,
                    deadline,
                    callback: carried.take(),
                };
                drop(tbl);
                reprogram_next_kvm_timer();
                return token;
            }
            i += 1;
        }
        drop(tbl);
        probe += 1;
    }

    token
}

struct BackendVm {
    in_use: bool,
    booted: bool,
    vm: Option<AxVMRef>,
    vcpu_count: u32,
    page_mapping_count: usize,
    version: u32,
    arch: u32,
    irqchip_created: bool,
    pit_created: bool,
    pit_flags: u32,
    tss_addr: u32,
    identity_map_addr: u64,
    nr_irqchips: u32,
    ioapic_redirtbl: [u64; KVM_IOAPIC_NUM_PINS],
    ioapic_redirtbl_count: u32,
}

impl BackendVm {
    const fn empty() -> Self {
        Self {
            in_use: false,
            booted: false,
            vm: None,
            vcpu_count: 0,
            page_mapping_count: 0,
            version: 0,
            arch: 0,
            irqchip_created: false,
            pit_created: false,
            pit_flags: 0,
            tss_addr: 0,
            identity_map_addr: 0,
            nr_irqchips: 0,
            ioapic_redirtbl: [0; KVM_IOAPIC_NUM_PINS],
            ioapic_redirtbl_count: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct PageMapping {
    in_use: bool,
    vm_handle: u64,
    gpa: u64,
    hpa: u64,
    flags: u32,
}

impl PageMapping {
    const fn empty() -> Self {
        Self {
            in_use: false,
            vm_handle: 0,
            gpa: 0,
            hpa: 0,
            flags: 0,
        }
    }
}

struct KvmTimerEntry {
    in_use: bool,
    token: vmm::CancelToken,
    vm_id: vmm::VMId,
    deadline: time::TimeValue,
    callback: Option<Box<dyn FnOnce(time::TimeValue) + Send + 'static>>,
}

impl KvmTimerEntry {
    const fn empty() -> Self {
        Self {
            in_use: false,
            token: 0,
            vm_id: 0,
            deadline: time::TimeValue::from_nanos(0),
            callback: None,
        }
    }
}

/// Per-vCPU timer table. Each vCPU owns one of these behind its own
/// `SpinNoIrq`, so the register/expire/cancel paths that run on that vCPU's
/// KVM_RUN thread never race against another vCPU's thread over a shared
/// table. This replaces the former single global `static mut KVM_TIMERS`
/// array whose unlocked concurrent `select -> take -> empty` sequence
/// double-freed callback `Box`es under 32-vCPU oversubscription.
struct VcpuTimerTable {
    entries: [KvmTimerEntry; MAX_TIMERS_PER_VCPU],
}

impl VcpuTimerTable {
    const fn empty() -> Self {
        Self {
            entries: [const { KvmTimerEntry::empty() }; MAX_TIMERS_PER_VCPU],
        }
    }
}

struct RunContext {
    task_id: AtomicUsize,
    vm_id: AtomicUsize,
    vcpu_id: AtomicUsize,
}

impl RunContext {
    const fn empty() -> Self {
        Self {
            task_id: AtomicUsize::new(0),
            vm_id: AtomicUsize::new(0),
            vcpu_id: AtomicUsize::new(0),
        }
    }
}

struct BackendVcpu {
    in_use: bool,
    vm_handle: u64,
    id: u32,
    rip: u64,
    rsp: u64,
    rflags: u64,
    cr0: u64,
    cr3: u64,
    cr4: u64,
    efer: u64,
    apic_base: u64,
    xcr0: u64,
    cpuid_nent: u32,
    nmsrs: u32,
    tsc_khz: u32,
    regs: BackendRegs,
    sregs: BackendSregs,
    segments: [BackendSegment; NUM_SEGMENTS],
    dtables: [BackendDtable; NUM_DTABLES],
    cpuid_entries: [CpuidEntry; MAX_CPUID_ENTRIES],
    msr_entries: [MsrEntry; MAX_MSR_ENTRIES],
    fxsave_valid: bool,
    fxsave: X86KvmFxsave,
    pending_mmio_read: PendingMmioRead,
    pending_io_read: PendingIoRead,
    state_dirty: bool,
    sipi_started: bool,
}

impl BackendVcpu {
    const fn empty() -> Self {
        Self {
            in_use: false,
            vm_handle: 0,
            id: 0,
            rip: 0,
            rsp: 0,
            rflags: 0,
            cr0: 0,
            cr3: 0,
            cr4: 0,
            efer: 0,
            apic_base: 0,
            xcr0: 1,
            cpuid_nent: 0,
            nmsrs: 0,
            tsc_khz: 0,
            regs: BackendRegs::empty(),
            sregs: BackendSregs::empty(),
            segments: [const { BackendSegment::empty() }; NUM_SEGMENTS],
            dtables: [const { BackendDtable::empty() }; NUM_DTABLES],
            cpuid_entries: [const { CpuidEntry::empty() }; MAX_CPUID_ENTRIES],
            msr_entries: [const { MsrEntry::empty() }; MAX_MSR_ENTRIES],
            fxsave_valid: false,
            fxsave: X86KvmFxsave::zeroed(),
            pending_mmio_read: PendingMmioRead::empty(),
            pending_io_read: PendingIoRead::empty(),
            state_dirty: false,
            sipi_started: false,
        }
    }
}

#[derive(Clone, Copy)]
struct PendingMmioRead {
    active: bool,
    reg: usize,
    width: AccessWidth,
    reg_width: AccessWidth,
    signed_ext: bool,
}

impl PendingMmioRead {
    const fn empty() -> Self {
        Self {
            active: false,
            reg: 0,
            width: AccessWidth::Byte,
            reg_width: AccessWidth::Byte,
            signed_ext: false,
        }
    }
}

#[derive(Clone, Copy)]
struct PendingIoRead {
    active: bool,
    width: AccessWidth,
}

impl PendingIoRead {
    const fn empty() -> Self {
        Self {
            active: false,
            width: AccessWidth::Byte,
        }
    }
}

#[derive(Clone, Copy)]
struct BackendRegs {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rsp: u64,
    rbp: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
    rflags: u64,
}

impl BackendRegs {
    const fn empty() -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rsp: 0,
            rbp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0,
            rflags: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct BackendSregs {
    cr0: u64,
    cr2: u64,
    cr3: u64,
    cr4: u64,
    cr8: u64,
    efer: u64,
    apic_base: u64,
}

impl BackendSregs {
    const fn empty() -> Self {
        Self {
            cr0: 0,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            cr8: 0,
            efer: 0,
            apic_base: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct BackendSegment {
    base: u64,
    limit: u32,
    selector: u32,
    type_: u32,
    present: u32,
    dpl: u32,
    db: u32,
    s: u32,
    l: u32,
    g: u32,
    avl: u32,
    unusable: u32,
}

impl BackendSegment {
    const fn empty() -> Self {
        Self {
            base: 0,
            limit: 0,
            selector: 0,
            type_: 0,
            present: 0,
            dpl: 0,
            db: 0,
            s: 0,
            l: 0,
            g: 0,
            avl: 0,
            unusable: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct BackendDtable {
    base: u64,
    limit: u32,
}

impl BackendDtable {
    const fn empty() -> Self {
        Self { base: 0, limit: 0 }
    }
}

#[derive(Clone, Copy)]
struct CpuidEntry {
    function: u32,
    index: u32,
    flags: u32,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
}

impl CpuidEntry {
    const fn empty() -> Self {
        Self {
            function: 0,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct MsrEntry {
    index: u32,
    data: u64,
}

impl MsrEntry {
    const fn empty() -> Self {
        Self { index: 0, data: 0 }
    }
}

static mut VMS: [BackendVm; MAX_VMS] = [const { BackendVm::empty() }; MAX_VMS];
static mut VCPUS: [BackendVcpu; MAX_VCPUS] = [const { BackendVcpu::empty() }; MAX_VCPUS];

// Per-vCPU software-PLE spin tracking: the guest RIP seen at the previous
// internal exit, and the number of consecutive internal exits at that same RIP.
// Accessed only from that vCPU's own KVM_RUN thread, so relaxed atomics (used
// purely to avoid `static mut` UB) are sufficient; there is no cross-thread
// ordering requirement.
// Per-vCPU software-PLE spin/idle tracking: up to TWO distinct 64-byte guest
// RIP windows the vCPU has been confined to across consecutive internal exits,
// and the length of that streak. Two windows so an *idle* vCPU oscillating
// between hlt and its APIC-EOI handler is still recognised as "stuck", not just
// a vCPU pinned at one fixed spin RIP. Accessed only from that vCPU's own
// KVM_RUN thread, so relaxed atomics (used purely to avoid `static mut` UB) are
// sufficient; there is no cross-thread ordering requirement. Sentinel
// u64::MAX == "window slot empty".
static SOFT_PLE_LAST_RIP: [AtomicU64; MAX_VCPUS] =
    [const { AtomicU64::new(u64::MAX) }; MAX_VCPUS];
static SOFT_PLE_LAST_RIP2: [AtomicU64; MAX_VCPUS] =
    [const { AtomicU64::new(u64::MAX) }; MAX_VCPUS];
static SOFT_PLE_RIP_STREAK: [AtomicUsize; MAX_VCPUS] =
    [const { AtomicUsize::new(0) }; MAX_VCPUS];
static mut PAGE_MAPPINGS: [PageMapping; MAX_PAGE_MAPPINGS] =
    [const { PageMapping::empty() }; MAX_PAGE_MAPPINGS];
static KVM_TIMERS: [SpinNoIrq<VcpuTimerTable>; MAX_VCPUS] =
    [const { SpinNoIrq::new(VcpuTimerTable::empty()) }; MAX_VCPUS];
static mut PERCPUS: [AxVMPerCpu; MAX_HOST_CPUS] =
    [const { AxVMPerCpu::new_uninit() }; MAX_HOST_CPUS];
static mut PERCPU_INITIALIZED: [bool; MAX_HOST_CPUS] = [false; MAX_HOST_CPUS];
static RUN_CONTEXTS: [RunContext; MAX_RUN_CONTEXTS] =
    [const { RunContext::empty() }; MAX_RUN_CONTEXTS];
static FALLBACK_CURRENT_VM_ID: AtomicUsize = AtomicUsize::new(0);
static FALLBACK_CURRENT_VCPU_ID: AtomicUsize = AtomicUsize::new(0);
static NEXT_TIMER_TOKEN: AtomicUsize = AtomicUsize::new(1);

unsafe fn set_current_vcpu_context(vm_handle: u64, vcpu_id: u32) {
    let task_id = unsafe { axvisor_kvm_x86_bridge_current_task_id() };
    let vm_id = vm_handle as usize;
    let vcpu_id = vcpu_id as usize;

    FALLBACK_CURRENT_VM_ID.store(vm_id, Ordering::Release);
    FALLBACK_CURRENT_VCPU_ID.store(vcpu_id, Ordering::Release);

    if task_id == 0 {
        return;
    }

    let mut empty = MAX_RUN_CONTEXTS;
    let mut i = 0;
    while i < MAX_RUN_CONTEXTS {
        let seen = RUN_CONTEXTS[i].task_id.load(Ordering::Acquire);
        if seen == task_id {
            RUN_CONTEXTS[i].vm_id.store(vm_id, Ordering::Release);
            RUN_CONTEXTS[i].vcpu_id.store(vcpu_id, Ordering::Release);
            return;
        }
        if seen == 0 && empty == MAX_RUN_CONTEXTS {
            empty = i;
        }
        i += 1;
    }

    if empty < MAX_RUN_CONTEXTS {
        RUN_CONTEXTS[empty].vm_id.store(vm_id, Ordering::Release);
        RUN_CONTEXTS[empty].vcpu_id.store(vcpu_id, Ordering::Release);
        let _ = RUN_CONTEXTS[empty].task_id.compare_exchange(
            0,
            task_id,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

fn current_run_context() -> Option<(usize, usize)> {
    let task_id = unsafe { axvisor_kvm_x86_bridge_current_task_id() };
    if task_id == 0 {
        return None;
    }

    let mut i = 0;
    while i < MAX_RUN_CONTEXTS {
        if RUN_CONTEXTS[i].task_id.load(Ordering::Acquire) == task_id {
            let vm_id = RUN_CONTEXTS[i].vm_id.load(Ordering::Acquire);
            let vcpu_id = RUN_CONTEXTS[i].vcpu_id.load(Ordering::Acquire);
            return Some((vm_id, vcpu_id));
        }
        i += 1;
    }

    None
}

#[api_impl]
impl host::HostIf for HostIfImpl {
    fn get_host_cpu_num() -> usize {
        unsafe { axvisor_kvm_x86_bridge_get_cpu_num() }
    }

    fn current_host_cpu_id() -> usize {
        unsafe { axvisor_kvm_x86_bridge_current_cpu_id() }
    }

    fn init_percpu() {}

    fn release_host_filesystems() -> ax_errno::AxResult {
        Ok(())
    }

    fn exit(exit_code: i32) -> ! {
        let _ = exit_code;
        bridge_log("axvisor_kvm_x86_bridge host exit requested");
        loop {}
    }

    fn emerg_write_bytes(bytes: &[u8]) {
        unsafe { axvisor_kvm_x86_bridge_log(bytes.as_ptr(), bytes.len()) };
    }
}

#[api_impl]
impl console::ConsoleIf for ConsoleIfImpl {
    fn write_bytes(bytes: &[u8]) {
        unsafe { axvisor_kvm_x86_bridge_log(bytes.as_ptr(), bytes.len()) };
    }

    fn read_bytes(_bytes: &mut [u8]) -> usize {
        0
    }
}

#[api_impl]
impl time::TimeIf for TimeIfImpl {
    fn current_time_nanos() -> time::Nanos {
        unsafe { axvisor_kvm_x86_bridge_current_time_nanos() }
    }

    fn set_oneshot_timer(deadline: time::TimeValue) {
        // deadline 与 current_time_nanos() 同为宿主单调时钟绝对纳秒。
        // 在嵌套虚拟化下 VMX 抢占定时器不可靠，空闲 guest 需要宿主 hrtimer
        // 周期性唤醒（program_timer -> hrtimer -> workfn -> wake_halted_vcpus）。
        let nanos = deadline.as_nanos().min(u64::MAX as u128) as u64;
        if nanos != 0 {
            unsafe { axvisor_kvm_x86_bridge_program_timer(nanos) };
        }
    }
}

#[api_impl]
impl sync::SyncIf for SyncIfImpl {
    fn create_wait_queue() -> usize {
        1
    }

    fn destroy_wait_queue(_queue: usize) {}

    fn wait_queue_wait(_queue: usize) {
        unsafe { axvisor_kvm_x86_bridge_yield_now() };
    }

    fn wait_queue_wait_until(_queue: usize, condition: alloc::boxed::Box<dyn Fn() -> bool + Send + 'static>) {
        while !condition() {
            unsafe { axvisor_kvm_x86_bridge_yield_now() };
        }
    }

    fn wait_queue_wake_one(_queue: usize) {}

    fn wait_queue_wake_all(_queue: usize) {}
}

#[api_impl]
impl task::TaskIf for TaskIfImpl {
    fn spawn_task_raw(
        _options: task::TaskOptions,
        _entry: alloc::boxed::Box<dyn FnOnce() + Send + 'static>,
    ) -> task::TaskHandle {
        task::TaskHandle::from_raw(0)
    }

    fn join_task(_task: task::TaskHandle) {}

    fn current_task() -> Option<task::TaskHandle> {
        let raw = unsafe { axvisor_kvm_x86_bridge_current_task_id() };
        (raw != 0).then_some(task::TaskHandle::from_raw(raw))
    }

    fn yield_now() {
        unsafe { axvisor_kvm_x86_bridge_yield_now() };
    }
}

#[api_impl]
impl memory::MemoryIf for MemoryIfImpl {
    fn alloc_frame() -> Option<memory::PhysAddr> {
        let paddr = unsafe { axvisor_kvm_x86_bridge_alloc_frame() };
        (paddr != 0).then_some(memory::PhysAddr::from(paddr as usize))
    }

    fn dealloc_frame(addr: memory::PhysAddr) {
        unsafe { axvisor_kvm_x86_bridge_dealloc_frame(addr.as_usize() as u64) };
    }

    fn phys_to_virt(addr: memory::PhysAddr) -> memory::VirtAddr {
        memory::VirtAddr::from(unsafe {
            axvisor_kvm_x86_bridge_phys_to_virt(addr.as_usize() as u64)
        } as usize)
    }

    fn virt_to_phys(addr: memory::VirtAddr) -> memory::PhysAddr {
        memory::PhysAddr::from(unsafe {
            axvisor_kvm_x86_bridge_virt_to_phys(addr.as_usize() as u64)
        } as usize)
    }
}

#[api_impl]
impl vmm::VmmIf for VmmIfImpl {
    fn current_vm_id() -> vmm::VMId {
        current_run_context()
            .map(|(vm_id, _)| vm_id)
            .unwrap_or_else(|| FALLBACK_CURRENT_VM_ID.load(Ordering::Acquire))
    }

    fn current_vcpu_id() -> vmm::VCpuId {
        current_run_context()
            .map(|(_, vcpu_id)| vcpu_id)
            .unwrap_or_else(|| FALLBACK_CURRENT_VCPU_ID.load(Ordering::Acquire))
    }

    fn vcpu_num(vm_id: vmm::VMId) -> Option<usize> {
        let index = vm_index_from_handle(vm_id as u64).ok()?;
        unsafe {
            if VMS[index].in_use {
                Some(core::cmp::max(VMS[index].vcpu_count as usize, 1))
            } else {
                None
            }
        }
    }

    fn active_vcpus(vm_id: vmm::VMId) -> Option<usize> {
        let _ = vm_index_from_handle(vm_id as u64).ok()?;
        let mut mask = 0usize;
        unsafe {
            let mut i = 0;
            while i < MAX_VCPUS {
                if VCPUS[i].in_use && VCPUS[i].vm_handle == vm_id as u64 {
                    if VCPUS[i].id < usize::BITS {
                        mask |= 1usize << VCPUS[i].id;
                    }
                }
                i += 1;
            }
        }
        Some(mask)
    }

    fn inject_interrupt(vm_id: vmm::VMId, vcpu_id: vmm::VCpuId, vector: vmm::InterruptVector) {
        let Some(axvm) = axvm_for_vm_id(vm_id) else {
            return;
        };
        let Some(vcpu) = axvm.vcpu(vcpu_id) else {
            return;
        };
        if vcpu.inject_interrupt(vector as usize).is_ok() {
            unsafe {
                axvisor_kvm_x86_bridge_wake_vcpu(vm_id as u64, vcpu_id as u32);
                // Under oversubscription, hand our core to the IPI target so it
                // runs promptly (avoids guest CSD-lock / smp_call_function
                // timeouts). No-op when not oversubscribed. Skipped for
                // self-injection (the caller is the target and already running).
                if vcpu_id != Self::current_vcpu_id() {
                    axvisor_kvm_x86_bridge_boost_vcpu(vm_id as u64, vcpu_id as u32);
                }
            }
        }
    }

    fn inject_interrupt_to_cpus(vm_id: vmm::VMId, vcpu_set: VCpuSet, vector: vmm::InterruptVector) {
        let Some(axvm) = axvm_for_vm_id(vm_id) else {
            return;
        };
        let cur = Self::current_vcpu_id();
        let mut next = vcpu_set.first_index();
        while let Some(vcpu_id) = next {
            if let Some(vcpu) = axvm.vcpu(vcpu_id) {
                if vcpu.inject_interrupt(vector as usize).is_ok() {
                    unsafe {
                        axvisor_kvm_x86_bridge_wake_vcpu(vm_id as u64, vcpu_id as u32);
                        if vcpu_id != cur {
                            axvisor_kvm_x86_bridge_boost_vcpu(vm_id as u64, vcpu_id as u32);
                        }
                    }
                }
            }
            next = vcpu_set.next_index(vcpu_id);
        }
    }

    fn register_timer(
        deadline: time::TimeValue,
        callback: alloc::boxed::Box<dyn FnOnce(time::TimeValue) + Send + 'static>,
    ) -> vmm::CancelToken {
        let vm_id = Self::current_vm_id();
        let vcpu_id = Self::current_vcpu_id();
        register_kvm_timer_on_table(vm_id, vcpu_id, deadline, callback)
    }

    fn register_timer_on_vcpu(
        target_vm_id: vmm::VMId,
        target_vcpu_id: vmm::VCpuId,
        deadline: time::TimeValue,
        callback: alloc::boxed::Box<dyn FnOnce(time::TimeValue) + Send + 'static>,
    ) -> vmm::CancelToken {
        register_kvm_timer_on_table(target_vm_id, target_vcpu_id, deadline, callback)
    }

    fn cancel_timer(token: vmm::CancelToken) {
        // Fast path: the timer almost always belongs to the calling vCPU.
        let vcpu_id = Self::current_vcpu_id();
        if vcpu_id < MAX_VCPUS {
            let mut tbl = KVM_TIMERS[vcpu_id].lock();
            let mut i = 0;
            while i < MAX_TIMERS_PER_VCPU {
                if tbl.entries[i].in_use && tbl.entries[i].token == token {
                    tbl.entries[i] = KvmTimerEntry::empty();
                    drop(tbl);
                    reprogram_next_kvm_timer();
                    return;
                }
                i += 1;
            }
        }

        // Fallback: token was registered on another vCPU's table. Sweep all.
        let mut v = 0;
        while v < MAX_VCPUS {
            if v == vcpu_id {
                v += 1;
                continue;
            }
            let mut tbl = KVM_TIMERS[v].lock();
            let mut i = 0;
            while i < MAX_TIMERS_PER_VCPU {
                if tbl.entries[i].in_use && tbl.entries[i].token == token {
                    tbl.entries[i] = KvmTimerEntry::empty();
                    drop(tbl);
                    reprogram_next_kvm_timer();
                    return;
                }
                i += 1;
            }
            drop(tbl);
            v += 1;
        }
    }

    fn pvclock_write(vm_id: vmm::VMId, vcpu_id: vmm::VCpuId, msr: u32, value: u64) {
        unsafe {
            axvisor_kvm_x86_bridge_pvclock_write(vm_id as u64, vcpu_id as u32, msr, value);
        }
    }

    fn pvclock_refresh(vm_id: vmm::VMId, vcpu_id: vmm::VCpuId) {
        unsafe {
            axvisor_kvm_x86_bridge_pvclock_refresh(vm_id as u64, vcpu_id as u32);
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    bridge_log(&format!("panic in axvisor_kvm_x86_bridge: {info}"));
    loop {}
}

#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    bridge_log(&format!(
        "allocation failure in axvisor_kvm_x86_bridge size={} align={}",
        layout.size(),
        layout.align()
    ));
    loop {}
}

#[unsafe(export_name = "__rust_no_alloc_shim_is_unstable_v2")]
extern "Rust" fn rust_no_alloc_shim_is_unstable_v2() {}

#[unsafe(export_name = "_RNvCs2S033ihgi4L_7___rustc35___rust_no_alloc_shim_is_unstable_v2")]
extern "Rust" fn rust_no_alloc_shim_is_unstable_v2_mangled() {}

#[unsafe(no_mangle)]
pub extern "C" fn __udivti3(n: u128, d: u128) -> u128 {
    if d == 0 {
        return 0;
    }

    let mut quotient = 0u128;
    let mut remainder = 0u128;
    for bit in (0..128).rev() {
        remainder = (remainder << 1) | ((n >> bit) & 1);
        if remainder >= d {
            remainder -= d;
            quotient |= 1u128 << bit;
        }
    }
    quotient
}

fn vm_index_from_handle(handle: u64) -> Result<usize, i32> {
    if handle == 0 || handle as usize > MAX_VMS {
        return Err(-EINVAL);
    }
    Ok(handle as usize - 1)
}

fn vcpu_index_from_handle(handle: u64) -> Result<usize, i32> {
    if handle == 0 || handle as usize > MAX_VCPUS {
        return Err(-EINVAL);
    }
    Ok(handle as usize - 1)
}

unsafe fn find_vcpu_slot_by_id(vm_handle: u64, vcpu_id: u32) -> Option<usize> {
    let mut i = 0;
    while i < MAX_VCPUS {
        if VCPUS[i].in_use && VCPUS[i].vm_handle == vm_handle && VCPUS[i].id == vcpu_id {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn width_bytes(width: AccessWidth) -> u32 {
    width.size() as u32
}

fn width_mask(width: AccessWidth) -> u64 {
    match width {
        AccessWidth::Byte => 0xff,
        AccessWidth::Word => 0xffff,
        AccessWidth::Dword => 0xffff_ffff,
        AccessWidth::Qword => u64::MAX,
    }
}

fn sign_extend_value(value: u64, width: AccessWidth) -> u64 {
    match width {
        AccessWidth::Byte => (value as i8) as i64 as u64,
        AccessWidth::Word => (value as i16) as i64 as u64,
        AccessWidth::Dword => (value as i32) as i64 as u64,
        AccessWidth::Qword => value,
    }
}

fn mmio_read_value(data: *const c_void, len: u32) -> Result<u64, i32> {
    if data.is_null() || len == 0 || len > 8 {
        return Err(-EINVAL);
    }

    let bytes = unsafe { core::slice::from_raw_parts(data.cast::<u8>(), len as usize) };
    let mut raw = [0u8; 8];
    let mut i = 0;
    while i < bytes.len() {
        raw[i] = bytes[i];
        i += 1;
    }

    Ok(u64::from_le_bytes(raw))
}

fn write_u16_le(bytes: &mut [u8], offset: usize, value: u16) {
    let raw = value.to_le_bytes();
    bytes[offset] = raw[0];
    bytes[offset + 1] = raw[1];
}

fn write_u32_le(bytes: &mut [u8], offset: usize, value: u32) {
    let raw = value.to_le_bytes();
    bytes[offset] = raw[0];
    bytes[offset + 1] = raw[1];
    bytes[offset + 2] = raw[2];
    bytes[offset + 3] = raw[3];
}

fn write_u64_le(bytes: &mut [u8], offset: usize, value: u64) {
    let raw = value.to_le_bytes();
    let mut i = 0;
    while i < raw.len() {
        bytes[offset + i] = raw[i];
        i += 1;
    }
}

fn set_default_fxsave_fields(fxsave: &mut X86KvmFxsave) {
    write_u16_le(&mut fxsave.bytes, FXSAVE_FCW_OFFSET, 0x37f);
    write_u32_le(&mut fxsave.bytes, FXSAVE_MXCSR_OFFSET, 0x1f80);
    write_u32_le(
        &mut fxsave.bytes,
        FXSAVE_MXCSR_MASK_OFFSET,
        X86_MXCSR_VALID_MASK,
    );
}

fn copy_bytes(dst: &mut [u8], dst_offset: usize, src: *const u8, len: usize) -> Result<(), i32> {
    if src.is_null() {
        return Err(-EINVAL);
    }
    if dst_offset.checked_add(len).filter(|end| *end <= dst.len()).is_none() {
        return Err(-EINVAL);
    }

    let src = unsafe { core::slice::from_raw_parts(src, len) };
    dst[dst_offset..dst_offset + len].copy_from_slice(src);
    Ok(())
}

fn port_number(port: Port) -> u64 {
    port.number() as u64
}

fn is_x86_inkernel_irqchip_port(port: Port) -> bool {
    matches!(
        port.number(),
        0x20 | 0x21 | 0x40..=0x43 | 0x61 | 0xa0 | 0xa1 | 0x4d0 | 0x4d1 | 0x3f8..=0x3ff
    )
}

fn is_x86_pit_port(port: Port) -> bool {
    matches!(port.number(), 0x40..=0x43 | 0x61)
}

fn is_x86_pic_port(port: Port) -> bool {
    matches!(port.number(), 0x20 | 0x21 | 0xa0 | 0xa1 | 0x4d0 | 0x4d1)
}

fn is_x86_inkernel_mmio_addr(addr: GuestPhysAddr) -> bool {
    matches!(addr.as_usize(), 0xfec0_0000..=0xfec0_0fff)
}

fn should_log_raw_exit(run_result: &Result<AxVCpuExitReason, i32>, retry: usize) -> bool {
    // Benign steady-state exits (idle EOI, halt, in-kernel MMIO/PIO, internal
    // retries) are silenced regardless of retry count so they cannot flood the
    // L1 kernel log and starve the guest serial console. Only structural exits
    // (CpuUp, real MMIO/IO, errors) and periodic retry checkpoints are logged.
    match run_result {
        Ok(AxVCpuExitReason::Nothing)
        | Ok(AxVCpuExitReason::ExternalInterrupt { .. })
        | Ok(AxVCpuExitReason::InterruptEnd { vector: None })
        | Ok(AxVCpuExitReason::PreemptionTimer)
        | Ok(AxVCpuExitReason::Yield)
        | Ok(AxVCpuExitReason::Halt) => retry != 0 && retry % 4096 == 0,
        Ok(AxVCpuExitReason::MmioRead { addr, .. })
        | Ok(AxVCpuExitReason::MmioWrite { addr, .. })
            if is_x86_inkernel_mmio_addr(*addr) =>
        {
            false
        }
        Ok(AxVCpuExitReason::IoRead {
            port,
            ..
        }) if is_x86_inkernel_irqchip_port(*port) => false,
        Ok(AxVCpuExitReason::IoWrite {
            port,
            ..
        }) if is_x86_inkernel_irqchip_port(*port) => false,
        _ => retry == 0 || retry % 4096 == 0,
    }
}

fn note_internal_run_progress(vcpu_id: u32, retry_count: &mut usize, label: &str) {
    *retry_count += 1;
    if *retry_count % KVM_RUN_INTERNAL_EXIT_LOG_INTERVAL == 0 {
        bridge_log(&format!(
            "axvisor_kvm_x86_bridge internal progress vcpu={} count={} at {}",
            vcpu_id, *retry_count, label
        ));
    }
}

fn clear_vcpu_pending_reads(vcpu_index: usize) {
    unsafe {
        VCPUS[vcpu_index].pending_mmio_read = PendingMmioRead::empty();
        VCPUS[vcpu_index].pending_io_read = PendingIoRead::empty();
    }
}

unsafe fn reset_backend_exit_outputs(
    reason: *mut u32,
    width: *mut u32,
    addr: *mut u64,
    data: *mut u64,
    hardware_entry_failure_reason: *mut u64,
) {
    unsafe {
        *reason = AXKVM_BACKEND_EXIT_FAIL_ENTRY;
        *width = 0;
        *addr = 0;
        *data = 0;
        *hardware_entry_failure_reason = 0;
    }
}

unsafe fn prepare_userspace_mmio_read_exit(
    vcpu_index: usize,
    reason: *mut u32,
    width: *mut u32,
    addr: *mut u64,
    mmio_addr: GuestPhysAddr,
    access_width: AccessWidth,
    reg: usize,
    reg_width: AccessWidth,
    signed_ext: bool,
) {
    unsafe {
        *reason = AXKVM_BACKEND_EXIT_MMIO_READ;
        *width = width_bytes(access_width);
        *addr = mmio_addr.as_usize() as u64;
        VCPUS[vcpu_index].pending_mmio_read = PendingMmioRead {
            active: true,
            reg,
            width: access_width,
            reg_width,
            signed_ext,
        };
        VCPUS[vcpu_index].pending_io_read = PendingIoRead::empty();
    }
}

unsafe fn prepare_userspace_mmio_write_exit(
    vcpu_index: usize,
    reason: *mut u32,
    width: *mut u32,
    addr: *mut u64,
    data: *mut u64,
    mmio_addr: GuestPhysAddr,
    access_width: AccessWidth,
    mmio_data: u64,
) {
    unsafe {
        clear_vcpu_pending_reads(vcpu_index);
        *reason = AXKVM_BACKEND_EXIT_MMIO_WRITE;
        *width = width_bytes(access_width);
        *addr = mmio_addr.as_usize() as u64;
        *data = mmio_data;
    }
}

unsafe fn prepare_userspace_io_read_exit(
    vcpu_index: usize,
    reason: *mut u32,
    width: *mut u32,
    addr: *mut u64,
    port: Port,
    access_width: AccessWidth,
) {
    unsafe {
        VCPUS[vcpu_index].pending_mmio_read = PendingMmioRead::empty();
        VCPUS[vcpu_index].pending_io_read = PendingIoRead {
            active: true,
            width: access_width,
        };
        *reason = AXKVM_BACKEND_EXIT_IO_READ;
        *width = width_bytes(access_width);
        *addr = port_number(port);
    }
}

unsafe fn prepare_userspace_io_write_exit(
    vcpu_index: usize,
    reason: *mut u32,
    width: *mut u32,
    addr: *mut u64,
    data: *mut u64,
    port: Port,
    access_width: AccessWidth,
    io_data: u64,
) {
    unsafe {
        clear_vcpu_pending_reads(vcpu_index);
        *reason = AXKVM_BACKEND_EXIT_IO_WRITE;
        *width = width_bytes(access_width);
        *addr = port_number(port);
        *data = io_data;
    }
}

fn make_internal_run_progress(
    axvm: &AxVMRef,
    vcpu_index: usize,
    retry_count: &mut usize,
    label: &str,
    clear_pending_reads: bool,
) -> i32 {
    unsafe {
        if clear_pending_reads {
            clear_vcpu_pending_reads(vcpu_index);
        }
        // Determine oversubscription once: it selects both the timer-drain scope
        // below and the migrate-drop reschedule boundary at the tail.
        let vm_handle = VCPUS[vcpu_index].vm_handle;
        let online = axvisor_kvm_x86_bridge_get_cpu_num();
        let oversubscribed = online != 0 && vm_active_vcpu_count(vm_handle) > online;

        // Timer drain. KVM delivers each vCPU's owed periodic LAPIC tick from the
        // vCPU's OWN thread (kvm_inject_pending_timer_irqs in vcpu_run), and the
        // hrtimer callback that latches the tick runs in HARD IRQ context
        // (HRTIMER_MODE_ABS_HARD, arch/x86/kvm/lapic.c:3085) so it can never be
        // starved by runnable vCPU threads.
        //
        // axvisor instead drains + re-arms in a WQ_HIGHPRI workqueue worker
        // (axkvm_backend_timer_workfn) because the re-arm/kick path takes
        // vm->lock (a sleeping mutex) and cannot run in hardirq. Under
        // oversubscription that worker STARVES: 20 runnable vCPU threads saturate
        // 18 cores and the SCHED_NORMAL worker never gets a slot -- observed
        // directly (run YtAMc9): expire_all last fired at t=9.89s then never
        // again for 230s, so all LAPIC ticks stopped, guest jiffies froze, and
        // SMP bringup wedged at "Bringing up secondary CPUs".
        //
        // Fix: under oversubscription, drain EVERY per-vCPU table right here on
        // the (never-starved) busy vCPU thread, not just this vCPU's own table.
        // This is safe -- expire_all_due_timers is explicitly designed to run
        // from an unrelated thread: each due timer's callback carries its own
        // captured vm_id/vcpu_id and injects to the correct target via a
        // lock-protected software queue, entries are taken under the per-table
        // lock and the callback runs after release (re-arming into its own
        // table). It makes the busy vCPU threads the drain engine, so tick
        // liveness no longer depends on the starvable workqueue. The healthy
        // non-oversubscribed baseline (1/2/4/8/16) keeps the cheap current-vCPU
        // drain and its workqueue, so its timing is untouched.
        if oversubscribed {
            expire_all_due_timers();
        } else {
            expire_due_kvm_timers();
        }
        progress_x86_virtual_irqs(axvm, VCPUS[vcpu_index].id as usize);
        note_internal_run_progress(VCPUS[vcpu_index].id, retry_count, label);
        // Ordinary internal-progress path (InterruptEnd / PreemptionTimer
        // timer housekeeping etc.). This path fires on every benign internal
        // exit even without oversubscription. Use cond_resched() -- the
        // KVM-faithful primitive: it yields this host core ONLY when the host
        // scheduler tick/load-balancer set TIF_NEED_RESCHED, and the vCPU
        // thread stays RUNNABLE (returns as soon as it is picked again). This
        // is exactly how KVM keeps N vCPU threads progressing on M<N cores --
        // CFS time-slices them via need_resched from the host tick -- without
        // blocking a busy-polling AP off the runqueue (which would remove it
        // from CFS's balancing set and starve BSP<->AP bringup). When no
        // reschedule is pending (the healthy 1/2/4/8/16 baseline) it is a cheap
        // no-op, so the timer/interrupt path is not delayed and does not
        // regress. Long-spin / idle-oscillation hand-off still lives in
        // soft_ple_maybe_park (directed_yield + one-tick park), gated on a
        // stable <=2-window RIP streak.
        let _ = *retry_count;

        // KVM-faithful reschedule boundary. The critical structural mismatch
        // with KVM was that `enter_percpu()` holds migrate_disable() across the
        // ENTIRE Rust inner loop (from run_vcpu_raw entry to break-to-C), so a
        // hot-spinning or idle-oscillating vCPU thread stays PINNED to its core
        // and CFS can neither migrate nor rebalance it -- 20 vCPU threads never
        // spread across 18 cores, and cond_resched() is a no-op at nr_running==1
        // (the one-thread-per-core layout), so the pinned thread never yields.
        // Observed effect (run wPVMjF): one vCPU hot-loops 200k+ times while the
        // BSP goes silent/starved and AP bringup wedges -> RCU stall.
        //
        // KVM's vcpu_run outer loop (arch/x86/kvm/x86.c:11756) runs with
        // preemption ENABLED; preempt_disable is held ONLY inside
        // vcpu_enter_guest around the actual VMRUN (x86.c:11370-11580). The
        // reschedule / need_resched handling (__xfer_to_guest_mode_work_pending,
        // x86.c:11788) happens OUTSIDE that, so the thread is freely migratable
        // between guest entries and CFS can load-balance all vCPU threads.
        //
        // We mirror that here: under oversubscription, at this between-entries
        // reschedule point, drop the migrate lock (leave_percpu) so CFS may pick
        // this thread and migrate it, run cond_resched(), then re-acquire
        // (enter_percpu) which re-validates VMX (VMXON) on the possibly-new
        // pCPU. This is safe because run_vcpu_raw already brackets each guest
        // entry with vcpu.bind()/unbind() (axvm/src/vm.rs:1173/1212 =
        // VMPTRLD/VMCLEAR), so no VMCS is loaded across this point and the next
        // entry rebinds to whatever pCPU we end up on. Non-oversubscribed runs
        // (1/2/4/8/16) skip this entirely and keep the cheap in-place
        // cond_resched(), so the healthy baseline is untouched.
        let _ = vm_handle;
        if oversubscribed {
            // Bounded-residency reschedule point for EVERY vCPU thread (BSP and
            // busy-polling AP alike), mirroring KVM's post-VM-exit
            // _TIF_NEED_RESCHED -> schedule() (kernel/entry/virt.c:13,
            // arch/x86/kvm/x86.c:11793). Drop migrate_disable so CFS may migrate
            // this thread, then run the picker UNCONDITIONALLY via schedule()
            // (schedule_now) rather than cond_resched(): at nr_running==1 (the
            // one-thread-per-core oversubscription layout) cond_resched() is a
            // no-op (should_resched needs TIF_NEED_RESCHED, core.c:7694) and
            // yield_to()/directed_yield returns -ESRCH (syscalls.c:1426), so
            // neither can hand off a core. schedule() always enters __schedule()
            // and, crucially, keeps this thread TASK_RUNNING (unlike park_now's
            // schedule_timeout_interruptible(1) which removes an AP from the
            // runnable set for a jiffy and was observed to never resume it),
            // letting CFS co-schedule the sibling that must run (the BSP driving
            // cpuhp_bp_sync_alive) as this thread yields the picker. Re-acquire
            // (enter_percpu) re-validates VMX (VMXON) on the possibly-new pCPU;
            // safe because run_vcpu_raw brackets each guest entry with
            // vcpu.bind()/unbind() (axvm/src/vm.rs:1173/1212 = VMPTRLD/VMCLEAR),
            // so no VMCS is loaded across this point and the next entry rebinds.
            // Bias the picker toward the thread that MUST run to advance SMP
            // bringup BEFORE yielding it. schedule_now() alone only hands the
            // core to whatever CFS picks next; under equal weights CFS keeps
            // re-picking the ~12 hot-spinning APs over the starved BSP (observed
            // run yAMmEf: BSP vcpu0 = 46 enters vs spinners 7000-9000 each), so
            // the serial cpuhp_bp_sync_alive engine never runs and bringup
            // wedges at "Bringing up secondary CPUs".
            //
            // directed_yield() applies the correct target: Priority 0 boosts the
            // AP the BSP is currently waiting on (current_bringup_target, set per
            // CPU_UP), Priority 1 has a spinning AP hand its core to the boot
            // controller (BSP), else Priority 2 round-robins RUNNABLE siblings
            // (post-bringup KVM kvm_vcpu_on_spin behaviour). When yield_to() can
            // not hand off at nr_running==1 it nice-boosts the target
            // (axkvm_bringup_boost, idempotent + watchdog-restored), skewing the
            // subsequent schedule() pick toward it. This logic already existed
            // but was only reached from the rare hardware-PLE `Yield` exit (6
            // calls/run); the hot spinners take InterruptEnd/PreemptionTimer, so
            // routing it here is what actually delivers the BSP boost. We ignore
            // the should_park return (schedule_now below is the core hand-off;
            // block-parking regressed bringup, see soft_ple_maybe_park).
            let _ = axvisor_kvm_x86_bridge_directed_yield(
                vm_handle,
                VCPUS[vcpu_index].id as u32,
            );
            // Drop migrate_disable and unload the VMCS BEFORE any block/reschedule
            // (leave_percpu = VMCLEAR + migrate_enable). This is the ONLY safe point
            // to block: the old block-park froze L1 by blocking inside the
            // migrate_disable window with the VMCS still loaded.
            let _ = leave_percpu();
            // Optional post-bringup spinner park. During SMP bringup the C bridge
            // returns 0 here: current_bringup_target is explicitly RT-boosted and
            // this path must keep all vCPU threads runnable, then use schedule_now()
            // below as the KVM-faithful hand-off. After bringup, a confirmed
            // SCHED_IDLE spinner may still be parked briefly. 1 = parked+woken
            // (core already yielded, skip schedule_now); 0 = not eligible (fall
            // back to runnable resched); <0 = -EINTR, abort the run.
            let park = axvisor_kvm_x86_bridge_spin_park(
                vm_handle,
                VCPUS[vcpu_index].id as u32,
            );
            if park < 0 {
                let reenter = enter_percpu();
                if reenter != 0 {
                    bridge_log(&format!(
                        "make_internal_run_progress reenter_percpu failed (park abort) vcpu={} err={}",
                        VCPUS[vcpu_index].id, reenter
                    ));
                }
                return park;
            }
            if park == 0 {
                axvisor_kvm_x86_bridge_schedule_now();
            }
            let reenter = enter_percpu();
            if reenter != 0 {
                bridge_log(&format!(
                    "make_internal_run_progress reenter_percpu failed vcpu={} err={}",
                    VCPUS[vcpu_index].id, reenter
                ));
            }
        } else {
            axvisor_kvm_x86_bridge_cond_resched();
        }
    }
    0
}

/// Software substitute for hardware PLE, called from the run loop on the busy
/// internal-exit paths (InterruptEnd{None} / Nothing) *before* the benign
/// `make_internal_run_progress` housekeeping.
///
/// Returns `true` (and hands off the core) only when this vCPU looks like a
/// genuinely spinning AP under oversubscription: the machine is oversubscribed,
/// this is not the boot controller, and the guest RIP has been *unchanged* for
/// `SOFT_PLE_RIP_STREAK_THRESHOLD` consecutive internal exits. A benign waiter
/// advances its RIP (or HLTs) long before the streak matures, so this never
/// perturbs the healthy timer path that the 1/2/4/8/16-vCPU baseline needs.
///
/// The hand-off is: directed-yield toward the boot controller / a RUNNABLE
/// sibling (best effort — usually a no-op under CFS spread), then park this
/// spinner for one tick via schedule_timeout(1), which actually frees the core
/// so CFS can schedule the waited-for vCPU. Mirrors the intent of KVM's
/// kvm_vcpu_on_spin + kvm_vcpu_block combination.
fn soft_ple_maybe_park(axvm: &AxVMRef, vcpu_index: usize) {
    let (vcpu_id, vm_handle) =
        unsafe { (VCPUS[vcpu_index].id, VCPUS[vcpu_index].vm_handle) };
    let vidx = vcpu_id as usize;
    if vidx >= MAX_VCPUS {
        return;
    }

    // Only meaningful under oversubscription: more guest vCPUs than host CPUs.
    let online = unsafe { axvisor_kvm_x86_bridge_get_cpu_num() };
    if online == 0 || vm_active_vcpu_count(vm_handle) <= online {
        soft_ple_reset(vidx);
        unsafe { axvisor_kvm_x86_bridge_spin_restore(vm_handle, vcpu_id) };
        return;
    }

    let Some(rip) = current_guest_rip(axvm, vcpu_index) else {
        soft_ple_reset(vidx);
        unsafe { axvisor_kvm_x86_bridge_spin_restore(vm_handle, vcpu_id) };
        return;
    };

    // Track the RIP at 64-byte window granularity (a spinning guest bounces
    // across the handful of instructions in its loop body; masking keeps the
    // streak alive within one code region while a real jump to a different
    // region resets it). BUT an *idle* vCPU under oversubscription is not stuck
    // at ONE region: it oscillates between `hlt` and its APIC-EOI handler
    // (native_apic_mem_eoi) -- two far-apart windows -- taking a timer IRQ,
    // EOIing, and re-halting forever. A single-window streak resets on every
    // flip and never matures, so these idle-but-core-pinning vCPUs never park
    // and (via yield()'s no-op) starve the 2 APs still finishing bringup.
    //
    // So allow the streak to persist as long as the vCPU stays within a stable
    // set of AT MOST TWO windows (SOFT_PLE_LAST_RIP + SOFT_PLE_LAST_RIP2). A
    // THIRD distinct window is genuine forward progress (e.g. single-threaded
    // early BSP boot advancing through code) and resets the streak -- which is
    // exactly what keeps early boot and the 1/2/4/8/16 baseline unperturbed.
    let rip_window = rip & !0x3f;
    let w1 = SOFT_PLE_LAST_RIP[vidx].load(Ordering::Relaxed);
    let w2 = SOFT_PLE_LAST_RIP2[vidx].load(Ordering::Relaxed);
    let streak = if rip_window == w1 || rip_window == w2 {
        SOFT_PLE_RIP_STREAK[vidx].fetch_add(1, Ordering::Relaxed) + 1
    } else if w1 == u64::MAX {
        SOFT_PLE_LAST_RIP[vidx].store(rip_window, Ordering::Relaxed);
        SOFT_PLE_RIP_STREAK[vidx].fetch_add(1, Ordering::Relaxed) + 1
    } else if w2 == u64::MAX {
        SOFT_PLE_LAST_RIP2[vidx].store(rip_window, Ordering::Relaxed);
        SOFT_PLE_RIP_STREAK[vidx].fetch_add(1, Ordering::Relaxed) + 1
    } else {
        // Third distinct window: real progress. Restart tracking from it.
        SOFT_PLE_LAST_RIP[vidx].store(rip_window, Ordering::Relaxed);
        SOFT_PLE_LAST_RIP2[vidx].store(u64::MAX, Ordering::Relaxed);
        SOFT_PLE_RIP_STREAK[vidx].store(1, Ordering::Relaxed);
        // Real forward progress: if this vCPU was demoted to SCHED_IDLE while
        // confirmed spinning, restore it immediately (RIP-driven restore, never
        // timer-driven -- the timer path is starved under the wedge).
        unsafe { axvisor_kvm_x86_bridge_spin_restore(vm_handle, vcpu_id) };
        1
    };

    if streak < SOFT_PLE_RIP_STREAK_THRESHOLD {
        return;
    }

    // Re-arm just below the threshold so, once a vCPU is confirmed spinning/idle
    // in its <=2-window set, it re-parks every SOFT_PLE_REPARK_INTERVAL exits
    // instead of waiting another full threshold window. A third window still
    // resets the streak (see above), so a vCPU that resumes real progress stops
    // parking immediately.
    SOFT_PLE_RIP_STREAK[vidx].store(
        SOFT_PLE_RIP_STREAK_THRESHOLD - SOFT_PLE_REPARK_INTERVAL,
        Ordering::Relaxed,
    );

    // Confirmed spinner hand-off. This vCPU has kept its guest RIP inside a
    // stable <=2-window set for SOFT_PLE_RIP_STREAK_THRESHOLD consecutive
    // internal exits -- under oversubscription this is a genuinely spinning AP
    // busy-polling cpuhp_ap_sync_alive (guest RIP 0x812b02f4/02ed), NOT a
    // forward-progressing vCPU.
    //
    // Best-effort directed-yield only -- the analogue of kvm_vcpu_on_spin()'s
    // yield_to(): boost the boot controller / a runnable sibling on the core we
    // are about to yield. On a fully oversubscribed host it usually returns
    // -ESRCH (both spinner and target own a core alone, nr_running==1 per rq;
    // kernel/sched/syscalls.c:1426), so it is only a hint, never the guarantee.
    //
    // We deliberately do NOT block-park the confirmed spinner here. An earlier
    // bounded schedule_timeout_interruptible(1) block-park (run 3wHKNm/d330uH)
    // FROZE the entire L1 host at t~10-11.5s: descheduling the confirmed AP
    // spinner off its runqueue via park in this run-loop context (inside
    // enter_percpu/migrate_disable, after leave_percpu) wedged L1 -- the whole
    // qemu serial (including the hard-IRQ periodic heartbeat) went silent for
    // the remaining 230s. Real KVM's kvm_vcpu_on_spin() likewise never blocks a
    // PLE spinner (virt/kvm/kvm_main.c:3959); it only yield_to()s. So we keep to
    // the directed-yield hint and let the LAPIC-timer liveness path (the
    // hard-IRQ periodic drain) drive forward progress.
    //
    // The decisive hand-off, though, is the SCHED_IDLE demotion below. On a
    // fully oversubscribed host each confirmed spinner owns an L1 core alone
    // (nr_running==1), where directed_yield()/yield_to() returns -ESRCH and
    // bare schedule()/cond_resched() re-pick the same task: nothing can give the
    // core to the starved BSP driving cpuhp_bp_sync_alive. Demoting the
    // confirmed spinner to SCHED_IDLE keeps it RUNNABLE (so it never leaves
    // CFS's balancing set -- that is what froze L1 with block-park) yet sinks it
    // below every SCHED_NORMAL/RT task, so the instant the nice-boosted BSP (or
    // a migrated runnable sibling, or an L1 kernel thread like the RT witness /
    // timer softirq) lands on this core it preempts the spinner and makes
    // progress. It is restored the moment this vCPU's guest RIP leaves the spin
    // window (the third-window branch above) or leaves oversubscription/HLT.
    unsafe {
        let _ = axvisor_kvm_x86_bridge_directed_yield(vm_handle, vcpu_id);
        axvisor_kvm_x86_bridge_spin_demote(vm_handle, vcpu_id);
    }
}

// Count only vCPUs that are actually started/runnable and thus contending for a
// host CPU. This approximates KVM's oversubscription pressure model, where
// kvm_vcpu_on_spin skips vCPUs that are not schedulable
// (virt/kvm/kvm_main.c:4001 `if (!READ_ONCE(vcpu->ready)) continue;`). A
// registered-but-never-started vCPU (created via KVM_CREATE_VCPU but not yet
// SIPI'd online) exerts no scheduling pressure and must NOT count toward
// oversubscription. The BSP (id 0) is running once the VM boots; an AP is
// running only after it has received SIPI (sipi_started). The raw registered
// count mis-classifies gvisor's pattern of pre-creating all vCPUs up front
// (e.g. 12 created, only vcpu0 running) as oversubscribed, wrongly
// demoting/parking the sole runner. Caveat: a VMM that starts an AP via
// KVM_SET_MP_STATE(RUNNABLE) instead of SIPI would be under-counted here; the
// common Linux/Firecracker/gvisor SIPI path is covered.
fn vm_active_vcpu_count(vm_handle: u64) -> usize {
    let mut n = 0usize;
    unsafe {
        let mut i = 0;
        while i < MAX_VCPUS {
            if VCPUS[i].in_use
                && VCPUS[i].vm_handle == vm_handle
                && (VCPUS[i].id == 0 || VCPUS[i].sipi_started)
            {
                n += 1;
            }
            i += 1;
        }
    }
    n
}

fn handle_inkernel_progress_result(
    axvm: &AxVMRef,
    vcpu_index: usize,
    retry_count: &mut usize,
    label: &str,
    handled: i32,
) -> Result<bool, i32> {
    if handled == 0 {
        let prog = make_internal_run_progress(axvm, vcpu_index, retry_count, label, true);
        if prog < 0 {
            // -EINTR from the spinner park: abort the run so the KVM_RUN ioctl
            // returns and the caller (immediate_exit / signal) is honoured.
            return Err(prog);
        }
        return Ok(true);
    }
    if handled == -EOPNOTSUPP {
        return Ok(false);
    }
    Err(handled)
}

fn handle_x86_inkernel_io_read(axvm: &AxVMRef, vcpu_id: usize, port: Port, width: AccessWidth) -> i32 {
    if !is_x86_inkernel_irqchip_port(port) {
        return -EOPNOTSUPP;
    }

    let result = if is_x86_pit_port(port) {
        axvm.get_devices().x86_pit_handle_read(port, width)
    } else if is_x86_pic_port(port) {
        axvm.get_devices().x86_pic_handle_read(port, width)
    } else {
        axvm.get_devices().handle_port_read(port, width)
    };

    let value = match ax_result_to_errno(result) {
        Ok(value) => value,
        Err(err) => return err,
    };
    match ax_result_to_errno(axvm.complete_x86_io_read(vcpu_id, value, width.size())) {
        Ok(()) => 0,
        Err(err) => err,
    }
}

fn handle_x86_inkernel_io_write(
    axvm: &AxVMRef,
    port: Port,
    width: AccessWidth,
    data: u64,
) -> i32 {
    if !is_x86_inkernel_irqchip_port(port) {
        return -EOPNOTSUPP;
    }

    let value = (data & width_mask(width)) as usize;
    let result = if is_x86_pit_port(port) {
        axvm.get_devices().x86_pit_handle_write(port, width, value)
    } else if is_x86_pic_port(port) {
        axvm.get_devices().x86_pic_handle_write(port, width, value)
    } else {
        axvm.get_devices().handle_port_write(port, width, value)
    };

    match ax_result_to_errno(result) {
        Ok(()) => 0,
        Err(err) => err,
    }
}

fn current_guest_rip(axvm: &AxVMRef, vcpu_index: usize) -> Option<u64> {
    let vcpu_id = unsafe { VCPUS[vcpu_index].id as usize };
    let vcpu = axvm.vcpu(vcpu_id)?;

    // Read the guest RIP that was cached at the most recent VM-exit (captured
    // while the VMCS was loaded). We must NOT do a live `vmread` here:
    // `make_internal_run_progress` runs *after* `run_vcpu_raw` has already
    // unbound the vCPU, so the VMCS is no longer loaded on this CPU and a
    // `vmread` would fail (previously this panicked via `.unwrap()` and spun the
    // physical CPU into an RCU stall). `last_exit_rip()` is a plain field read,
    // and `get_arch_vcpu()` avoids the re-entrancy guard in `with_arch_vcpu`.
    Some(vcpu.get_arch_vcpu().last_exit_rip() as u64)
}

/// Clear a vCPU's software-PLE spin/idle tracking (both RIP windows + streak).
/// Called on the early-return paths of `soft_ple_maybe_park` (not oversubscribed
/// / RIP unavailable) and on every fresh KVM_RUN entry from C, so the streak
/// only counts an uninterrupted run of in-window internal exits.
fn soft_ple_reset(vidx: usize) {
    if vidx >= MAX_VCPUS {
        return;
    }
    SOFT_PLE_LAST_RIP[vidx].store(u64::MAX, Ordering::Relaxed);
    SOFT_PLE_LAST_RIP2[vidx].store(u64::MAX, Ordering::Relaxed);
    SOFT_PLE_RIP_STREAK[vidx].store(0, Ordering::Relaxed);
}

fn handle_x86_inkernel_mmio_read(
    axvm: &AxVMRef,
    vcpu_id: usize,
    addr: GuestPhysAddr,
    width: AccessWidth,
    reg: usize,
    reg_width: AccessWidth,
    signed_ext: bool,
) -> i32 {
    if axvm.get_devices().find_mmio_dev(addr).is_none() {
        return -EOPNOTSUPP;
    }

    let raw = match ax_result_to_errno(axvm.get_devices().handle_mmio_read(addr, width)) {
        Ok(raw) => raw as u64,
        Err(err) => return err,
    };
    let masked = raw & width_mask(width);
    let value = if signed_ext {
        sign_extend_value(masked, width)
    } else {
        masked & width_mask(reg_width)
    };

    let Some(vcpu) = axvm.vcpu(vcpu_id) else {
        return -EINVAL;
    };
    vcpu.set_gpr(reg, value as usize);
    0
}

fn handle_x86_inkernel_mmio_write(
    axvm: &AxVMRef,
    addr: GuestPhysAddr,
    width: AccessWidth,
    data: u64,
) -> i32 {
    if axvm.get_devices().find_mmio_dev(addr).is_none() {
        return -EOPNOTSUPP;
    }

    match ax_result_to_errno(axvm.get_devices().handle_mmio_write(
        addr,
        width,
        (data & width_mask(width)) as usize,
    )) {
        Ok(()) => 0,
        Err(err) => err,
    }
}

enum CpuUpRunAction {
    ContinueCurrentRun,
    ReturnCpuUpExit,
}

unsafe fn handle_x86_cpu_up_exit(
    axvm: &AxVMRef,
    source_vcpu_index: usize,
    target_cpu: u64,
    entry_point: GuestPhysAddr,
    arg: u64,
    retry_count: &mut usize,
    reason: *mut u32,
    addr: *mut u64,
    data: *mut u64,
) -> Result<CpuUpRunAction, i32> {
    let source_vcpu_id = VCPUS[source_vcpu_index].id;
    bridge_log(&format!(
        "axvisor_kvm_x86_bridge cpu_up source_vcpu={} target_cpu={} entry={:#x} arg={:#x}",
        source_vcpu_id,
        target_cpu,
        entry_point.as_usize(),
        arg
    ));

    let Some(target_vcpu) = axvm.vcpu_list().get(target_cpu as usize) else {
        bridge_log(&format!(
            "axvisor_kvm_x86_bridge cpu_up target missing target_cpu={} vcpu_count={}",
            target_cpu,
            axvm.vcpu_list().len()
        ));
        return Err(-EINVAL);
    };

    let Some(target_slot) = find_vcpu_slot_by_id(VCPUS[source_vcpu_index].vm_handle, target_cpu as u32)
    else {
        bridge_log(&format!(
            "axvisor_kvm_x86_bridge cpu_up target slot missing target_cpu={}",
            target_cpu
        ));
        return Err(-EINVAL);
    };

    if VCPUS[target_slot].sipi_started {
        bridge_log(&format!(
            "axvisor_kvm_x86_bridge cpu_up duplicate_sipi target_cpu={} entry={:#x}",
            target_cpu,
            entry_point.as_usize()
        ));
        let prog = make_internal_run_progress(
            axvm,
            source_vcpu_index,
            retry_count,
            "duplicate-sipi",
            true,
        );
        if prog < 0 {
            return Err(prog);
        }
        return Ok(CpuUpRunAction::ContinueCurrentRun);
    }

    match ax_result_to_errno(axvm.setup_x86_ap_vcpu_entry(target_vcpu, entry_point)) {
        Ok(()) => {
            VCPUS[target_slot].sipi_started = true;
        }
        Err(err) => {
            bridge_log(&format!(
                "axvisor_kvm_x86_bridge cpu_up setup failed target_cpu={} entry={:#x} err={} {}",
                target_cpu,
                entry_point.as_usize(),
                err,
                bridge_errno_label(err)
            ));
            return Err(err);
        }
    }

    clear_vcpu_pending_reads(source_vcpu_index);
    *reason = AXKVM_BACKEND_EXIT_CPU_UP;
    *addr = target_cpu;
    *data = entry_point.as_usize() as u64;
    Ok(CpuUpRunAction::ReturnCpuUpExit)
}

/// Shim-internal backend map flag mirroring AXKVM_MAP_RDONLY in the C side
/// (axvisor_kvm_main.c): install the EPT leaf read-only. BIT(31) avoids the
/// KVM_MEM_* bits that also flow through this u32.
const AXKVM_MAP_RDONLY: u32 = 1u32 << 31;
/// KVM_MEM_READONLY (BIT(1)): a read-only memslot.
const KVM_MEM_READONLY: u32 = 1u32 << 1;

fn ram_mapping_flags(kvm_flags: u32) -> MappingFlags {
    let mut flags = MappingFlags::READ | MappingFlags::EXECUTE | MappingFlags::USER;

    // Drop WRITE for a read-only fault-in (VM_IO/PFNMAP resolved non-writable,
    // a read-only VMA, or a KVM_MEM_READONLY slot). Otherwise map writable.
    if (kvm_flags & (AXKVM_MAP_RDONLY | KVM_MEM_READONLY)) == 0 {
        flags |= MappingFlags::WRITE;
    }

    flags
}

fn axvm_for_vm_id(vm_id: usize) -> Option<AxVMRef> {
    let index = vm_index_from_handle(vm_id as u64).ok()?;
    unsafe {
        if VMS[index].in_use && VMS[index].booted {
            VMS[index].vm.clone()
        } else {
            None
        }
    }
}

fn ax_result_to_errno<T>(result: Result<T, ax_errno::AxError>) -> Result<T, i32> {
    result.map_err(|err| {
        let linux_err = ax_errno::LinuxError::from(err);
        -(linux_err.code() as i32)
    })
}

fn bridge_errno_label(err: i32) -> String {
    let errno = -err;
    match ax_errno::LinuxError::try_from(errno) {
        Ok(linux_err) => format!("{linux_err:?}"),
        Err(_) => match ax_errno::AxError::try_from(err) {
            Ok(ax_err) => format!("{ax_err:?}"),
            Err(_) => format!("raw_errno={err}"),
        },
    }
}

#[allow(static_mut_refs)]
unsafe fn enter_percpu() -> i32 {
    unsafe { axvisor_kvm_x86_bridge_migrate_disable() };

    let cpu_id = unsafe { axvisor_kvm_x86_bridge_current_cpu_id() };
    if cpu_id >= MAX_HOST_CPUS {
        unsafe { axvisor_kvm_x86_bridge_migrate_enable() };
        bridge_log(&format!(
            "axvisor_kvm_x86_bridge enter_percpu cpu_id={} exceeds max={}",
            cpu_id, MAX_HOST_CPUS
        ));
        return -EINVAL;
    }

    if !PERCPU_INITIALIZED[cpu_id] {
        match ax_result_to_errno(PERCPUS[cpu_id].init(cpu_id)) {
            Ok(()) => PERCPU_INITIALIZED[cpu_id] = true,
            Err(err) => {
                unsafe { axvisor_kvm_x86_bridge_migrate_enable() };
                return err;
            }
        }
    }

    if PERCPUS[cpu_id].is_enabled() {
        return 0;
    }

    match ax_result_to_errno(PERCPUS[cpu_id].hardware_enable()) {
        Ok(()) => 0,
        Err(err) => {
            unsafe { axvisor_kvm_x86_bridge_migrate_enable() };
            err
        }
    }
}

#[allow(static_mut_refs)]
unsafe fn leave_percpu() -> i32 {
    unsafe { axvisor_kvm_x86_bridge_migrate_enable() };
    0
}

fn find_primary_vcpu_entry(vm_handle: u64) -> usize {
    // SAFETY: All bridge state updates are serialized by the C KVM object layer.
    unsafe {
        let mut i = 0;
        while i < MAX_VCPUS {
            if VCPUS[i].in_use && VCPUS[i].vm_handle == vm_handle && VCPUS[i].id == 0 {
                if VCPUS[i].regs.rip != 0 {
                    return VCPUS[i].regs.rip as usize;
                }
                if VCPUS[i].rip != 0 {
                    return VCPUS[i].rip as usize;
                }
            }
            i += 1;
        }
    }

    0
}

fn build_axvm_config(vm_handle: u64, vm: &BackendVm) -> AxVMCrateConfig {
    let id = vm_handle as usize;
    let entry = find_primary_vcpu_entry(vm_handle);
    let cpu_num = core::cmp::max(vm.vcpu_count, 1) as usize;
    let mut config = AxVMCrateConfig::default();

    config.base.id = id;
    config.base.name = format!("axkvm-{id}");
    config.base.vm_type = VMType::VMTLinux as usize;
    config.base.cpu_num = cpu_num;
    config.kernel.entry_point = entry;
    config.kernel.kernel_load_addr = entry;
    config.kernel.image_location = Some(String::from("memory"));
    config.devices.interrupt_mode = if vm.irqchip_created {
        VMInterruptMode::Emulated
    } else {
        VMInterruptMode::NoIrq
    };
    /*
     * AxVisor KVM-backend mode consumes COM1 in-kernel. This avoids a
     * byte-per-exit serial path that can delay SMP bring-up enough to break
     * Firecracker 2-vCPU guests under debug logging.
     */
    config.devices.emu_devices.push(EmulatedDeviceConfig {
        name: String::from("serial"),
        base_gpa: 0,
        length: 0,
        irq_id: 4,
        emu_type: EmulatedDeviceType::Console,
        cfg_list: Vec::new(),
    });
    if vm.irqchip_created {
        config.devices.emu_devices.push(EmulatedDeviceConfig {
            name: String::from("ioapic"),
            base_gpa: 0xfec0_0000,
            length: 0x1000,
            irq_id: 0,
            emu_type: EmulatedDeviceType::X86IoApic,
            cfg_list: Vec::new(),
        });
        config.devices.emu_devices.push(EmulatedDeviceConfig {
            name: String::from("pic"),
            base_gpa: 0,
            length: 0,
            irq_id: 0,
            emu_type: EmulatedDeviceType::X86Pic,
            cfg_list: Vec::new(),
        });
    }
    if vm.pit_created {
        config.devices.emu_devices.push(EmulatedDeviceConfig {
            name: String::from("pit"),
            base_gpa: 0,
            length: 0,
            irq_id: 0,
            emu_type: EmulatedDeviceType::X86Pit,
            cfg_list: Vec::new(),
        });
    }

    config
}

fn convert_segment(segment: BackendSegment) -> X86KvmSegment {
    X86KvmSegment {
        base: segment.base,
        limit: segment.limit,
        selector: segment.selector as u16,
        type_: segment.type_ as u8,
        present: segment.present != 0,
        dpl: segment.dpl as u8,
        db: segment.db != 0,
        s: segment.s != 0,
        l: segment.l != 0,
        g: segment.g != 0,
        avl: segment.avl != 0,
        unusable: segment.unusable != 0,
    }
}

fn convert_dtable(dtable: BackendDtable) -> X86KvmDtable {
    X86KvmDtable {
        base: dtable.base,
        limit: dtable.limit as u16,
    }
}

unsafe fn reset_vcpu_slot_in_place(index: usize) {
    VCPUS[index].in_use = false;
    VCPUS[index].vm_handle = 0;
    VCPUS[index].id = 0;
    VCPUS[index].rip = 0;
    VCPUS[index].rsp = 0;
    VCPUS[index].rflags = 0;
    VCPUS[index].cr0 = 0;
    VCPUS[index].cr3 = 0;
    VCPUS[index].cr4 = 0;
    VCPUS[index].efer = 0;
    VCPUS[index].apic_base = 0;
    VCPUS[index].cpuid_nent = 0;
    VCPUS[index].nmsrs = 0;
    VCPUS[index].tsc_khz = 0;
    VCPUS[index].regs = BackendRegs::empty();
    VCPUS[index].sregs = BackendSregs::empty();

    let mut i = 0;
    while i < NUM_SEGMENTS {
        VCPUS[index].segments[i] = BackendSegment::empty();
        i += 1;
    }
    i = 0;
    while i < NUM_DTABLES {
        VCPUS[index].dtables[i] = BackendDtable::empty();
        i += 1;
    }
    i = 0;
    while i < MAX_CPUID_ENTRIES {
        VCPUS[index].cpuid_entries[i] = CpuidEntry::empty();
        i += 1;
    }
    i = 0;
    while i < MAX_MSR_ENTRIES {
        VCPUS[index].msr_entries[i] = MsrEntry::empty();
        i += 1;
    }
    VCPUS[index].fxsave_valid = false;
    VCPUS[index].fxsave = X86KvmFxsave::zeroed();

    VCPUS[index].pending_mmio_read = PendingMmioRead::empty();
    VCPUS[index].pending_io_read = PendingIoRead::empty();
    VCPUS[index].state_dirty = false;
    VCPUS[index].sipi_started = false;
}

fn build_kvm_vcpu_state_box(vcpu: &BackendVcpu) -> Box<X86KvmVcpuState> {
    let mut state = Box::<X86KvmVcpuState>::new_uninit();
    let state_ptr = state.as_mut_ptr();
    let cpuid_nent = core::cmp::min(vcpu.cpuid_nent as usize, X86_KVM_MAX_CPUID_ENTRIES);
    let nmsrs = core::cmp::min(vcpu.nmsrs as usize, X86_KVM_MAX_MSR_ENTRIES);

    unsafe {
        ptr::addr_of_mut!((*state_ptr).regs).write(GeneralRegisters::from_kvm_regs(
            vcpu.regs.rax,
            vcpu.regs.rbx,
            vcpu.regs.rcx,
            vcpu.regs.rdx,
            vcpu.regs.rsi,
            vcpu.regs.rdi,
            vcpu.regs.rbp,
            vcpu.regs.r8,
            vcpu.regs.r9,
            vcpu.regs.r10,
            vcpu.regs.r11,
            vcpu.regs.r12,
            vcpu.regs.r13,
            vcpu.regs.r14,
            vcpu.regs.r15,
        ));
        ptr::addr_of_mut!((*state_ptr).rip).write(vcpu.regs.rip);
        ptr::addr_of_mut!((*state_ptr).rsp).write(vcpu.regs.rsp);
        ptr::addr_of_mut!((*state_ptr).rflags).write(vcpu.regs.rflags);
        ptr::addr_of_mut!((*state_ptr).cr0).write(vcpu.sregs.cr0);
        ptr::addr_of_mut!((*state_ptr).cr2).write(vcpu.sregs.cr2);
        ptr::addr_of_mut!((*state_ptr).cr3).write(vcpu.sregs.cr3);
        ptr::addr_of_mut!((*state_ptr).cr4).write(vcpu.sregs.cr4);
        ptr::addr_of_mut!((*state_ptr).cr8).write(vcpu.sregs.cr8);
        ptr::addr_of_mut!((*state_ptr).efer).write(vcpu.sregs.efer);
        ptr::addr_of_mut!((*state_ptr).apic_base).write(vcpu.sregs.apic_base);
        ptr::addr_of_mut!((*state_ptr).xcr0).write(vcpu.xcr0);
        ptr::addr_of_mut!((*state_ptr).fxsave_valid).write(vcpu.fxsave_valid);
        ptr::addr_of_mut!((*state_ptr).fxsave).write(vcpu.fxsave);

        let mut i = 0;
        while i < NUM_SEGMENTS {
            ptr::addr_of_mut!((*state_ptr).segments[i]).write(convert_segment(vcpu.segments[i]));
            i += 1;
        }
        i = 0;
        while i < NUM_DTABLES {
            ptr::addr_of_mut!((*state_ptr).dtables[i]).write(convert_dtable(vcpu.dtables[i]));
            i += 1;
        }
        i = 0;
        while i < X86_KVM_MAX_CPUID_ENTRIES {
            let value = if i < cpuid_nent {
                let entry = vcpu.cpuid_entries[i];
                X86KvmCpuidEntry {
                    function: entry.function,
                    index: entry.index,
                    flags: entry.flags,
                    eax: entry.eax,
                    ebx: entry.ebx,
                    ecx: entry.ecx,
                    edx: entry.edx,
                }
            } else {
                X86KvmCpuidEntry::default()
            };
            ptr::addr_of_mut!((*state_ptr).cpuid_entries[i]).write(value);
            i += 1;
        }
        ptr::addr_of_mut!((*state_ptr).cpuid_nent).write(cpuid_nent);
        i = 0;
        while i < X86_KVM_MAX_MSR_ENTRIES {
            let value = if i < nmsrs {
                let entry = vcpu.msr_entries[i];
                X86KvmMsrEntry {
                    index: entry.index,
                    data: entry.data,
                }
            } else {
                X86KvmMsrEntry::default()
            };
            ptr::addr_of_mut!((*state_ptr).msr_entries[i]).write(value);
            i += 1;
        }
        ptr::addr_of_mut!((*state_ptr).nmsrs).write(nmsrs);

        state.assume_init()
    }
}

fn log_vcpu_state(prefix: &str, vcpu_id: u32, state: &X86KvmVcpuState) {
    const SEG_NAMES: [&str; NUM_SEGMENTS] = ["cs", "ds", "es", "fs", "gs", "ss", "tr", "ldt"];

    bridge_log(&format!(
        "{prefix} vcpu={} rip={:#x} rsp={:#x} rflags={:#x} cr0={:#x} cr3={:#x} cr4={:#x} efer={:#x} apic_base={:#x} xcr0={:#x} fxsave_valid={} cpuid_nent={} nmsrs={}",
        vcpu_id,
        state.rip,
        state.rsp,
        state.rflags,
        state.cr0,
        state.cr3,
        state.cr4,
        state.efer,
        state.apic_base,
        state.xcr0,
        state.fxsave_valid,
        state.cpuid_nent,
        state.nmsrs
    ));
    bridge_log(&format!(
        "{prefix} vcpu={} gdt_base={:#x} gdt_limit={:#x} idt_base={:#x} idt_limit={:#x}",
        vcpu_id,
        state.dtables[0].base,
        state.dtables[0].limit,
        state.dtables[1].base,
        state.dtables[1].limit
    ));

    let mut i = 0;
    while i < NUM_SEGMENTS {
        let seg = state.segments[i];
        bridge_log(&format!(
            "{prefix} vcpu={} seg={} selector={:#x} base={:#x} limit={:#x} type={:#x} present={} dpl={} db={} s={} l={} g={} avl={} unusable={}",
            vcpu_id,
            SEG_NAMES[i],
            seg.selector,
            seg.base,
            seg.limit,
            seg.type_,
            seg.present,
            seg.dpl,
            seg.db,
            seg.s,
            seg.l,
            seg.g,
            seg.avl,
            seg.unusable
        ));
        i += 1;
    }
}

unsafe fn apply_vcpu_state_if_booted(backend_vcpu_index: usize) -> i32 {
    if !VCPUS[backend_vcpu_index].in_use {
        return -EINVAL;
    }
    let vm_index = match vm_index_from_handle(VCPUS[backend_vcpu_index].vm_handle) {
        Ok(index) => index,
        Err(err) => return err,
    };
    if !VMS[vm_index].in_use || !VMS[vm_index].booted {
        return 0;
    }
    if !VCPUS[backend_vcpu_index].state_dirty {
        return 0;
    }
    if VCPUS[backend_vcpu_index].sipi_started {
        bridge_log(&format!(
            "axvisor_kvm_x86_bridge skip stale dirty state after SIPI vcpu={}",
            VCPUS[backend_vcpu_index].id
        ));
        VCPUS[backend_vcpu_index].state_dirty = false;
        return 0;
    }
    let axvm = match VMS[vm_index].vm.as_ref() {
        Some(axvm) => axvm,
        None => return -EINVAL,
    };
    let state = build_kvm_vcpu_state_box(&VCPUS[backend_vcpu_index]);
    log_vcpu_state(
        "apply_vcpu_state",
        VCPUS[backend_vcpu_index].id,
        state.as_ref(),
    );
    match ax_result_to_errno(
        axvm.apply_x86_kvm_vcpu_state(VCPUS[backend_vcpu_index].id as usize, state.as_ref()),
    ) {
        Ok(()) => {
            VCPUS[backend_vcpu_index].state_dirty = false;
            0
        }
        Err(err) => err,
    }
}

unsafe fn apply_vm_vcpu_states_for_boot(vm_index: usize, backend_vm: u64, axvm: &AxVM) -> i32 {
    let mut index = 0;

    while index < MAX_VCPUS {
        if VCPUS[index].in_use
            && VCPUS[index].vm_handle == backend_vm
            && VCPUS[index].state_dirty
        {
            let state = build_kvm_vcpu_state_box(&VCPUS[index]);
            log_vcpu_state("boot_vm apply_vcpu_state", VCPUS[index].id, state.as_ref());
            match ax_result_to_errno(
                axvm.apply_x86_kvm_vcpu_state(VCPUS[index].id as usize, state.as_ref()),
            ) {
                Ok(()) => {
                    VCPUS[index].state_dirty = false;
                }
                Err(err) => {
                    bridge_log(&format!(
                        "boot_vm apply_vcpu_state failed vm={} vcpu={} err={} {}",
                        backend_vm,
                        VCPUS[index].id,
                        err,
                        bridge_errno_label(err)
                    ));
                    return err;
                }
            }
        }
        index += 1;
    }

    if !VMS[vm_index].in_use {
        return -EINVAL;
    }

    0
}

unsafe fn complete_pending_mmio_read(
    backend_vcpu_index: usize,
    data: *const c_void,
    len: u32,
) -> i32 {
    if !VCPUS[backend_vcpu_index].in_use {
        return -EINVAL;
    }
    let pending = VCPUS[backend_vcpu_index].pending_mmio_read;
    if !pending.active {
        return -EINVAL;
    }
    if len != width_bytes(pending.width) {
        return -EINVAL;
    }

    let raw = match mmio_read_value(data, len) {
        Ok(raw) => raw,
        Err(err) => return err,
    };
    let masked = raw & width_mask(pending.width);
    let value = if pending.signed_ext {
        sign_extend_value(masked, pending.width)
    } else {
        masked & width_mask(pending.reg_width)
    };

    let vm_index = match vm_index_from_handle(VCPUS[backend_vcpu_index].vm_handle) {
        Ok(index) => index,
        Err(err) => return err,
    };
    if !VMS[vm_index].in_use || !VMS[vm_index].booted {
        return -EINVAL;
    }
    let axvm = match VMS[vm_index].vm.as_ref() {
        Some(axvm) => axvm,
        None => return -EINVAL,
    };
    let vcpu = match axvm.vcpu(VCPUS[backend_vcpu_index].id as usize) {
        Some(vcpu) => vcpu,
        None => return -EINVAL,
    };

    vcpu.set_gpr(pending.reg, value as usize);
    VCPUS[backend_vcpu_index].pending_mmio_read = PendingMmioRead::empty();
    0
}

unsafe fn complete_pending_io_read(
    backend_vcpu_index: usize,
    data: *const c_void,
    len: u32,
) -> i32 {
    if !VCPUS[backend_vcpu_index].in_use {
        return -EINVAL;
    }
    let pending = VCPUS[backend_vcpu_index].pending_io_read;
    if !pending.active {
        return -EINVAL;
    }
    if len != width_bytes(pending.width) {
        return -EINVAL;
    }

    let raw = match mmio_read_value(data, len) {
        Ok(raw) => raw,
        Err(err) => return err,
    };
    let value = raw & width_mask(pending.width);

    let vm_index = match vm_index_from_handle(VCPUS[backend_vcpu_index].vm_handle) {
        Ok(index) => index,
        Err(err) => return err,
    };
    if !VMS[vm_index].in_use || !VMS[vm_index].booted {
        return -EINVAL;
    }
    let axvm = match VMS[vm_index].vm.as_ref() {
        Some(axvm) => axvm,
        None => return -EINVAL,
    };

    match ax_result_to_errno(axvm.complete_x86_io_read(
        VCPUS[backend_vcpu_index].id as usize,
        value as usize,
        len as usize,
    )) {
        Ok(()) => {
            VCPUS[backend_vcpu_index].pending_io_read = PendingIoRead::empty();
            0
        }
        Err(err) => err,
    }
}

unsafe fn map_page_into_axvm(
    axvm: &AxVMRef,
    gpa: u64,
    hpa: u64,
    flags: u32,
) -> Result<(), i32> {
    ax_result_to_errno(axvm.map_region(
        GuestPhysAddr::from(gpa as usize),
        HostPhysAddr::from(hpa as usize),
        PAGE_SIZE_4K,
        ram_mapping_flags(flags),
    ))
}

unsafe fn replay_page_mappings(vm_handle: u64, axvm: &AxVMRef) -> Result<(), i32> {
    let mut i = 0;
    while i < MAX_PAGE_MAPPINGS {
        let mapping = PAGE_MAPPINGS[i];
        if mapping.in_use && mapping.vm_handle == vm_handle {
            map_page_into_axvm(axvm, mapping.gpa, mapping.hpa, mapping.flags)?;
        }
        i += 1;
    }

    Ok(())
}

unsafe fn apply_ioapic_redirection_table(vm: &BackendVm, axvm: &AxVMRef) {
    if !vm.irqchip_created {
        return;
    }

    let count = core::cmp::min(vm.ioapic_redirtbl_count as usize, KVM_IOAPIC_NUM_PINS);
    let mut applied = 0usize;
    let mut i = 0;
    while i < count {
        if axvm
            .get_devices()
            .x86_ioapic_set_redirection_entry(i, vm.ioapic_redirtbl[i])
        {
            applied += 1;
        }
        i += 1;
    }
    bridge_log(&format!(
        "boot_vm apply_ioapic_redirtbl count={} applied={} gsi5={:#x}",
        count,
        applied,
        if count > 5 { vm.ioapic_redirtbl[5] } else { 0 }
    ));
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_backend_init() -> i32 {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_backend_exit() {}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_create_vm(backend_vm: *mut u64) -> i32 {
    if backend_vm.is_null() {
        return -EINVAL;
    }

    // SAFETY: C serializes backend VM creation through the VM fd lifecycle.
    unsafe {
        let mut i = 0;
        while i < MAX_VMS {
            if !VMS[i].in_use {
                VMS[i] = BackendVm::empty();
                VMS[i].in_use = true;
                *backend_vm = (i + 1) as u64;
                return 0;
            }
            i += 1;
        }
    }

    -ENOSPC
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_destroy_vm(backend_vm: u64) {
    let index = match vm_index_from_handle(backend_vm) {
        Ok(index) => index,
        Err(_) => return,
    };

    // SAFETY: C owns the terminal destroy transition for this handle.
    unsafe {
        cancel_kvm_timers_for_vm(backend_vm as usize);
        let mut i = 0;
        while i < MAX_PAGE_MAPPINGS {
            if PAGE_MAPPINGS[i].in_use && PAGE_MAPPINGS[i].vm_handle == backend_vm {
                PAGE_MAPPINGS[i] = PageMapping::empty();
            }
            i += 1;
        }
        VMS[index] = BackendVm::empty();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vm_state(
    backend_vm: u64,
    version: u32,
    arch: u32,
    irqchip_created: u32,
    pit_created: u32,
    pit_flags: u32,
    tss_addr: u32,
    identity_map_addr: u64,
    nr_irqchips: u32,
    ioapic_redirtbl: *const u64,
    ioapic_redirtbl_count: u32,
) -> i32 {
    let index = match vm_index_from_handle(backend_vm) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: The index was validated and C serializes backend state updates.
    unsafe {
        if !VMS[index].in_use {
            return -EINVAL;
        }
        VMS[index].version = version;
        VMS[index].arch = arch;
        VMS[index].irqchip_created = irqchip_created != 0;
        VMS[index].pit_created = pit_created != 0;
        VMS[index].pit_flags = pit_flags;
        VMS[index].tss_addr = tss_addr;
        VMS[index].identity_map_addr = identity_map_addr;
        VMS[index].nr_irqchips = nr_irqchips;
        VMS[index].ioapic_redirtbl = [0; KVM_IOAPIC_NUM_PINS];
        VMS[index].ioapic_redirtbl_count = 0;
        if !ioapic_redirtbl.is_null() {
            let count = core::cmp::min(ioapic_redirtbl_count as usize, KVM_IOAPIC_NUM_PINS);
            let entries = core::slice::from_raw_parts(ioapic_redirtbl, count);
            let mut i = 0;
            while i < count {
                VMS[index].ioapic_redirtbl[i] = entries[i];
                i += 1;
            }
            VMS[index].ioapic_redirtbl_count = count as u32;
        }
    }

    0
}

/// Map a single guest page into the backend EPT WITHOUT recording it in the
/// bounded PAGE_MAPPINGS replay table. Used by the lazy on-demand fault-in
/// path (huge sparse gvisor slots would overflow the 262K-entry table, and
/// these pages appear only after boot so they never need replay on re-init).
#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_map_page_nolog(
    backend_vm: u64,
    gpa: u64,
    hpa: u64,
    flags: u32,
) -> i32 {
    let index = match vm_index_from_handle(backend_vm) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: C serializes fault-in under the VM mutex.
    unsafe {
        if !VMS[index].in_use {
            return -EINVAL;
        }
        if gpa as usize % PAGE_SIZE_4K != 0 || hpa as usize % PAGE_SIZE_4K != 0 {
            return -EINVAL;
        }
        if let Some(axvm) = VMS[index].vm.as_ref() {
            if let Err(err) = map_page_into_axvm(axvm, gpa, hpa, flags) {
                return err;
            }
        } else {
            return -EINVAL;
        }
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_map_page(
    backend_vm: u64,
    gpa: u64,
    hpa: u64,
    flags: u32,
) -> i32 {
    let index = match vm_index_from_handle(backend_vm) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: C serializes memslot updates under the VM mutex.
    unsafe {
        if !VMS[index].in_use {
            return -EINVAL;
        }
        if gpa as usize % PAGE_SIZE_4K != 0 || hpa as usize % PAGE_SIZE_4K != 0 {
            return -EINVAL;
        }
        if let Some(axvm) = VMS[index].vm.as_ref() {
            if let Err(err) = map_page_into_axvm(axvm, gpa, hpa, flags) {
                return err;
            }
        }

        let mut first_free = MAX_PAGE_MAPPINGS;
        let mut i = 0;
        while i < MAX_PAGE_MAPPINGS {
            if PAGE_MAPPINGS[i].in_use
                && PAGE_MAPPINGS[i].vm_handle == backend_vm
                && PAGE_MAPPINGS[i].gpa == gpa
            {
                PAGE_MAPPINGS[i].hpa = hpa;
                PAGE_MAPPINGS[i].flags = flags;
                return 0;
            }
            if !PAGE_MAPPINGS[i].in_use && first_free == MAX_PAGE_MAPPINGS {
                first_free = i;
            }
            i += 1;
        }
        if first_free == MAX_PAGE_MAPPINGS {
            return -ENOSPC;
        }
        PAGE_MAPPINGS[first_free] = PageMapping {
            in_use: true,
            vm_handle: backend_vm,
            gpa,
            hpa,
            flags,
        };
        VMS[index].page_mapping_count += 1;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_unmap_range(backend_vm: u64, gpa: u64, size: u64) -> i32 {
    let index = match vm_index_from_handle(backend_vm) {
        Ok(index) => index,
        Err(err) => return err,
    };
    if size == 0 {
        return 0;
    }
    if gpa as usize % PAGE_SIZE_4K != 0 || size as usize % PAGE_SIZE_4K != 0 {
        return -EINVAL;
    }

    // SAFETY: C serializes memslot updates under the VM mutex.
    unsafe {
        if !VMS[index].in_use {
            return -EINVAL;
        }
        if let Some(axvm) = VMS[index].vm.as_ref() {
            if let Err(err) = ax_result_to_errno(axvm.unmap_region(
                GuestPhysAddr::from(gpa as usize),
                size as usize,
            )) {
                return err;
            }
        }

        let end = gpa.saturating_add(size);
        let mut i = 0;
        while i < MAX_PAGE_MAPPINGS {
            if PAGE_MAPPINGS[i].in_use
                && PAGE_MAPPINGS[i].vm_handle == backend_vm
                && PAGE_MAPPINGS[i].gpa >= gpa
                && PAGE_MAPPINGS[i].gpa < end
            {
                PAGE_MAPPINGS[i] = PageMapping::empty();
                VMS[index].page_mapping_count = VMS[index].page_mapping_count.saturating_sub(1);
            }
            i += 1;
        }
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_create_vcpu(
    backend_vm: u64,
    vcpu_id: u32,
    backend_vcpu: *mut u64,
) -> i32 {
    if backend_vcpu.is_null() {
        return -EINVAL;
    }
    let vm_index = match vm_index_from_handle(backend_vm) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: C serializes vCPU creation through the VM fd lifecycle.
    unsafe {
        if !VMS[vm_index].in_use {
            return -EINVAL;
        }
        let mut i = 0;
        while i < MAX_VCPUS {
            if !VCPUS[i].in_use {
                reset_vcpu_slot_in_place(i);
                VCPUS[i].in_use = true;
                VCPUS[i].vm_handle = backend_vm;
                VCPUS[i].id = vcpu_id;
                VMS[vm_index].vcpu_count = core::cmp::max(VMS[vm_index].vcpu_count, vcpu_id + 1);
                *backend_vcpu = (i + 1) as u64;
                return 0;
            }
            i += 1;
        }
    }

    -ENOSPC
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_destroy_vcpu(backend_vcpu: u64) {
    let index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(_) => return,
    };

    // SAFETY: C owns the terminal destroy transition for this handle.
    unsafe {
        if VCPUS[index].in_use {
            if let Ok(vm_index) = vm_index_from_handle(VCPUS[index].vm_handle) {
                if VMS[vm_index].in_use && VMS[vm_index].vcpu_count > 0 {
                    let mut max_seen = 0;
                    let mut i = 0;
                    while i < MAX_VCPUS {
                        if i != index && VCPUS[i].in_use && VCPUS[i].vm_handle == VCPUS[index].vm_handle {
                            max_seen = core::cmp::max(max_seen, VCPUS[i].id + 1);
                        }
                        i += 1;
                    }
                    VMS[vm_index].vcpu_count = max_seen;
                }
            }
        }
        reset_vcpu_slot_in_place(index);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_state(
    backend_vcpu: u64,
    _version: u32,
    _arch: u32,
    rip: u64,
    rsp: u64,
    rflags: u64,
    cr0: u64,
    cr3: u64,
    cr4: u64,
    efer: u64,
    apic_base: u64,
    xcr0: u64,
    cpuid_nent: u32,
    nmsrs: u32,
    tsc_khz: u32,
) -> i32 {
    let index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: The index was validated and C serializes backend state updates.
    unsafe {
        if !VCPUS[index].in_use {
            return -EINVAL;
        }
        VCPUS[index].rip = rip;
        VCPUS[index].rsp = rsp;
        VCPUS[index].rflags = rflags;
        VCPUS[index].cr0 = cr0;
        VCPUS[index].cr3 = cr3;
        VCPUS[index].cr4 = cr4;
        VCPUS[index].efer = efer;
        VCPUS[index].apic_base = apic_base;
        VCPUS[index].xcr0 = xcr0;
        VCPUS[index].cpuid_nent = cpuid_nent;
        VCPUS[index].nmsrs = nmsrs;
        VCPUS[index].tsc_khz = tsc_khz;
        VCPUS[index].state_dirty = true;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_regs(
    backend_vcpu: u64,
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rsp: u64,
    rbp: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
    rflags: u64,
) -> i32 {
    let index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: The index was validated and C serializes backend state updates.
    unsafe {
        if !VCPUS[index].in_use {
            return -EINVAL;
        }
        VCPUS[index].regs = BackendRegs {
            rax,
            rbx,
            rcx,
            rdx,
            rsi,
            rdi,
            rsp,
            rbp,
            r8,
            r9,
            r10,
            r11,
            r12,
            r13,
            r14,
            r15,
            rip,
            rflags,
        };
        VCPUS[index].state_dirty = true;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_sregs_control(
    backend_vcpu: u64,
    cr0: u64,
    cr2: u64,
    cr3: u64,
    cr4: u64,
    cr8: u64,
    efer: u64,
    apic_base: u64,
) -> i32 {
    let index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: The index was validated and C serializes backend state updates.
    unsafe {
        if !VCPUS[index].in_use {
            return -EINVAL;
        }
        VCPUS[index].sregs = BackendSregs {
            cr0,
            cr2,
            cr3,
            cr4,
            cr8,
            efer,
            apic_base,
        };
        VCPUS[index].state_dirty = true;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_segment(
    backend_vcpu: u64,
    segment_id: u32,
    base: u64,
    limit: u32,
    selector: u32,
    type_: u32,
    present: u32,
    dpl: u32,
    db: u32,
    s: u32,
    l: u32,
    g: u32,
    avl: u32,
    unusable: u32,
) -> i32 {
    let index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };
    let segment_id = segment_id as usize;
    if segment_id >= NUM_SEGMENTS {
        return -EINVAL;
    }

    // SAFETY: Both indexes were validated and C serializes backend state updates.
    unsafe {
        if !VCPUS[index].in_use {
            return -EINVAL;
        }
        VCPUS[index].segments[segment_id] = BackendSegment {
            base,
            limit,
            selector,
            type_,
            present,
            dpl,
            db,
            s,
            l,
            g,
            avl,
            unusable,
        };
        VCPUS[index].state_dirty = true;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_dtable(
    backend_vcpu: u64,
    table_id: u32,
    base: u64,
    limit: u32,
) -> i32 {
    let index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };
    let table_id = table_id as usize;
    if table_id >= NUM_DTABLES {
        return -EINVAL;
    }

    // SAFETY: Both indexes were validated and C serializes backend state updates.
    unsafe {
        if !VCPUS[index].in_use {
            return -EINVAL;
        }
        VCPUS[index].dtables[table_id] = BackendDtable { base, limit };
        VCPUS[index].state_dirty = true;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_cpuid_entry(
    backend_vcpu: u64,
    entry_index: u32,
    function: u32,
    index: u32,
    flags: u32,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
) -> i32 {
    let index_from_handle = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };
    let entry_index = entry_index as usize;
    if entry_index >= MAX_CPUID_ENTRIES {
        return -EINVAL;
    }

    // SAFETY: Both indexes were validated and C serializes backend state updates.
    unsafe {
        if !VCPUS[index_from_handle].in_use {
            return -EINVAL;
        }
        VCPUS[index_from_handle].cpuid_entries[entry_index] = CpuidEntry {
            function,
            index,
            flags,
            eax,
            ebx,
            ecx,
            edx,
        };
        VCPUS[index_from_handle].state_dirty = true;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_msr_entry(
    backend_vcpu: u64,
    entry_index: u32,
    index: u32,
    data: u64,
) -> i32 {
    let index_from_handle = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };
    let entry_index = entry_index as usize;
    if entry_index >= MAX_MSR_ENTRIES {
        return -EINVAL;
    }

    // SAFETY: Both indexes were validated and C serializes backend state updates.
    unsafe {
        if !VCPUS[index_from_handle].in_use {
            return -EINVAL;
        }
        VCPUS[index_from_handle].msr_entries[entry_index] = MsrEntry { index, data };
        VCPUS[index_from_handle].state_dirty = true;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_fpu(
    backend_vcpu: u64,
    fcw: u32,
    fsw: u32,
    ftwx: u32,
    last_opcode: u32,
    last_ip: u64,
    last_dp: u64,
    mxcsr: u32,
    fpr: *const u8,
    fpr_len: u32,
    xmm: *const u8,
    xmm_len: u32,
) -> i32 {
    let index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };
    if fpr_len as usize != FXSAVE_ST_SIZE || xmm_len as usize != FXSAVE_XMM_SIZE {
        return -EINVAL;
    }
    if (mxcsr & !X86_MXCSR_VALID_MASK) != 0 {
        return -EINVAL;
    }

    let mut fxsave = X86KvmFxsave::zeroed();
    set_default_fxsave_fields(&mut fxsave);
    write_u16_le(&mut fxsave.bytes, FXSAVE_FCW_OFFSET, fcw as u16);
    write_u16_le(&mut fxsave.bytes, FXSAVE_FSW_OFFSET, fsw as u16);
    fxsave.bytes[FXSAVE_FTW_OFFSET] = ftwx as u8;
    write_u16_le(
        &mut fxsave.bytes,
        FXSAVE_FOP_OFFSET,
        last_opcode as u16,
    );
    write_u64_le(&mut fxsave.bytes, FXSAVE_RIP_OFFSET, last_ip);
    write_u64_le(&mut fxsave.bytes, FXSAVE_RDP_OFFSET, last_dp);
    write_u32_le(&mut fxsave.bytes, FXSAVE_MXCSR_OFFSET, mxcsr);
    if let Err(err) = copy_bytes(&mut fxsave.bytes, FXSAVE_ST_OFFSET, fpr, fpr_len as usize) {
        return err;
    }
    if let Err(err) = copy_bytes(&mut fxsave.bytes, FXSAVE_XMM_OFFSET, xmm, xmm_len as usize) {
        return err;
    }

    // SAFETY: The index was validated and C serializes backend state updates.
    unsafe {
        if !VCPUS[index].in_use {
            return -EINVAL;
        }
        VCPUS[index].fxsave = fxsave;
        VCPUS[index].fxsave_valid = true;
        VCPUS[index].state_dirty = true;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_xsave_legacy(
    backend_vcpu: u64,
    region: *const u32,
    region_u32s: u32,
) -> i32 {
    let index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };
    if region.is_null() || (region_u32s as usize) < FXSAVE_SIZE / core::mem::size_of::<u32>() {
        return -EINVAL;
    }

    let bytes = unsafe { core::slice::from_raw_parts(region.cast::<u8>(), FXSAVE_SIZE) };
    let mut fxsave = X86KvmFxsave::zeroed();
    fxsave.bytes.copy_from_slice(bytes);
    if fxsave.bytes[FXSAVE_MXCSR_MASK_OFFSET..FXSAVE_MXCSR_MASK_OFFSET + 4]
        == [0, 0, 0, 0]
    {
        write_u32_le(
            &mut fxsave.bytes,
            FXSAVE_MXCSR_MASK_OFFSET,
            X86_MXCSR_VALID_MASK,
        );
    }

    // SAFETY: The index was validated and C serializes backend state updates.
    unsafe {
        if !VCPUS[index].in_use {
            return -EINVAL;
        }
        VCPUS[index].fxsave = fxsave;
        VCPUS[index].fxsave_valid = true;
        VCPUS[index].state_dirty = true;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_boot_vm(backend_vm: u64) -> i32 {
    let index = match vm_index_from_handle(backend_vm) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: The index was validated and C serializes backend boot.
    unsafe {
        if !VMS[index].in_use {
            return -EINVAL;
        }
        if VMS[index].booted {
            return 0;
        }
        let config = build_axvm_config(backend_vm, &VMS[index]);
        bridge_log(&format!(
            "boot_vm begin vm={} entry={:#x} vcpus={} irqchip={} pit={} emu_devices={} mappings={}",
            backend_vm,
            config.kernel.entry_point,
            config.base.cpu_num,
            VMS[index].irqchip_created,
            VMS[index].pit_created,
            config.devices.emu_devices.len(),
            VMS[index].page_mapping_count
        ));
        let axvm = match ax_result_to_errno(AxVM::new(config.into())) {
            Ok(axvm) => axvm,
            Err(err) => {
                bridge_log(&format!(
                    "boot_vm AxVM::new failed vm={} err={} {}",
                    backend_vm,
                    err,
                    bridge_errno_label(err)
                ));
                return err;
            }
        };
        bridge_log(&format!("boot_vm AxVM::new ok vm={backend_vm}"));
        if let Err(err) = replay_page_mappings(backend_vm, &axvm) {
            bridge_log(&format!(
                "boot_vm replay_page_mappings failed vm={} err={} {} mappings={}",
                backend_vm,
                err,
                bridge_errno_label(err),
                VMS[index].page_mapping_count
            ));
            return err;
        }
        bridge_log(&format!("boot_vm replay_page_mappings ok vm={backend_vm}"));
        set_current_vcpu_context(backend_vm, 0);
        let enable_ret = enter_percpu();
        if enable_ret != 0 {
            bridge_log(&format!(
                "boot_vm enter_percpu failed vm={} err={} {}",
                backend_vm,
                enable_ret,
                bridge_errno_label(enable_ret)
            ));
            return enable_ret;
        }
        bridge_log(&format!("boot_vm enter_percpu ok vm={backend_vm}"));
        if let Err(err) = ax_result_to_errno(axvm.init()) {
            let _ = leave_percpu();
            bridge_log(&format!(
                "boot_vm axvm.init failed vm={} err={} {}",
                backend_vm,
                err,
                bridge_errno_label(err)
            ));
            return err;
        }
        bridge_log(&format!("boot_vm axvm.init ok vm={backend_vm}"));
        let apply_ret = apply_vm_vcpu_states_for_boot(index, backend_vm, &axvm);
        if apply_ret != 0 {
            let _ = leave_percpu();
            return apply_ret;
        }
        apply_ioapic_redirection_table(&VMS[index], &axvm);
        if let Err(err) = ax_result_to_errno(axvm.boot()) {
            let _ = leave_percpu();
            bridge_log(&format!(
                "boot_vm axvm.boot failed vm={} err={} {}",
                backend_vm,
                err,
                bridge_errno_label(err)
            ));
            return err;
        }
        bridge_log(&format!("boot_vm axvm.boot ok vm={backend_vm}"));
        let leave_ret = leave_percpu();
        if leave_ret != 0 {
            bridge_log(&format!(
                "boot_vm leave_percpu failed vm={} err={} {}",
                backend_vm,
                leave_ret,
                bridge_errno_label(leave_ret)
            ));
            return leave_ret;
        }
        bridge_log(&format!("boot_vm leave_percpu ok vm={backend_vm}"));
        VMS[index].vm = Some(axvm);
        VMS[index].booted = true;
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_run_vcpu(
    backend_vcpu: u64,
    reason: *mut u32,
    width: *mut u32,
    addr: *mut u64,
    data: *mut u64,
    hardware_entry_failure_reason: *mut u64,
) -> i32 {
    if reason.is_null()
        || width.is_null()
        || addr.is_null()
        || data.is_null()
        || hardware_entry_failure_reason.is_null()
    {
        return -EINVAL;
    }
    let vcpu_index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: all output pointers were validated non-null and are owned by the
    // C bridge for the duration of this call. Per-vCPU run state is owned by
    // the calling KVM_RUN thread.
    unsafe {
        reset_backend_exit_outputs(reason, width, addr, data, hardware_entry_failure_reason);

        if !VCPUS[vcpu_index].in_use {
            return -EINVAL;
        }
        let vm_index = match vm_index_from_handle(VCPUS[vcpu_index].vm_handle) {
            Ok(index) => index,
            Err(err) => return err,
        };
        if !VMS[vm_index].in_use || !VMS[vm_index].booted {
            return -EINVAL;
        }
        let axvm = match VMS[vm_index].vm.as_ref() {
            Some(axvm) => axvm,
            None => return -EINVAL,
        };
        set_current_vcpu_context(VCPUS[vcpu_index].vm_handle, VCPUS[vcpu_index].id);
        let enable_ret = enter_percpu();
        if enable_ret != 0 {
            return enable_ret;
        }
        let apply_ret = apply_vcpu_state_if_booted(vcpu_index);
        if apply_ret != 0 {
            let _ = leave_percpu();
            return apply_ret;
        }

        let mut run_retry_count = 0usize;
        // Fresh KVM_RUN entry from C: C handled a real progress boundary
        // (HLT wake / MMIO / IO / CpuUp), so re-arm the soft-PLE spin/idle
        // detector cleanly. Also covers the very first entry.
        soft_ple_reset(VCPUS[vcpu_index].id as usize);
        // A fresh entry from C means this vCPU crossed a real progress boundary,
        // so if it had been demoted to SCHED_IDLE as a confirmed spinner,
        // restore its normal scheduling priority now (idempotent no-op if not
        // demoted). RIP-driven restore, never timer-driven.
        axvisor_kvm_x86_bridge_spin_restore(
            VCPUS[vcpu_index].vm_handle,
            VCPUS[vcpu_index].id as u32,
        );
        loop {
            if run_retry_count == 0 {
                // Bounded: log only the first N fresh KVM_RUN entries so a
                // steady-state (idle EOI/halt) vCPU cannot flood the L1 kernel
                // log and starve the guest serial console.
                static ENTER_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
                if ENTER_LOG_COUNT.fetch_add(1, Ordering::Relaxed) < 256 {
                    bridge_log(&format!(
                        "run_vcpu enter raw vcpu={} axvm_vcpu_id={}",
                        VCPUS[vcpu_index].id, VCPUS[vcpu_index].id
                    ));
                }
            }
            let fpu_begin_ret = axvisor_kvm_x86_bridge_guest_fpu_begin();
            if fpu_begin_ret != 0 {
                let _ = leave_percpu();
                return fpu_begin_ret;
            }
            // DIAG (gvisor signal-interruptibility, bounded to first 24 entries):
            // bracket each vmresume so we can tell whether entry N enters the
            // guest ("gv_resume before n=N") and whether it ever returns
            // ("gv_resume after n=N"). A "before" with no matching "after" =
            // vmresume swallowed the entry (guest ran with no VM-exit). sigp
            // records signal_pending(current) at the boundary: if gvisor sent
            // SIGURG before the wedge we should see sigp=1 on a later boundary.
            {
                static GV_RESUME_PROBE: core::sync::atomic::AtomicUsize =
                    core::sync::atomic::AtomicUsize::new(0);
                let n = GV_RESUME_PROBE
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                if n <= 24 {
                    let sigp = axvisor_kvm_x86_bridge_signal_pending();
                    bridge_log(&format!(
                        "gv_resume before n={} vcpu={} retry={} sigp={}",
                        n, VCPUS[vcpu_index].id, run_retry_count, sigp));
                }
            }
            let run_result = ax_result_to_errno(axvm.run_vcpu_raw(VCPUS[vcpu_index].id as usize));
            {
                static GV_RESUME_AFTER: core::sync::atomic::AtomicUsize =
                    core::sync::atomic::AtomicUsize::new(0);
                let n = GV_RESUME_AFTER
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                if n <= 24 {
                    let sigp = axvisor_kvm_x86_bridge_signal_pending();
                    bridge_log(&format!(
                        "gv_resume after n={} vcpu={} sigp={} result={:?}",
                        n, VCPUS[vcpu_index].id, sigp, run_result));
                }
            }
            axvisor_kvm_x86_bridge_guest_fpu_end();
            if should_log_raw_exit(&run_result, run_retry_count) {
                bridge_log(&format!(
                    "run_vcpu raw returned vcpu={} retry={} result={run_result:?}",
                    VCPUS[vcpu_index].id, run_retry_count
                ));
            }

            match run_result {
                Ok(AxVCpuExitReason::Nothing) | Ok(AxVCpuExitReason::ExternalInterrupt { .. }) => {
                    soft_ple_maybe_park(&axvm, vcpu_index);
                    let prog = make_internal_run_progress(
                        &axvm,
                        vcpu_index,
                        &mut run_retry_count,
                        "nothing-or-external-interrupt",
                        false,
                    );
                    if prog < 0 {
                        return prog;
                    }
                    continue;
                }
                Ok(AxVCpuExitReason::Yield) => {
                    // PLE fired: the guest spun past the PAUSE window (lock-holder
                    // preemption / AP-bringup handshake). Directed-yield this
                    // physical CPU to a runnable-but-preempted sibling vCPU
                    // thread as a best-effort hint (like KVM's kvm_vcpu_on_spin),
                    // then resume — handled entirely in-host, never break out to
                    // Firecracker. We do NOT block-park here: the migrate-drop +
                    // cond_resched reschedule boundary in make_internal_run_progress
                    // gives CFS the chance to rebalance while keeping this thread
                    // RUNNABLE (KVM-faithful; block-parking during bringup froze
                    // the VM — see soft_ple_maybe_park rationale).
                    let _ = axvisor_kvm_x86_bridge_directed_yield(
                        VCPUS[vcpu_index].vm_handle,
                        VCPUS[vcpu_index].id as u32,
                    );
                    let prog = make_internal_run_progress(
                        &axvm,
                        vcpu_index,
                        &mut run_retry_count,
                        "pause-yield",
                        false,
                    );
                    if prog < 0 {
                        return prog;
                    }
                    continue;
                }
                Ok(AxVCpuExitReason::InterruptEnd { vector }) => {
                    if vector.is_some() {
                        complete_x86_external_eoi(&axvm, VCPUS[vcpu_index].id as usize, vector);
                    } else {
                        // No vector delivered: the guest re-exited without making
                        // progress. Under oversubscription this is where a spinning
                        // AP loops; the RIP-window streak gate decides whether to park.
                        soft_ple_maybe_park(&axvm, vcpu_index);
                    }
                    let prog = make_internal_run_progress(
                        &axvm,
                        vcpu_index,
                        &mut run_retry_count,
                        "interrupt-end",
                        false,
                    );
                    if prog < 0 {
                        return prog;
                    }
                    continue;
                }
                Ok(AxVCpuExitReason::PreemptionTimer) => {
                    // A vCPU that keeps taking the preemption timer confined to a
                    // stable <=2-window RIP set under oversubscription is spinning
                    // or idle-oscillating; the same soft_ple gate parks it.
                    soft_ple_maybe_park(&axvm, vcpu_index);
                    let prog = make_internal_run_progress(
                        &axvm,
                        vcpu_index,
                        &mut run_retry_count,
                        "preemption-timer",
                        false,
                    );
                    if prog < 0 {
                        return prog;
                    }
                    continue;
                }
                Ok(AxVCpuExitReason::MmioRead {
                    addr: mmio_addr,
                    width: access_width,
                    reg,
                    reg_width,
                    signed_ext,
                }) => {
                    let handled = handle_x86_inkernel_mmio_read(
                        &axvm,
                        VCPUS[vcpu_index].id as usize,
                        mmio_addr,
                        access_width,
                        reg,
                        reg_width,
                        signed_ext,
                    );
                    match handle_inkernel_progress_result(
                        &axvm,
                        vcpu_index,
                        &mut run_retry_count,
                        "inkernel-mmio-read",
                        handled,
                    ) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(err) => {
                            let _ = leave_percpu();
                            return err;
                        }
                    }
                    prepare_userspace_mmio_read_exit(
                        vcpu_index,
                        reason,
                        width,
                        addr,
                        mmio_addr,
                        access_width,
                        reg,
                        reg_width,
                        signed_ext,
                    );
                    break;
                }
                Ok(AxVCpuExitReason::MmioWrite {
                    addr: mmio_addr,
                    width: access_width,
                    data: mmio_data,
                }) => {
                    let handled =
                        handle_x86_inkernel_mmio_write(&axvm, mmio_addr, access_width, mmio_data);
                    match handle_inkernel_progress_result(
                        &axvm,
                        vcpu_index,
                        &mut run_retry_count,
                        "inkernel-mmio-write",
                        handled,
                    ) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(err) => {
                            let _ = leave_percpu();
                            return err;
                        }
                    }
                    prepare_userspace_mmio_write_exit(
                        vcpu_index,
                        reason,
                        width,
                        addr,
                        data,
                        mmio_addr,
                        access_width,
                        mmio_data,
                    );
                    break;
                }
                Ok(AxVCpuExitReason::IoRead {
                    port,
                    width: access_width,
                }) => {
                    let handled = handle_x86_inkernel_io_read(
                        &axvm,
                        VCPUS[vcpu_index].id as usize,
                        port,
                        access_width,
                    );
                    match handle_inkernel_progress_result(
                        &axvm,
                        vcpu_index,
                        &mut run_retry_count,
                        "inkernel-io-read",
                        handled,
                    ) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(err) => {
                            let _ = leave_percpu();
                            return err;
                        }
                    }
                    prepare_userspace_io_read_exit(
                        vcpu_index,
                        reason,
                        width,
                        addr,
                        port,
                        access_width,
                    );
                    break;
                }
                Ok(AxVCpuExitReason::IoWrite {
                    port,
                    width: access_width,
                    data: io_data,
                }) => {
                    let handled = handle_x86_inkernel_io_write(&axvm, port, access_width, io_data);
                    match handle_inkernel_progress_result(
                        &axvm,
                        vcpu_index,
                        &mut run_retry_count,
                        "inkernel-io-write",
                        handled,
                    ) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(err) => {
                            let _ = leave_percpu();
                            return err;
                        }
                    }
                    prepare_userspace_io_write_exit(
                        vcpu_index,
                        reason,
                        width,
                        addr,
                        data,
                        port,
                        access_width,
                        io_data,
                    );
                    break;
                }
                Ok(AxVCpuExitReason::Halt) => {
                    expire_due_kvm_timers();
                    progress_x86_virtual_irqs(&axvm, VCPUS[vcpu_index].id as usize);
                    // Under nested virtualization the VMX preemption timer is
                    // dropped, so an idle guest sitting in `sti; hlt` gets no
                    // periodic exit and its PIT/jiffies freeze. The C halt path
                    // (wait_event_interruptible_timeout, 1 jiffy) is only a
                    // best-effort poll and is unreliable for a low-priority idle
                    // vCPU thread. Arm the host hrtimer at the next PIT deadline
                    // so axkvm_backend_timer_cb -> workfn -> wake_halted_vcpus
                    // wakes this vCPU on time, restoring the ~18ms tick cadence.
                    arm_x86_idle_wakeup_timer(&axvm);
                    clear_vcpu_pending_reads(vcpu_index);
                    *reason = AXKVM_BACKEND_EXIT_HLT;
                    break;
                }
                Ok(AxVCpuExitReason::SystemDown) => {
                    clear_vcpu_pending_reads(vcpu_index);
                    *reason = AXKVM_BACKEND_EXIT_SHUTDOWN;
                    break;
                }
                Ok(AxVCpuExitReason::FailEntry {
                    hardware_entry_failure_reason: fail_reason,
                }) => {
                    bridge_log(&format!(
                        "axvisor_kvm_x86_bridge fail_entry vcpu={} reason={:#x}",
                        VCPUS[vcpu_index].id, fail_reason
                    ));
                    clear_vcpu_pending_reads(vcpu_index);
                    *reason = AXKVM_BACKEND_EXIT_FAIL_ENTRY;
                    *hardware_entry_failure_reason = fail_reason;
                    break;
                }
                Ok(AxVCpuExitReason::CpuUp {
                    target_cpu,
                    entry_point,
                    arg,
                }) => {
                    match handle_x86_cpu_up_exit(
                        &axvm,
                        vcpu_index,
                        target_cpu,
                        entry_point,
                        arg,
                        &mut run_retry_count,
                        reason,
                        addr,
                        data,
                    ) {
                        Ok(CpuUpRunAction::ContinueCurrentRun) => continue,
                        Ok(CpuUpRunAction::ReturnCpuUpExit) => break,
                        Err(err) => {
                            let _ = leave_percpu();
                            return err;
                        }
                    }
                }
                Ok(AxVCpuExitReason::Hypercall { nr, args }) => {
                    // KVM PV hypercall (VMCALL). We advertise the KVM CPUID
                    // signature (for kvm-clock), so the guest may probe PV
                    // services such as KVM_HC_CLOCK_PAIRING (nr=9). We do not
                    // implement any of them in-kernel yet. Mirror native KVM's
                    // default path: hand the guest -KVM_ENOSYS in rax and
                    // resume, so it gracefully falls back to native paths
                    // (e.g. APIC IPIs, raw TSC) instead of killing the VM.
                    // The VMCALL RIP was already advanced in the vmx handler.
                    const KVM_ENOSYS: u64 = 1000;
                    let enosys = KVM_ENOSYS.wrapping_neg();
                    if let Some(vcpu) = axvm.vcpu(VCPUS[vcpu_index].id as usize) {
                        // Index 0 == RAX in GeneralRegisters::set_reg_of_index.
                        vcpu.set_gpr(0, enosys as usize);
                    }
                    bridge_log(&format!(
                        "axvisor_kvm_x86_bridge hypercall enosys vcpu={} nr={} args={args:x?}",
                        VCPUS[vcpu_index].id, nr
                    ));
                    let prog = make_internal_run_progress(
                        &axvm,
                        vcpu_index,
                        &mut run_retry_count,
                        "hypercall-enosys",
                        false,
                    );
                    if prog < 0 {
                        return prog;
                    }
                    continue;
                }
                Ok(AxVCpuExitReason::NestedPageFault { addr, access_flags }) => {
                    // Lazy on-demand guest memory: the memslot was registered
                    // without eager pinning (gvisor's huge sparse slots), so a
                    // real RAM access to an as-yet-unmapped GPA surfaces here.
                    // Fault-in + pin the page and map it into the EPT, then
                    // re-enter the guest. A negative rc means the GPA is not
                    // backed by any valid slot (or OOM) -> genuine error.
                    let write = access_flags.contains(MappingFlags::WRITE);
                    // DIAG(gvisor): bounded log of the faulting GPA + access flags
                    // so we can see exactly which pages are being faulted-in and
                    // whether the same GPA re-faults with a different access type.
                    {
                        static GV_NPF_PROBE: core::sync::atomic::AtomicUsize =
                            core::sync::atomic::AtomicUsize::new(0);
                        let n = GV_NPF_PROBE
                            .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
                            + 1;
                        if n <= 60 {
                            bridge_log(&format!(
                                "gv_npf n={} vcpu={} gpa={:#x} flags={:?} write={}",
                                n,
                                VCPUS[vcpu_index].id,
                                addr.as_usize(),
                                access_flags,
                                write
                            ));
                        }
                    }
                    let rc = axvisor_kvm_x86_bridge_fault_in_gpa(
                        VCPUS[vcpu_index].vm_handle,
                        addr.as_usize() as u64,
                        write as u32,
                    );
                    if rc < 0 {
                        clear_vcpu_pending_reads(vcpu_index);
                        bridge_log(&format!(
                            "axvisor_kvm_x86_bridge nested page fault unresolved vcpu={} gpa={:#x} rc={}",
                            VCPUS[vcpu_index].id,
                            addr.as_usize(),
                            rc
                        ));
                        *reason = AXKVM_BACKEND_EXIT_INTERNAL_ERROR;
                        break;
                    }
                    continue;
                }
                Ok(other) => {
                    clear_vcpu_pending_reads(vcpu_index);
                    bridge_log(&format!(
                        "axvisor_kvm_x86_bridge unsupported exit vcpu={} exit={other:?}",
                        VCPUS[vcpu_index].id
                    ));
                    *reason = AXKVM_BACKEND_EXIT_INTERNAL_ERROR;
                    break;
                }
                Err(err) => {
                    let _ = leave_percpu();
                    bridge_log(&format!(
                        "axvisor_kvm_x86_bridge run_vcpu_raw err vcpu={} err={err}",
                        VCPUS[vcpu_index].id
                    ));
                    return err;
                }
            }
        }
        let leave_ret = leave_percpu();
        if leave_ret != 0 {
            return leave_ret;
        }
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_complete_mmio_read(
    backend_vcpu: u64,
    data: *const c_void,
    len: u32,
) -> i32 {
    let vcpu_index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: The C KVM object layer serializes vCPU ioctl execution and owns
    // the completion buffer for the duration of this call.
    unsafe { complete_pending_mmio_read(vcpu_index, data, len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_complete_io_read(
    backend_vcpu: u64,
    data: *const c_void,
    len: u32,
) -> i32 {
    let vcpu_index = match vcpu_index_from_handle(backend_vcpu) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: The C KVM object layer serializes vCPU ioctl execution and owns
    // the completion buffer for the duration of this call.
    unsafe { complete_pending_io_read(vcpu_index, data, len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_inject_irq(backend_vm: u64, gsi: u32) -> i32 {
    let vm_index = match vm_index_from_handle(backend_vm) {
        Ok(index) => index,
        Err(err) => return err,
    };

    // SAFETY: C serializes VM lifetime and IRQFD work holds the VM file binding.
    unsafe {
        if !VMS[vm_index].in_use || !VMS[vm_index].booted {
            return -EINVAL;
        }
        let axvm = match VMS[vm_index].vm.as_ref() {
            Some(axvm) => axvm,
            None => return -EINVAL,
        };

        if gsi < KVM_IOAPIC_NUM_PINS as u32 {
            if let Some(irq) = axvm.get_devices().x86_ioapic_assert_gsi(gsi as usize) {
                let target_vcpu_id = irq.target_vcpu_id.unwrap_or(0);
                inject_x86_ioapic_irq(axvm, target_vcpu_id, irq.vector, irq.level_triggered);
                bridge_log(&format!(
                    "inject_irq ioapic_event gsi={} vector={:#x} target_vcpu={} level={}",
                    gsi, irq.vector, target_vcpu_id, irq.level_triggered
                ));
                return 0;
            }

            let fallback_entry = 0x20u64 + gsi as u64;
            if axvm
                .get_devices()
                .x86_ioapic_set_redirection_entry(gsi as usize, fallback_entry)
            {
                if let Some(irq) = axvm.get_devices().x86_ioapic_assert_gsi(gsi as usize) {
                    let target_vcpu_id = irq.target_vcpu_id.unwrap_or(0);
                    inject_x86_ioapic_irq(
                        axvm,
                        target_vcpu_id,
                        irq.vector,
                        irq.level_triggered,
                    );
                    bridge_log(&format!(
                        "inject_irq ioapic_fallback_event gsi={} vector={:#x} target_vcpu={} level={}",
                        gsi, irq.vector, target_vcpu_id, irq.level_triggered
                    ));
                    return 0;
                }
            }
        }

        match ax_result_to_errno(axvm.inject_x86_gsi(gsi as usize)) {
            Ok(()) => 0,
            Err(err) => {
                if gsi < KVM_IOAPIC_NUM_PINS as u32 {
                    let entry = 0x20u64 + gsi as u64;
                    if axvm
                        .get_devices()
                        .x86_ioapic_set_redirection_entry(gsi as usize, entry)
                    {
                        match ax_result_to_errno(axvm.inject_x86_gsi(gsi as usize)) {
                            Ok(()) => {
                                bridge_log(&format!(
                                    "inject_irq fallback_ioapic gsi={} vector={:#x} ioapic_err={}",
                                    gsi, entry, err
                                ));
                                return 0;
                            }
                            Err(inject_err) => {
                                bridge_log(&format!(
                                    "inject_irq fallback_ioapic_failed gsi={} vector={:#x} first_err={} inject_err={}",
                                    gsi, entry, err, inject_err
                                ));
                            }
                        }
                    }
                }
                if gsi < 16 && inject_x86_pic_irq(axvm, 0, gsi as u8) {
                    bridge_log(&format!(
                        "inject_irq fallback_pic gsi={} ioapic_err={}",
                        gsi, err
                    ));
                    0
                } else {
                    err
                }
            }
        }
    }
}

/// Drain due LAPIC timers across every per-vCPU table from the host hrtimer
/// workqueue.
///
/// Called from `axkvm_backend_timer_workfn` (process context) so that a vCPU
/// starved off-core under CPU oversubscription still gets its periodic tick
/// expired, re-armed, and injected without depending on that vCPU being
/// scheduled on a host CPU. See `expire_all_due_timers` for the locking rules.
/// This does not set up or rely on any current vM/vCPU run context — each timer
/// callback injects with its own captured id.
#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_x86_bridge_expire_all_due_timers() {
    expire_all_due_timers();
}
