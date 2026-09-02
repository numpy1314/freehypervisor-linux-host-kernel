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

use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::{format, string::String, vec};
use ax_errno::{AxError, AxErrorKind, AxResult};
use axaddrspace::{GuestPhysAddr, GuestVirtAddr, HostPhysAddr, MappingFlags, device::AccessWidth};
use axvcpu::AxVCpuExitReason;
use riscv::asm::hfence_gvma;
use riscv::register::{scause, sie, sstatus};
use riscv_decode::{
    Instruction,
    types::{IType, SType},
};
#[cfg(feature = "sstc")]
use riscv_h::register::vstimecmp;
use riscv_h::register::{
    henvcfg, hgeie, hie, hstatus, htimedelta, hvip,
    vsatp::{self, Vsatp},
    vscause::{self, Vscause},
    vsepc,
    vsie::{self, Vsie},
    vsscratch,
    vsstatus::{self, Vsstatus},
    vstval,
    vstvec::{self, Vstvec},
};
use rustsbi::{Forward, RustSBI};
use sbi_spec::{hsm, legacy, nacl, pmu, rfnc, srst, sta, susp};

use crate::{
    EID_HVC, RISCVVCpuCreateConfig, consts::traps::irq::S_EXT, guest_mem, regs::*, sbi_console::*,
    trap::Exception, vpmu::VirtualPmu,
};

extern "C" {
    fn _run_guest(state: *mut VmCpuRegisters);
}

#[inline(always)]
unsafe fn hfence_gvma_all() {
    hfence_gvma(0, 0);
}

const TINST_PSEUDO_STORE: u32 = 0x3020;
const TINST_PSEUDO_LOAD: u32 = 0x3000;
const EID_TIME: usize = 0x5449_4D45;
const FID_SET_TIMER: usize = 0;
const SSTATUS_FS_MASK: usize = 0b11 << 13;
const SSTATUS_FS_DIRTY: usize = 0b11 << 13;
#[cfg(feature = "sstc")]
const SYSTEM_OPCODE: u32 = 0x73;
#[cfg(feature = "sstc")]
const CSR_STIMECMP: u16 = 0x14d;
static NOTHING_EXIT_COUNTER: AtomicUsize = AtomicUsize::new(0);
static DBCN_TRACE_COUNTER: AtomicUsize = AtomicUsize::new(0);
static DBCN_PARAM_TRACE_COUNTER: AtomicUsize = AtomicUsize::new(0);
static RUN_GUEST_TRACE_COUNTER: AtomicUsize = AtomicUsize::new(0);

const DBCN_MAX_TRANSFER_LEN: usize = 4096;

#[derive(Debug)]
enum AxvisorTrap {
    Interrupt(riscv::register::scause::Interrupt),
    Exception(Exception),
}

#[inline]
fn instr_is_pseudo(ins: u32) -> bool {
    ins == TINST_PSEUDO_STORE || ins == TINST_PSEUDO_LOAD
}

fn dbcn_preview(buf: &[u8], limit: usize) -> String {
    let mut out = String::new();
    for &b in buf.iter().take(limit) {
        match b {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = core::fmt::write(&mut out, format_args!("\\x{b:02x}"));
            }
        }
    }
    if buf.len() > limit {
        out.push_str("...");
    }
    out
}

#[inline]
fn dbcn_should_trace(trace_idx: usize, buf: &[u8]) -> bool {
    trace_idx < 64
        || [
            b"Hello".as_slice(),
            b"VFS".as_slice(),
            b"EXT4".as_slice(),
            b"Mounted root".as_slice(),
            b"Run /axvisor-smoke-init.sh".as_slice(),
            b"Run init".as_slice(),
            b"panic".as_slice(),
            b"Kernel command line".as_slice(),
            b"vda".as_slice(),
        ]
        .iter()
        .any(|needle| buf.windows(needle.len()).any(|window| window == *needle))
}

fn dbcn_trace_params(tag: &'static str, len: usize, gpa: GuestPhysAddr, param: &[usize; 6]) {
    let trace_idx = DBCN_PARAM_TRACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    if trace_idx < 32 || len > DBCN_MAX_TRANSFER_LEN {
        let summary = format!(
            "dbcn_{tag}_params[{trace_idx}]: len={} gpa={:#x} a0={:#x} a1={:#x} a2={:#x}",
            len,
            usize::from(gpa),
            param[0],
            param[1],
            param[2]
        );
        info!("{summary}");
        axvisor_api::host::emerg_write_bytes(summary.as_bytes());
    }
}

fn log_unsupported_trap_summary(vcpu: &RISCVVCpu, tag: &'static str, scause_bits: usize) {
    let summary = format!(
        "riscv_vcpu::unsupported tag={} scause={:#x} sepc={:#x} stval={:#x} htval={:#x} htinst={:#x} vsepc={:#x} vstval={:#x} vsatp={:#x} hgatp={:#x} a0={:#x} a1={:#x} a2={:#x} a3={:#x} a6={:#x} a7={:#x}",
        tag,
        scause_bits,
        vcpu.regs.guest_regs.sepc,
        vcpu.regs.trap_csrs.stval,
        vcpu.regs.trap_csrs.htval,
        vcpu.regs.trap_csrs.htinst,
        vcpu.regs.vs_csrs.vsepc,
        vcpu.regs.vs_csrs.vstval,
        vcpu.regs.vs_csrs.vsatp,
        vcpu.regs.virtual_hs_csrs.hgatp,
        vcpu.regs.guest_regs.gprs.reg(GprIndex::A0),
        vcpu.regs.guest_regs.gprs.reg(GprIndex::A1),
        vcpu.regs.guest_regs.gprs.reg(GprIndex::A2),
        vcpu.regs.guest_regs.gprs.reg(GprIndex::A3),
        vcpu.regs.guest_regs.gprs.reg(GprIndex::A6),
        vcpu.regs.guest_regs.gprs.reg(GprIndex::A7),
    );
    error!("{summary}");
    axvisor_api::host::emerg_write_bytes(summary.as_bytes());
}

fn host_emerg_line(msg: &str) {
    axvisor_api::host::emerg_write_bytes(msg.as_bytes());
    axvisor_api::host::emerg_write_bytes(b"\n");
}

#[inline]
fn read_hgatp() -> usize {
    let value: usize;
    unsafe {
        core::arch::asm!("csrr {value}, hgatp", value = out(reg) value);
    }
    value
}

#[inline]
fn guest_hs_sstatus_bits(mut bits: usize) -> usize {
    // The HS sstatus.FS field is still enforced while executing VS/VU code.
    // Linux host kernel normally runs with FS=Off; inheriting that value makes
    // guest Linux FPU restore paths trap as illegal instructions even when
    // guest vsstatus.FS is enabled.
    bits &= !SSTATUS_FS_MASK;
    bits | SSTATUS_FS_DIRTY
}

#[derive(Default)]
/// A virtual CPU within a guest
pub struct RISCVVCpu {
    hart_id: usize,
    regs: VmCpuRegisters,
    sbi: RISCVVCpuSbi,
}

#[derive(RustSBI)]
struct RISCVVCpuSbi {
    #[rustsbi(pmu)]
    pmu: VirtualPmu,
    // Keep Linux-host forwarding restricted to extensions that do not carry
    // guest-provided memory/shared-memory pointers through the host SBI path.
    //
    // DBCN, HSM suspend/resume-style flows and shared-memory-oriented
    // extensions such as NACL/STA are not safe to forward generically on the
    // Linux-host path until each one gets an explicit guest-memory translation
    // shim. Leave only simple register-only forwarding enabled here.
    #[rustsbi(fence, reset, info, timer)]
    forward: Forward,
}

impl Default for RISCVVCpuSbi {
    #[inline]
    fn default() -> Self {
        Self {
            pmu: VirtualPmu::default(),
            forward: Forward,
        }
    }
}

impl axvcpu::AxArchVCpu for RISCVVCpu {
    type CreateConfig = RISCVVCpuCreateConfig;

    type SetupConfig = ();

    fn new(_vm_id: usize, _vcpu_id: usize, config: Self::CreateConfig) -> AxResult<Self> {
        let mut regs = VmCpuRegisters::default();
        // Setup the guest's general purpose registers.
        // `a0` is the hartid
        regs.guest_regs.gprs.set_reg(GprIndex::A0, config.hart_id);
        // `a1` is the address of the device tree blob.
        regs.guest_regs.gprs.set_reg(GprIndex::A1, config.dtb_addr);

        Ok(Self {
            hart_id: config.hart_id,
            regs,
            sbi: RISCVVCpuSbi::default(),
        })
    }

    fn setup(&mut self, _config: Self::SetupConfig) -> AxResult {
        // Set sstatus.
        let mut sstatus = sstatus::read();
        /*
         * Match KVM's guest reset state: keep SIE clear while still in the
         * HS-mode world-switch path, but set SPIE so SRET enables interrupts
         * only after the CPU has actually entered VS/VU execution. Setting SIE
         * here opens a Linux-host interrupt window between CSR restore and
         * SRET, while stvec/sscratch/GPRs already describe the guest switch.
         */
        sstatus.set_sie(false);
        sstatus.set_spie(true);
        sstatus.set_spp(sstatus::SPP::Supervisor);
        self.regs.guest_regs.sstatus = guest_hs_sstatus_bits(sstatus.bits());

        // Set hstatus.
        let mut hstatus = hstatus::read();
        hstatus.set_spv(true);
        hstatus.set_vsxl(hstatus::VsxlValues::Vsxl64);
        // Set SPVP bit in order to accessing VS-mode memory from HS-mode.
        hstatus.set_spvp(true);
        // Let the guest execute its normal supervisor instructions without
        // spuriously trapping them back to the hypervisor.
        hstatus.set_vtvm(false);
        hstatus.set_vtw(false);
        hstatus.set_vtsr(false);
        unsafe {
            hstatus.write();
        }
        self.regs.guest_regs.hstatus = hstatus.bits();

        let mut hie = hie::Hie::from_bits(0);
        hie.set_vssie(true);
        hie.set_vstie(true);
        hie.set_vseie(true);
        #[cfg(feature = "sstc")]
        {
            // Start with no guest timer deadline armed; a zeroed vstimecmp
            // would be observed as already expired and inject a spurious timer
            // interrupt before Linux programs its first clockevent.
            self.regs.vs_csrs.vstimecmp = usize::MAX;
        }
        self.regs.virtual_hs_csrs.hie = hie.bits();
        self.regs.virtual_hs_csrs.hvip = 0;
        self.regs.virtual_hs_csrs.hgeie = 0;
        self.regs.virtual_hs_csrs.henvcfg = GUEST_HENVCFG;

        Ok(())
    }

    fn set_entry(&mut self, entry: GuestPhysAddr) -> AxResult {
        self.regs.guest_regs.sepc = entry.as_usize();
        Ok(())
    }

    fn set_ept_root(&mut self, ept_root: HostPhysAddr) -> AxResult {
        // AxVM builds a 4-level guest stage-2 page table on RISC-V, so hgatp
        // must use Sv48x4 as well.
        self.regs.virtual_hs_csrs.hgatp = 9usize << 60 | usize::from(ept_root) >> 12;
        Ok(())
    }

    fn run(&mut self) -> AxResult<AxVCpuExitReason> {
        let run_trace_idx = RUN_GUEST_TRACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        unsafe {
            sstatus::clear_sie();
            sie::set_ssoft();
            // Host S-ext must remain enabled while running the guest: Linux
            // guest virtio-mmio passthrough IRQs arrive as HS external
            // interrupts and are then claimed/filtered/completed by the
            // Linux-host PLIC bridge before being injected into the vPLIC.
            sie::set_sext();
            // Keep the current HS timer enable state instead of forcing it on
            // for every VM entry. Guest timer re-arming and host timer users
            // must manage `stimer` explicitly, otherwise a pending HS timer can
            // preempt the guest on every re-entry and starve VS interrupt work.
        }
        if run_trace_idx < 16 || run_trace_idx.is_power_of_two() {
            let summary = format!(
                "riscv_vcpu::before_run_guest idx={} sepc={:#x} a0={:#x} a1={:#x} a2={:#x} a6={:#x} a7={:#x} saved_sstatus={:#x} saved_hstatus={:#x} saved_hie={:#x} saved_hvip={:#x} saved_hgatp={:#x} hw_sstatus={:#x} hw_hstatus={:#x} hw_hie={:#x} hw_hvip={:#x} hw_hgatp={:#x} vsatp={:#x} vstvec={:#x} vsepc={:#x}",
                run_trace_idx,
                self.regs.guest_regs.sepc,
                self.regs.guest_regs.gprs.reg(GprIndex::A0),
                self.regs.guest_regs.gprs.reg(GprIndex::A1),
                self.regs.guest_regs.gprs.reg(GprIndex::A2),
                self.regs.guest_regs.gprs.reg(GprIndex::A6),
                self.regs.guest_regs.gprs.reg(GprIndex::A7),
                self.regs.guest_regs.sstatus,
                self.regs.guest_regs.hstatus,
                self.regs.virtual_hs_csrs.hie,
                self.regs.virtual_hs_csrs.hvip,
                self.regs.virtual_hs_csrs.hgatp,
                sstatus::read().bits(),
                hstatus::read().bits(),
                hie::read().bits(),
                hvip::read().bits(),
                read_hgatp(),
                self.regs.vs_csrs.vsatp,
                self.regs.vs_csrs.vstvec,
                self.regs.vs_csrs.vsepc
            );
            host_emerg_line(&summary);
        }
        unsafe {
            // Safe to run the guest as it only touches memory assigned to it by being owned
            // by its page table
            _run_guest(&mut self.regs);
        }
        // Linux may take a host interrupt as soon as SIE is restored. Snapshot
        // the guest-exit trap CSRs before reopening that window, otherwise
        // scause/stval/htval/htinst can describe the host interrupt instead of
        // the VM exit we are about to handle.
        self.regs.trap_csrs.load_from_hw();
        if run_trace_idx < 16 || run_trace_idx.is_power_of_two() {
            let summary = format!(
                "riscv_vcpu::after_run_guest idx={} scause={:#x} sepc={:#x} stval={:#x} htval={:#x} htinst={:#x} a0={:#x} a1={:#x} a6={:#x} a7={:#x} saved_sstatus={:#x} saved_hstatus={:#x} hw_hie={:#x} hw_hvip={:#x} hw_hgatp={:#x} hw_vsatp={:#x} hw_vstvec={:#x} hw_vsepc={:#x} hw_vscause={:#x} hw_vstval={:#x}",
                run_trace_idx,
                scause::read().bits(),
                self.regs.guest_regs.sepc,
                self.regs.trap_csrs.stval,
                self.regs.trap_csrs.htval,
                self.regs.trap_csrs.htinst,
                self.regs.guest_regs.gprs.reg(GprIndex::A0),
                self.regs.guest_regs.gprs.reg(GprIndex::A1),
                self.regs.guest_regs.gprs.reg(GprIndex::A6),
                self.regs.guest_regs.gprs.reg(GprIndex::A7),
                self.regs.guest_regs.sstatus,
                self.regs.guest_regs.hstatus,
                hie::read().bits(),
                hvip::read().bits(),
                read_hgatp(),
                vsatp::read().bits(),
                vstvec::read().bits(),
                vsepc::read(),
                vscause::read().bits(),
                vstval::read()
            );
            host_emerg_line(&summary);
        }
        unsafe {
            sie::clear_ssoft();
            sstatus::set_sie();
        }
        match self.vmexit_handler() {
            Ok(exit_reason) => Ok(exit_reason),
            Err(err) => {
                let summary = format!(
                    "riscv_vcpu::vmexit_handler_err err={err:?} scause={:#x} sepc={:#x} stval={:#x} htval={:#x} htinst={:#x} vsepc={:#x} vstval={:#x} vsatp={:#x} hgatp={:#x}",
                    scause::read().bits(),
                    self.regs.guest_regs.sepc,
                    self.regs.trap_csrs.stval,
                    self.regs.trap_csrs.htval,
                    self.regs.trap_csrs.htinst,
                    self.regs.vs_csrs.vsepc,
                    self.regs.vs_csrs.vstval,
                    self.regs.vs_csrs.vsatp,
                    self.regs.virtual_hs_csrs.hgatp
                );
                host_emerg_line(&summary);
                Err(err)
            }
        }
    }

    fn bind(&mut self) -> AxResult {
        // Load the vCPU's CSRs from the stored state.
        unsafe {
            // Linux-host runs AxVisor inside kernel threads. Refresh the
            // H-extension config CSRs when this vCPU is loaded, matching the
            // assumption that Asterinas/KVM satisfy before guest entry.
            crate::percpu::setup_hypervisor_csrs();

            let vsatp = Vsatp::from_bits(self.regs.vs_csrs.vsatp);
            vsatp.write();
            let vstvec = Vstvec::from_bits(self.regs.vs_csrs.vstvec);
            vstvec.write();
            let vsepc = self.regs.vs_csrs.vsepc;
            vsepc::write(vsepc);
            let vstval = self.regs.vs_csrs.vstval;
            vstval::write(vstval);
            let htimedelta = self.regs.vs_csrs.htimedelta;
            htimedelta::write(htimedelta);
            let vscause = Vscause::from_bits(self.regs.vs_csrs.vscause);
            vscause.write();
            let vsscratch = self.regs.vs_csrs.vsscratch;
            vsscratch::write(vsscratch);
            let vsstatus = Vsstatus::from_bits(self.regs.vs_csrs.vsstatus);
            vsstatus.write();
            let vsie = Vsie::from_bits(self.regs.vs_csrs.vsie);
            vsie.write();
            #[cfg(feature = "sstc")]
            vstimecmp::write(self.regs.vs_csrs.vstimecmp);
            let hie = hie::Hie::from_bits(self.regs.virtual_hs_csrs.hie);
            hie.write();
            // Restore latched virtual pending interrupts as part of the vCPU
            // context so VM exits do not silently drop timer or external IRQs.
            let hvip = hvip::Hvip::from_bits(self.regs.virtual_hs_csrs.hvip);
            hvip.write();
            hgeie::write(self.regs.virtual_hs_csrs.hgeie);
            self.regs.hyp_regs.henvcfg = henvcfg::read();
            henvcfg::write(self.regs.hyp_regs.henvcfg | self.regs.virtual_hs_csrs.henvcfg);
            core::arch::asm!(
                "csrw hgatp, {hgatp}",
                hgatp = in(reg) self.regs.virtual_hs_csrs.hgatp,
            );
            hfence_gvma_all();
        }
        self.sbi.pmu.backend_bind();
        Ok(())
    }

    fn unbind(&mut self) -> AxResult {
        self.sbi.pmu.backend_unbind();
        // Store the vCPU's CSRs to the stored state.
        unsafe {
            self.regs.vs_csrs.vsatp = vsatp::read().bits();
            self.regs.vs_csrs.vstvec = vstvec::read().bits();
            self.regs.vs_csrs.vsepc = vsepc::read();
            self.regs.vs_csrs.vstval = vstval::read();
            self.regs.vs_csrs.htimedelta = htimedelta::read();
            self.regs.vs_csrs.vscause = vscause::read().bits();
            self.regs.vs_csrs.vsscratch = vsscratch::read();
            self.regs.vs_csrs.vsstatus = vsstatus::read().bits();
            self.regs.vs_csrs.vsie = vsie::read().bits();
            #[cfg(feature = "sstc")]
            {
                self.regs.vs_csrs.vstimecmp = vstimecmp::read();
            }
            self.regs.virtual_hs_csrs.hie = hie::read().bits();
            self.regs.virtual_hs_csrs.hvip = hvip::read().bits();
            self.regs.virtual_hs_csrs.hgeie = hgeie::read();
            core::arch::asm!(
                "csrr {hgatp}, hgatp",
                hgatp = out(reg) self.regs.virtual_hs_csrs.hgatp,
            );
            hie::Hie::from_bits(0).write();
            // Clear host-side pending state after saving it to avoid leaking a
            // previous guest's virtual IRQs into later host/guest execution.
            hvip::Hvip::from_bits(0).write();
            hgeie::write(0);
            henvcfg::write(self.regs.hyp_regs.henvcfg);
            #[cfg(feature = "sstc")]
            vstimecmp::write(usize::MAX);
            core::arch::asm!("csrw hgatp, x0");
            hfence_gvma_all();
        }
        Ok(())
    }

    /// Set one of the vCPU's general purpose register.
    fn set_gpr(&mut self, index: usize, val: usize) {
        match index {
            0 => {
                // Do nothing, x0 is hardwired to zero
            }
            1..=31 => {
                if let Some(gpr_index) = GprIndex::from_raw(index as u32) {
                    self.set_gpr_from_gpr_index(gpr_index, val);
                } else {
                    warn!("RISCVVCpu: Failed to map general purpose register index: {index}");
                }
            }
            _ => {
                warn!("RISCVVCpu: Unsupported general purpose register index: {index}");
            }
        }
    }

    fn inject_interrupt(&mut self, _vector: usize) -> AxResult {
        match _vector {
            x if x == crate::consts::traps::irq::S_SOFT => unsafe {
                hvip::set_vssip();
                Ok(())
            },
            x if x == crate::consts::traps::irq::S_TIMER => unsafe {
                hvip::set_vstip();
                Ok(())
            },
            x if x == crate::consts::traps::irq::S_EXT => unsafe {
                hvip::set_vseip();
                Ok(())
            },
            _ => Err(AxError::Unsupported),
        }
    }

    fn set_return_value(&mut self, val: usize) {
        self.set_gpr_from_gpr_index(GprIndex::A0, val);
    }
}

impl RISCVVCpu {
    /// Capture any virtual pending interrupt bits that were raised after the
    /// last `unbind()` so the next `bind()` does not overwrite them with stale
    /// saved state.
    pub fn latch_hvip_from_hw(&mut self) {
        self.regs.virtual_hs_csrs.hvip |= hvip::read().bits();
    }
}

impl RISCVVCpu {
    #[inline]
    fn program_guest_timer(&mut self, deadline: usize) {
        #[cfg(feature = "sstc")]
        {
            self.regs.vs_csrs.vstimecmp = deadline;
        }
        sbi_rt::set_timer(deadline as u64);
        unsafe {
            // The guest has consumed the current VS timer event and programmed
            // a new deadline, so clear the injected VS timer pending bit and
            // re-arm HS timer delivery for the next expiration.
            hvip::clear_vstip();
            #[cfg(feature = "sstc")]
            vstimecmp::write(deadline);
            sie::set_stimer();
        }
    }

    /// Gets one of the vCPU's general purpose registers.
    pub fn get_gpr(&self, index: GprIndex) -> usize {
        self.regs.guest_regs.gprs.reg(index)
    }

    /// Set one of the vCPU's general purpose register.
    pub fn set_gpr_from_gpr_index(&mut self, index: GprIndex, val: usize) {
        self.regs.guest_regs.gprs.set_reg(index, val);
    }

    /// Advance guest pc by `instr_len` bytes
    pub fn advance_pc(&mut self, instr_len: usize) {
        match self.regs.guest_regs.sepc.checked_add(instr_len) {
            Some(next_pc) => {
                self.regs.guest_regs.sepc = next_pc;
                self.regs.vs_csrs.vsepc = next_pc;
                // `run_vcpu()` can re-enter the same already-bound vCPU
                // without going through a fresh `bind()`, so keep the live VS
                // EPC in sync with the cached guest PC after emulating an
                // instruction in the host.
                unsafe {
                    vsepc::write(next_pc);
                }
            }
            None => {
                error!("advance_pc overflow on linux-host path");
                self.regs.guest_regs.sepc = usize::MAX;
                self.regs.vs_csrs.vsepc = usize::MAX;
                unsafe {
                    vsepc::write(usize::MAX);
                }
            }
        }
    }

    /// Gets the vCPU's registers.
    pub fn regs(&mut self) -> &mut VmCpuRegisters {
        &mut self.regs
    }
}

impl RISCVVCpu {
    #[inline]
    fn log_nothing_exit(&self, tag: &'static str, scause_bits: usize) {
        let count = NOTHING_EXIT_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        let is_dbcn_traffic = matches!(
            tag,
            "sbi_dbcn_write"
                | "sbi_dbcn_write_byte"
                | "sbi_dbcn_read"
                | "sbi_dbcn_read_copy_fault"
                | "sbi_dbcn_write_copy_fault"
                | "sbi_dbcn_unsupported_fid"
        );
        let should_log = if is_dbcn_traffic {
            count <= 4
        } else {
            count <= 8 || count.is_power_of_two()
        };
        if should_log {
            info!(
                "riscv_vcpu::nothing count={} tag={} sepc={:#x} scause={:#x} stval={:#x} htval={:#x} htinst={:#x} a0={:#x} a1={:#x} a6={:#x} a7={:#x}",
                count,
                tag,
                self.regs.guest_regs.sepc,
                scause_bits,
                self.regs.trap_csrs.stval,
                self.regs.trap_csrs.htval,
                self.regs.trap_csrs.htinst,
                self.regs.guest_regs.gprs.reg(GprIndex::A0),
                self.regs.guest_regs.gprs.reg(GprIndex::A1),
                self.regs.guest_regs.gprs.reg(GprIndex::A6),
                self.regs.guest_regs.gprs.reg(GprIndex::A7),
            );
            let summary = format!(
                "riscv_vcpu::nothing count={} tag={} sepc={:#x} scause={:#x} stval={:#x} htval={:#x} htinst={:#x}",
                count,
                tag,
                self.regs.guest_regs.sepc,
                scause_bits,
                self.regs.trap_csrs.stval,
                self.regs.trap_csrs.htval,
                self.regs.trap_csrs.htinst,
            );
            axvisor_api::host::emerg_write_bytes(summary.as_bytes());
        }
    }

    /// Inject a synchronous VS exception so the guest handles a fault that happened during
    /// hypervisor-side instruction emulation.
    fn inject_guest_exception(&mut self, exception: Exception, fault_addr: GuestVirtAddr) {
        let mut vsstatus = vsstatus::read();
        let hstatus = hstatus::Hstatus::from_bits(self.regs.guest_regs.hstatus);
        let vstvec = vstvec::read().bits();
        let trap_pc = vstvec & !0b11;

        vsstatus.set_spie(vsstatus.sie());
        vsstatus.set_sie(false);
        vsstatus.set_spp(hstatus.spvp());

        self.regs.vs_csrs.vstvec = vstvec;
        self.regs.vs_csrs.vsepc = self.regs.guest_regs.sepc;
        self.regs.vs_csrs.vscause = exception as usize;
        self.regs.vs_csrs.vstval = fault_addr.as_usize();
        self.regs.vs_csrs.vsstatus = vsstatus.bits();
        self.regs.guest_regs.sepc = trap_pc;

        // `run_vcpu()` may re-enter the same bound vCPU without reloading the
        // cached VS CSR block, so keep the live CSRs in sync with the cache.
        unsafe {
            vsstatus.write();
            vscause::Vscause::from_bits(self.regs.vs_csrs.vscause).write();
            vstval::write(self.regs.vs_csrs.vstval);
            vsepc::write(self.regs.vs_csrs.vsepc);
        }
    }

    fn handle_guest_instruction_fetch_fault(
        &mut self,
        fault: guest_mem::GuestInstructionFetchFault,
    ) -> AxResult<AxVCpuExitReason> {
        let scause_bits = scause::read().bits();
        match fault {
            // HLVX reports load-class faults, but the emulated operation is a
            // guest instruction fetch. Convert them before injecting to VS mode.
            guest_mem::GuestInstructionFetchFault::PageFault { addr } => {
                self.inject_guest_exception(Exception::InstructionPageFault, addr);
                self.log_nothing_exit("guest_instruction_fetch_page_fault", scause_bits);
                Ok(AxVCpuExitReason::Nothing)
            }
            guest_mem::GuestInstructionFetchFault::AccessFault { addr } => {
                self.inject_guest_exception(Exception::InstructionFault, addr);
                self.log_nothing_exit("guest_instruction_fetch_access_fault", scause_bits);
                Ok(AxVCpuExitReason::Nothing)
            }
            guest_mem::GuestInstructionFetchFault::Misaligned { addr } => {
                self.inject_guest_exception(Exception::InstructionMisaligned, addr);
                self.log_nothing_exit("guest_instruction_fetch_misaligned", scause_bits);
                Ok(AxVCpuExitReason::Nothing)
            }
            guest_mem::GuestInstructionFetchFault::GuestPageFault { addr } => {
                // G-stage faults must stay visible to AxVM so it can populate or
                // reject the nested mapping.
                Ok(AxVCpuExitReason::NestedPageFault {
                    addr,
                    access_flags: MappingFlags::EXECUTE,
                })
            }
            guest_mem::GuestInstructionFetchFault::Unhandled {
                scause,
                stval,
                htval,
            } => {
                let _ = (scause, stval, htval);
                error!("Unhandled HLVX fault while fetching guest instruction");
                Err(ax_errno::ax_err_type!(
                    Unsupported,
                    "unhandled riscv HLVX fault while fetching guest instruction"
                ))
            }
        }
    }

    fn reflect_guest_sync_exception(
        &mut self,
        exception: Exception,
    ) -> AxResult<AxVCpuExitReason> {
        let scause_bits = scause::read().bits();
        let fault_addr = GuestVirtAddr::from_usize(self.regs.trap_csrs.stval);
        self.inject_guest_exception(exception, fault_addr);
        self.log_nothing_exit("reflect_guest_sync_exception", scause_bits);
        Ok(AxVCpuExitReason::Nothing)
    }

    fn vmexit_handler(&mut self) -> AxResult<AxVCpuExitReason> {
        use riscv::register::scause::Interrupt;

        let scause_bits = self.regs.trap_csrs.scause;
        let scause_code = scause_bits & !(1usize << (usize::BITS as usize - 1));
        let scause_is_interrupt = (scause_bits >> (usize::BITS as usize - 1)) != 0;

        // Keep the Linux-host RISC-V vmexit fast path free of heavy formatting.
        // Rich trace formatting here has been able to fault inside host-side
        // memcpy/string machinery before the actual guest runtime bug is
        // isolated.
        trace!(
            "vmexit_handler: scause={:#x}, sepc={:#x}, stval={:#x}",
            scause_bits,
            self.regs.guest_regs.sepc,
            self.regs.trap_csrs.stval
        );

        let trap = if scause_is_interrupt {
            AxvisorTrap::Interrupt(Interrupt::from(scause_code))
        } else {
            AxvisorTrap::Exception(Exception::from_number(scause_code).ok_or_else(|| {
                error!("Unknown trap cause: scause={scause_bits:#x}");
                AxError::from(AxErrorKind::InvalidData)
            })?)
        };

        match trap {
            AxvisorTrap::Exception(
                Exception::VirtualSupervisorEnvCall | Exception::UserEnvCall,
            ) => {
                // On the Linux-host bring-up path under QEMU, guest Linux SBI
                // ecalls can surface as `UserEnvCall` even though the guest is
                // running at a kernel VA and should conceptually be issuing a
                // VS-mode ecall. Treat both trap codes as the guest SBI entry
                // path so bring-up can proceed while the exact CSR/trap
                // semantics are still being validated.
                let a = self.regs.guest_regs.gprs.a_regs();
                let param = [a[0], a[1], a[2], a[3], a[4], a[5]];
                let extension_id = a[7];
                let function_id = a[6];

                trace!(
                    "sbi_call: eid={:#x} fid={:#x} a0={:#x} a1={:#x} a2={:#x}",
                    extension_id,
                    function_id,
                    param[0],
                    param[1],
                    param[2]
                );
                match extension_id {
                    // Compatibility with Legacy Extensions.
                    legacy::LEGACY_SET_TIMER..=legacy::LEGACY_SHUTDOWN => match extension_id {
                        legacy::LEGACY_SET_TIMER => {
                            // info!("set timer: {}", param[0]);
                            self.sbi.pmu.record_set_timer();
                            self.program_guest_timer(param[0]);

                            self.set_gpr_from_gpr_index(GprIndex::A0, 0);
                        }
                        legacy::LEGACY_CONSOLE_PUTCHAR => {
                            print_byte((param[0] & 0xff) as u8);
                            self.set_gpr_from_gpr_index(GprIndex::A0, 0);
                        }
                        legacy::LEGACY_CONSOLE_GETCHAR => {
                            let mut buf = [0u8; 1];
                            let c = if axvisor_api::console::read_bytes(&mut buf) == 1 {
                                buf[0] as usize
                            } else {
                                usize::MAX
                            };
                            self.set_gpr_from_gpr_index(GprIndex::A0, c);
                        }
                        legacy::LEGACY_SHUTDOWN => {
                            return Ok(AxVCpuExitReason::SystemDown);
                        }
                        _ => {
                            warn!(
                                "Unsupported SBI legacy extension id {extension_id:#x} function \
                                 id {function_id:#x}"
                            );
                        }
                    },
                    EID_TIME => match function_id {
                        FID_SET_TIMER => {
                            self.sbi.pmu.record_set_timer();
                            self.program_guest_timer(param[0]);
                            self.sbi_return(RET_SUCCESS, 0);
                            self.log_nothing_exit("sbi_time_set_timer", scause_bits);
                            return Ok(AxVCpuExitReason::Nothing);
                        }
                        _ => {
                            self.sbi_return(RET_ERR_NOT_SUPPORTED, 0);
                            self.log_nothing_exit("sbi_time_unsupported", scause_bits);
                            return Ok(AxVCpuExitReason::Nothing);
                        }
                    },
                    x if x == sbi_spec::base::EID_BASE => {
                        use sbi_spec::base;

                        let value = match function_id {
                            base::GET_SBI_SPEC_VERSION => {
                                // SBI v2.0 encoded as major<<24 | minor.
                                (2usize << 24) | 0
                            }
                            base::GET_SBI_IMPL_ID => base::impl_id::RUST_SBI,
                            base::GET_SBI_IMPL_VERSION => 0,
                            base::PROBE_EXTENSION => match param[0] {
                                sbi_spec::time::EID_TIME
                                | hsm::EID_HSM
                                | srst::EID_SRST
                                | sbi_spec::base::EID_BASE => 1,
                                // Linux-host bring-up keeps every extension
                                // that may carry guest-provided buffers,
                                // shared-memory descriptors, hart masks, or
                                // other host-sensitive pointers unavailable
                                // until it grows an explicit guest-memory shim.
                                nacl::EID_NACL
                                | sta::EID_STA
                                | susp::EID_SUSP
                                | pmu::EID_PMU
                                | rfnc::EID_RFNC => base::UNAVAILABLE_EXTENSION,
                                EID_DBCN => 1,
                                _ => base::UNAVAILABLE_EXTENSION,
                            },
                            base::GET_MVENDORID | base::GET_MARCHID | base::GET_MIMPID => 0,
                            _ => {
                                self.sbi_return(RET_ERR_NOT_SUPPORTED, 0);
                                self.log_nothing_exit("sbi_base_unsupported_fid", scause_bits);
                                return Ok(AxVCpuExitReason::Nothing);
                            }
                        };
                        self.sbi_return(RET_SUCCESS, value);
                        self.log_nothing_exit("sbi_base", scause_bits);
                        return Ok(AxVCpuExitReason::Nothing);
                    }
                    // Handle HSM extension
                    hsm::EID_HSM => match function_id {
                        hsm::HART_START => {
                            let hartid = a[0];
                            let start_addr = a[1];
                            let opaque = a[2];
                            self.advance_pc(4);
                            return Ok(AxVCpuExitReason::CpuUp {
                                target_cpu: hartid as _,
                                entry_point: GuestPhysAddr::from(start_addr),
                                arg: opaque as _,
                            });
                        }
                        hsm::HART_STOP => {
                            return Ok(AxVCpuExitReason::CpuDown { _state: 0 });
                        }
                        hsm::HART_GET_STATUS => {
                            let hartid = a[0] as usize;
                            let status = if hartid == self.hart_id {
                                hsm::hart_state::STARTED
                            } else {
                                // On the current Linux-host path we only expose
                                // harts that have explicitly been brought up via
                                // AxVisor's vCPU task lifecycle. Linux probes
                                // HSM status very early during boot, so report
                                // non-self harts as stopped until they are
                                // started through HART_START.
                                hsm::hart_state::STOPPED
                            };
                            self.sbi_return(RET_SUCCESS, status);
                            self.log_nothing_exit("sbi_hsm_get_status", scause_bits);
                            return Ok(AxVCpuExitReason::Nothing);
                        }
                        hsm::HART_SUSPEND => {
                            // Todo: support these parameters.
                            let _suspend_type = a[0];
                            let _resume_addr = a[1];
                            let _opaque = a[2];
                            return Ok(AxVCpuExitReason::Halt);
                        }
                        _ => {
                            self.sbi_return(RET_ERR_NOT_SUPPORTED, 0);
                            self.log_nothing_exit("sbi_hsm_unsupported", scause_bits);
                            return Ok(AxVCpuExitReason::Nothing);
                        }
                    },
                    // Handle hypercall
                    EID_HVC => {
                        self.advance_pc(4);
                        return Ok(AxVCpuExitReason::Hypercall {
                            nr: function_id as _,
                            args: [
                                param[0] as _,
                                param[1] as _,
                                param[2] as _,
                                param[3] as _,
                                param[4] as _,
                                param[5] as _,
                            ],
                        });
                    }
                    // Debug Console Extension
                    EID_DBCN => {
                        match function_id {
                            FID_CONSOLE_WRITE_BYTE => {
                                print_byte((param[0] & 0xff) as u8);
                                self.sbi_return(RET_SUCCESS, 0);
                                self.log_nothing_exit("sbi_dbcn_write_byte", scause_bits);
                            }
                            FID_CONSOLE_WRITE => {
                                let len = param[0];
                                let gpa = GuestPhysAddr::from(join_u64(param[1], param[2]) as usize);
                                dbcn_trace_params("write", len, gpa, &param);
                                if len > DBCN_MAX_TRANSFER_LEN {
                                    self.sbi_return(RET_ERR_INVALID_PARAM, 0);
                                    self.log_nothing_exit("sbi_dbcn_write_bad_len", scause_bits);
                                    return Ok(AxVCpuExitReason::Nothing);
                                }
                                let mut buf = vec![0u8; len];
                                let copied = guest_mem::copy_from_guest(&mut buf, gpa);
                                if copied != len {
                                    self.sbi_return(RET_ERR_FAILED, copied);
                                    self.log_nothing_exit("sbi_dbcn_write_copy_fault", scause_bits);
                                } else {
                                    let trace_idx = DBCN_TRACE_COUNTER.fetch_add(1, Ordering::Relaxed);
                                    if dbcn_should_trace(trace_idx, &buf) {
                                        let preview = dbcn_preview(&buf, 96);
                                        let summary = format!(
                                            "dbcn_write[{trace_idx}]: len={} gpa={:#x} preview=\"{}\"",
                                            len,
                                            usize::from(gpa),
                                            preview
                                        );
                                        info!("{summary}");
                                        axvisor_api::host::emerg_write_bytes(summary.as_bytes());
                                        axvisor_api::host::emerg_write_bytes(b"\n");
                                    }
                                    if trace_idx < 64 {
                                        let preview = dbcn_preview(&buf, 96);
                                        let summary = format!(
                                            "dbcn_write_raw[{trace_idx}]: copied={} len={} gpa={:#x} preview=\"{}\"",
                                            copied,
                                            len,
                                            usize::from(gpa),
                                            preview
                                        );
                                        info!("{summary}");
                                        axvisor_api::host::emerg_write_bytes(summary.as_bytes());
                                        axvisor_api::host::emerg_write_bytes(b"\n");
                                    }
                                    let ret = console_write(&buf);
                                    self.sbi_return(ret.error, ret.value);
                                    self.log_nothing_exit("sbi_dbcn_write", scause_bits);
                                }
                            }
                            FID_CONSOLE_READ => {
                                let len = param[0];
                                let gpa = GuestPhysAddr::from(join_u64(param[1], param[2]) as usize);
                                dbcn_trace_params("read", len, gpa, &param);
                                if len > DBCN_MAX_TRANSFER_LEN {
                                    self.sbi_return(RET_ERR_INVALID_PARAM, 0);
                                    self.log_nothing_exit("sbi_dbcn_read_bad_len", scause_bits);
                                    return Ok(AxVCpuExitReason::Nothing);
                                }
                                let mut buf = vec![0u8; len];
                                let ret = console_read(&mut buf);
                                let read_len = core::cmp::min(ret.value, len);
                                let copied = guest_mem::copy_to_guest(&buf[..read_len], gpa);
                                if copied != read_len {
                                    self.sbi_return(RET_ERR_FAILED, copied);
                                    self.log_nothing_exit("sbi_dbcn_read_copy_fault", scause_bits);
                                } else {
                                    self.sbi_return(ret.error, copied);
                                    self.log_nothing_exit("sbi_dbcn_read", scause_bits);
                                }
                            }
                            _ => {
                                self.sbi_return(RET_ERR_NOT_SUPPORTED, 0);
                                self.log_nothing_exit("sbi_dbcn_unsupported_fid", scause_bits);
                            }
                        }
                        return Ok(AxVCpuExitReason::Nothing);
                    }
                    susp::EID_SUSP | nacl::EID_NACL | sta::EID_STA => {
                        // These extensions carry guest resume addresses or
                        // shared-memory pointers. Generic forwarding is not
                        // safe on Linux-host until each one translates guest
                        // addresses before touching host memory or host SBI.
                        let _ = function_id;
                        let _ = param;
                        self.sbi_return(RET_ERR_NOT_SUPPORTED, 0);
                        self.log_nothing_exit("sbi_pointer_ext_unsupported", scause_bits);
                        return Ok(AxVCpuExitReason::Nothing);
                    }
                    srst::EID_SRST => match function_id {
                        srst::SYSTEM_RESET => {
                            let reset_type = param[0];
                            if reset_type == srst::RESET_TYPE_SHUTDOWN as _ {
                                // Shutdown the system.
                                return Ok(AxVCpuExitReason::SystemDown);
                            } else {
                                error!(
                                    "Unsupported SBI SRST reset_type={:#x} fid={:#x}",
                                    reset_type, function_id
                                );
                                self.sbi_return(RET_ERR_NOT_SUPPORTED, 0);
                                self.log_nothing_exit("sbi_srst_bad_reset_type", scause_bits);
                                return Ok(AxVCpuExitReason::Nothing);
                            }
                        }
                        _ => {
                            self.sbi_return(RET_ERR_NOT_SUPPORTED, 0);
                            self.log_nothing_exit("sbi_srst_unsupported", scause_bits);
                            return Ok(AxVCpuExitReason::Nothing);
                        }
                    },
                    pmu::EID_PMU => {
                        // Linux probes PMU early, but the optional snapshot /
                        // shared-memory portions of the SBI PMU ABI are not
                        // yet safe on the Linux-host path. Keep PMU fully
                        // unavailable during bring-up instead of partially
                        // forwarding into host SBI.
                        let _ = function_id;
                        let _ = param;
                        self.sbi_return(RET_ERR_NOT_SUPPORTED, 0);
                        self.log_nothing_exit("sbi_pmu_unsupported", scause_bits);
                        return Ok(AxVCpuExitReason::Nothing);
                    }
                    rfnc::EID_RFNC => {
                        // RFENCE arguments include hart masks and base
                        // addresses that are forwarded to the host SBI by the
                        // generic RustSBI path. Do not expose it on Linux-host
                        // until there is a guest-to-host translation layer.
                        let _ = function_id;
                        let _ = param;
                        self.sbi_return(RET_ERR_NOT_SUPPORTED, 0);
                        self.log_nothing_exit("sbi_rfnc_unsupported", scause_bits);
                        return Ok(AxVCpuExitReason::Nothing);
                    }
                    // Linux-host bring-up cannot safely assume that unknown SBI
                    // extensions are register-only. Several standard and vendor
                    // extensions carry guest physical pointers or shared-memory
                    // descriptors, and RustSBI's generic forwarding path may
                    // treat those values as host pointers. Keep the forwarding
                    // surface explicit here and reject everything else until it
                    // gets a Linux-host-specific guest-memory translation shim.
                    _ => {
                        self.sbi_return(RET_ERR_NOT_SUPPORTED, 0);
                        self.log_nothing_exit("sbi_unknown_unsupported", scause_bits);
                        return Ok(AxVCpuExitReason::Nothing);
                    }
                };

                self.advance_pc(4);
                self.log_nothing_exit("sbi_legacy_or_forwarded", scause_bits);
                Ok(AxVCpuExitReason::Nothing)
            }
            AxvisorTrap::Exception(Exception::VirtualInstruction) => self.handle_virtual_instruction(),
            AxvisorTrap::Interrupt(Interrupt::SupervisorTimer) => {
                // Forward the elapsed timer to VS and stop taking the same HS
                // timer interrupt repeatedly until software programs a new one.
                unsafe {
                    hvip::set_vstip();
                    sie::clear_stimer();
                }

                self.log_nothing_exit("supervisor_timer", scause_bits);
                Ok(AxVCpuExitReason::Nothing)
            }
            AxvisorTrap::Interrupt(Interrupt::SupervisorSoft) => {
                unsafe {
                    // Treat HS-level software interrupts on the Linux-host
                    // path as a guest kick / virtual SSIP delivery point.
                    // We only need to consume the currently pending VS soft
                    // interrupt here so the same interrupt does not trap on
                    // every re-entry.
                    hvip::clear_vssip();
                    sie::clear_ssoft();
                }

                self.log_nothing_exit("supervisor_soft", scause_bits);
                Ok(AxVCpuExitReason::Nothing)
            }
            AxvisorTrap::Interrupt(Interrupt::SupervisorExternal) => {
                // 9 == Interrupt::SupervisorExternal
                //
                // It's a great fault in the `riscv` crate that `Interrupt` and `Exception` are not
                // explicitly numbered, and they provide no way to convert them to a number. Also,
                // `as usize` will give use a wrong value.
                Ok(AxVCpuExitReason::ExternalInterrupt { vector: S_EXT as _ })
            }
            AxvisorTrap::Exception(Exception::InstructionGuestPageFault) => {
                let detail = format!(
                    "riscv_vcpu::instruction_gpf sepc={:#x} stval={:#x} htval={:#x} htinst={:#x} hgatp={:#x} a0={:#x} a1={:#x}",
                    self.regs.guest_regs.sepc,
                    self.regs.trap_csrs.stval,
                    self.regs.trap_csrs.htval,
                    self.regs.trap_csrs.htinst,
                    self.regs.virtual_hs_csrs.hgatp,
                    self.regs.guest_regs.gprs.reg(GprIndex::A0),
                    self.regs.guest_regs.gprs.reg(GprIndex::A1),
                );
                error!("{detail}");
                axvisor_api::host::emerg_write_bytes(detail.as_bytes());
                Ok(AxVCpuExitReason::NestedPageFault {
                    addr: self.regs.trap_csrs.gpt_page_fault_addr(),
                    access_flags: MappingFlags::EXECUTE,
                })
            }
            AxvisorTrap::Exception(
                gpf @ (Exception::LoadGuestPageFault | Exception::StoreGuestPageFault),
            ) => self.handle_guest_page_fault(gpf == Exception::StoreGuestPageFault),
            AxvisorTrap::Exception(
                e @ (Exception::InstructionPageFault
                | Exception::LoadPageFault
                | Exception::StorePageFault
                | Exception::InstructionFault
                | Exception::LoadFault
                | Exception::StoreFault
                | Exception::InstructionMisaligned
                | Exception::LoadMisaligned
                | Exception::StoreMisaligned),
            ) => self.reflect_guest_sync_exception(e),
            _ => {
                log_unsupported_trap_summary(self, "unhandled_trap", scause_bits);
                error!("Unhandled trap on linux-host path");
                Err(ax_errno::ax_err_type!(
                    Unsupported,
                    "unhandled riscv trap on linux-host path"
                ))
            }
        }
    }

    #[inline]
    fn sbi_return(&mut self, a0: usize, a1: usize) {
        self.set_gpr_from_gpr_index(GprIndex::A0, a0);
        self.set_gpr_from_gpr_index(GprIndex::A1, a1);
        self.advance_pc(4);
    }

    #[cfg(feature = "sstc")]
    fn handle_virtual_instruction(&mut self) -> AxResult<AxVCpuExitReason> {
        let instr = {
            let instr = self.regs.trap_csrs.stval as u32;
            if instr & 0x7f == SYSTEM_OPCODE {
                instr
            } else {
                let guest_pc = GuestVirtAddr::from(self.regs.guest_regs.sepc);
                match guest_mem::fetch_guest_instruction(guest_pc) {
                    Ok(instr) => instr,
                    Err(fault) => return self.handle_guest_instruction_fetch_fault(fault),
                }
            }
        };
        let csr = ((instr >> 20) & 0xfff) as u16;

        if csr != CSR_STIMECMP {
            self.sbi.pmu.record_illegal_insn();
            log_unsupported_trap_summary(self, "virtual_instruction_bad_csr", scause::read().bits());
            error!(
                "Unhandled virtual instruction csr={:#x} sepc={:#x} stval={:#x} htval={:#x} htinst={:#x}",
                csr,
                self.regs.guest_regs.sepc,
                self.regs.trap_csrs.stval,
                self.regs.trap_csrs.htval,
                self.regs.trap_csrs.htinst,
            );
            return Err(ax_errno::ax_err_type!(
                Unsupported,
                "Unhandled virtual instruction csr"
            ));
        }

        let funct3 = ((instr >> 12) & 0x7) as u8;
        let rd = ((instr >> 7) & 0x1f) as u8;
        let rs1 = ((instr >> 15) & 0x1f) as u8;
        let old_value = self.regs.vs_csrs.vstimecmp;
        let rs1_value = self.read_gpr_raw(rs1);
        let zimm = rs1 as usize;

        let new_value = match funct3 {
            0b001 => Some(rs1_value),
            0b010 => {
                if rs1 == 0 {
                    None
                } else {
                    Some(old_value | rs1_value)
                }
            }
            0b011 => {
                if rs1 == 0 {
                    None
                } else {
                    Some(old_value & !rs1_value)
                }
            }
            0b101 => Some(zimm),
            0b110 => {
                if zimm == 0 {
                    None
                } else {
                    Some(old_value | zimm)
                }
            }
            0b111 => {
                if zimm == 0 {
                    None
                } else {
                    Some(old_value & !zimm)
                }
            }
            _ => {
                self.sbi.pmu.record_illegal_insn();
                log_unsupported_trap_summary(
                    self,
                    "virtual_instruction_bad_funct3",
                    scause::read().bits(),
                );
                error!(
                    "Unhandled virtual instruction funct3={funct3:#x} csr={csr:#x} sepc={:#x}",
                    self.regs.guest_regs.sepc,
                );
                return Err(ax_errno::ax_err_type!(
                    Unsupported,
                    "Unhandled virtual instruction"
                ));
            }
        };

        if rd != 0 {
            self.write_gpr_raw(rd, old_value);
        }

        if let Some(new_value) = new_value {
            // Linux is using the advertised `sstc` path (`csrw stimecmp,...`).
            // We currently emulate that CSR access rather than exposing direct
            // hardware STCE, so this path must also program the underlying HS
            // timer instead of only updating saved VS state.
            self.program_guest_timer(new_value);
        }

        self.advance_pc(4);
        self.log_nothing_exit("virtual_instruction_sstc", scause::read().bits());
        Ok(AxVCpuExitReason::Nothing)
    }

    #[cfg(not(feature = "sstc"))]
    fn handle_virtual_instruction(&mut self) -> AxResult<AxVCpuExitReason> {
        self.sbi.pmu.record_illegal_insn();
        log_unsupported_trap_summary(
            self,
            "virtual_instruction_without_sstc",
            scause::read().bits(),
        );
        error!(
            "Unhandled virtual instruction without sstc sepc={:#x} stval={:#x} htval={:#x} htinst={:#x}",
            self.regs.guest_regs.sepc,
            self.regs.trap_csrs.stval,
            self.regs.trap_csrs.htval,
            self.regs.trap_csrs.htinst,
        );
        Err(ax_errno::ax_err_type!(
            Unsupported,
            "Unhandled virtual instruction without sstc"
        ))
    }

    #[cfg(feature = "sstc")]
    fn read_gpr_raw(&self, index: u8) -> usize {
        GprIndex::from_raw(index as u32)
            .map(|gpr| self.get_gpr(gpr))
            .unwrap_or(0)
    }

    #[cfg(feature = "sstc")]
    fn write_gpr_raw(&mut self, index: u8, value: usize) {
        if let Some(gpr) = GprIndex::from_raw(index as u32) {
            self.set_gpr_from_gpr_index(gpr, value);
        }
    }

    /// Handle a guest page fault. Return an exit reason.
    fn handle_guest_page_fault(&mut self, _writing: bool) -> AxResult<AxVCpuExitReason> {
        let fault_addr = self.regs.trap_csrs.gpt_page_fault_addr();
        let sepc = self.regs.guest_regs.sepc;
        let sepc_vaddr = GuestVirtAddr::from(sepc);

        /// Temporary enum to represent the decoded operation.
        enum DecodedOp {
            Read {
                i: IType,
                width: AccessWidth,
                signed_ext: bool,
            },
            Write {
                s: SType,
                width: AccessWidth,
            },
        }

        use DecodedOp::*;

        // Keep the Linux-host page-fault decode path flat: fetch and decode in
        // place so guest fetch faults can return their exit reason directly
        // instead of being wrapped into another aggregate result.
        let (decoded_instr, instr_len) = {
            // The htinst CSR contains "transformed instruction" that caused
            // the page fault. We can use it but we use the sepc to fetch the
            // original instruction instead for now.
            let mut instr = riscv_h::register::htinst::read();
            let instr_len;
            if instr == 0 {
                instr = match guest_mem::fetch_guest_instruction(sepc_vaddr) {
                    Ok(instr) => instr as _,
                    Err(fault) => return self.handle_guest_instruction_fetch_fault(fault),
                };
                instr_len = riscv_decode::instruction_length(instr as u16);
                instr = match instr_len {
                    2 => instr & 0xffff,
                    4 => instr,
                    _ => {
                        error!("guest page fault decode: unsupported fetched instruction length");
                        return Err(ax_errno::ax_err_type!(
                            Unsupported,
                            "unsupported guest instruction length from fetch"
                        ));
                    }
                };
            } else if instr_is_pseudo(instr as u32) {
                error!("fault on 1st stage page table walk");
                return Err(ax_errno::ax_err_type!(
                    Unsupported,
                    "guest page fault handler encountered pseudo instruction"
                ));
            } else {
                // Transform htinst value to standard instruction.
                // According to RISC-V Spec:
                //      Bits 1:0 of a transformed standard instruction will be
                //      binary 01 if the trapping instruction is compressed and
                //      11 if not.
                instr_len = match (instr as u16) & 0x3 {
                    0x1 => 2,
                    0x3 => 4,
                    _ => {
                        error!("guest page fault decode: unsupported transformed htinst length");
                        return Err(ax_errno::ax_err_type!(
                            Unsupported,
                            "unsupported transformed instruction length"
                        ));
                    }
                };
                instr |= 0x2;
            }

            (
                riscv_decode::decode(instr as u32).map_err(|_| {
                    ax_errno::ax_err_type!(
                        Unsupported,
                        "risc-v vcpu guest pf handler decoding instruction failed"
                    )
                })?,
                instr_len,
            )
        };
        let op = match decoded_instr {
            Instruction::Lb(i) => Read {
                i,
                width: AccessWidth::Byte,
                signed_ext: true,
            },
            Instruction::Lh(i) => Read {
                i,
                width: AccessWidth::Word,
                signed_ext: true,
            },
            Instruction::Lw(i) => Read {
                i,
                width: AccessWidth::Dword,
                signed_ext: true,
            },
            Instruction::Ld(i) => Read {
                i,
                width: AccessWidth::Qword,
                signed_ext: true,
            },
            Instruction::Lbu(i) => Read {
                i,
                width: AccessWidth::Byte,
                signed_ext: false,
            },
            Instruction::Lhu(i) => Read {
                i,
                width: AccessWidth::Word,
                signed_ext: false,
            },
            Instruction::Lwu(i) => Read {
                i,
                width: AccessWidth::Dword,
                signed_ext: false,
            },
            Instruction::Sb(s) => Write {
                s,
                width: AccessWidth::Byte,
            },
            Instruction::Sh(s) => Write {
                s,
                width: AccessWidth::Word,
            },
            Instruction::Sw(s) => Write {
                s,
                width: AccessWidth::Dword,
            },
            Instruction::Sd(s) => Write {
                s,
                width: AccessWidth::Qword,
            },
            _ => {
                // Not a load or store instruction, so we cannot handle it here, return a nested page fault.
                return Ok(AxVCpuExitReason::NestedPageFault {
                    addr: fault_addr,
                    access_flags: MappingFlags::empty(),
                });
            }
        };

        match &op {
            Read {
                i,
                width,
                signed_ext,
            } => {
                info!(
                    "riscv_vcpu::guest_gpf_mmio_candidate kind=read fault_gpa={:#x} width={:?} rd={} signed_ext={} sepc={:#x} instr_len={} stval={:#x} htval={:#x} htinst={:#x}",
                    fault_addr.as_usize(),
                    width,
                    i.rd(),
                    signed_ext,
                    sepc,
                    instr_len,
                    self.regs.trap_csrs.stval,
                    self.regs.trap_csrs.htval,
                    self.regs.trap_csrs.htinst,
                );
            }
            Write { s, width } => {
                info!(
                    "riscv_vcpu::guest_gpf_mmio_candidate kind=write fault_gpa={:#x} width={:?} rs2={} sepc={:#x} instr_len={} stval={:#x} htval={:#x} htinst={:#x}",
                    fault_addr.as_usize(),
                    width,
                    s.rs2(),
                    sepc,
                    instr_len,
                    self.regs.trap_csrs.stval,
                    self.regs.trap_csrs.htval,
                    self.regs.trap_csrs.htinst,
                );
            }
        }

        // WARN: This is a temporary place to add the instruction length to the guest's sepc.
        self.advance_pc(instr_len);

        Ok(match op {
            Read {
                i,
                width,
                signed_ext,
            } => {
                self.sbi.pmu.record_access_load();
                AxVCpuExitReason::MmioRead {
                    addr: fault_addr,
                    width,
                    reg: i.rd() as _,
                    reg_width: AccessWidth::Qword,
                    signed_ext,
                }
            }
            Write { s, width } => {
                self.sbi.pmu.record_access_store();
                let source_reg = s.rs2();
                let value = self.get_gpr(unsafe {
                    // SAFETY: `source_reg` is guaranteed to be in [0, 31]
                    GprIndex::from_raw(source_reg).unwrap_unchecked()
                });

                AxVCpuExitReason::MmioWrite {
                    addr: fault_addr,
                    width,
                    data: value as _,
                }
            }
        })
    }
}
