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

use alloc::{boxed::Box, format, sync::Arc, vec::Vec};
use core::{alloc::Layout, fmt};

use ax_cpumask::CpuMask;
use ax_errno::{AxError, AxResult, ax_err, ax_err_type};
use ax_kspin::SpinNoIrq as Mutex;
use ax_memory_addr::{align_down_4k, align_up_4k};
use axaddrspace::{
    AddrSpace, GuestPhysAddr, GuestVirtAddr, HostPhysAddr, HostVirtAddr, MappingFlags,
    device::AccessWidth,
};
use axdevice::{AxVmDeviceConfig, AxVmDevices};
#[cfg(target_arch = "x86_64")]
use axdevice_base::BaseDeviceOps;
use axvcpu::{AxVCpu, AxVCpuExitReason, InterruptTriggerMode};
use axvisor_api::vmm::InterruptVector;
use spin::Once;
#[cfg(all(target_arch = "x86_64", feature = "vmx"))]
use x86_vcpu::{X86_APIC_ACCESS_GPA, x86_apic_access_page_addr};
#[cfg(target_arch = "x86_64")]
use x86_vcpu::{X86ArchVCpu, X86VCpuSetupConfig};
#[cfg(target_arch = "x86_64")]
use x86_vcpu::GuestPageWalkInfo;

#[cfg(any(axvisor_host_riscv64, axvisor_host_x86_64))]
unsafe extern "Rust" {
    #[cfg(axvisor_host_riscv64)]
    fn axvisor_linux_bridge_register_passthrough_device(
        vm_id: usize,
        base_hpa: usize,
        length: usize,
        irq_id: usize,
    ) -> bool;
    fn axvisor_linux_bridge_register_guest_ram(base_hpa: usize, length: usize) -> bool;
}

unsafe extern "C" {
    /*
     * Lazy on-demand guest RAM fault-in. Mirrors KVM's kvm_faultin_pfn:
     * a stage-2 (EPT) fault to a GPA inside a registered memslot must be
     * resolved by pinning+mapping the backing page (RAM), NOT emulated as
     * MMIO. Returns 0 if the page was faulted in (re-enter guest),
     * -ENOENT if the GPA is not covered by any memslot (caller should fall
     * through to MMIO decode, matching KVM's !slot -> noslot_fault path),
     * or another negative errno on a genuine pin/map error (abort the run).
     * backend_vm=0 lets the C side resolve the VM via the running vCPU.
     * This is the C symbol exported by axvisor_kvm_main.c
     * (EXPORT_SYMBOL_GPL), so it must be declared with the C ABI.
     */
    fn axvisor_kvm_x86_bridge_fault_in_gpa(backend_vm: u64, gpa: u64, write: u32) -> i32;
}

#[cfg(not(target_arch = "x86_64"))]
use crate::vcpu::AxVCpuCreateConfig;
#[cfg(target_arch = "aarch64")]
use crate::vcpu::get_sysreg_device;
use crate::{
    config::{AxVMConfig, PhysCpuList, VMInterruptMode},
    hal::PagingHandlerImpl,
    has_hardware_support,
    vcpu::AxArchVCpuImpl,
};

const VM_ASPACE_BASE: usize = 0x0;
const VM_ASPACE_SIZE: usize = 0x7fff_ffff_f000;

/// A vCPU with architecture-independent interface.
type VCpu = AxVCpu<AxArchVCpuImpl>;
/// A reference to a vCPU.
pub type AxVCpuRef = Arc<VCpu>;
/// A reference to a VM.
pub type AxVMRef = Arc<AxVM>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VMMemoryRegionBacking {
    PagedAlloc,
    Reserved,
}

fn width_mask(width: AccessWidth) -> usize {
    match width {
        AccessWidth::Byte => 0xff,
        AccessWidth::Word => 0xffff,
        AccessWidth::Dword => 0xffff_ffff,
        AccessWidth::Qword => usize::MAX,
    }
}

fn sign_extend_value(value: usize, width: AccessWidth) -> usize {
    match width {
        AccessWidth::Byte => (value as i8) as isize as usize,
        AccessWidth::Word => (value as i16) as isize as usize,
        AccessWidth::Dword => (value as i32) as isize as usize,
        AccessWidth::Qword => value,
    }
}

fn host_emerg_line(msg: &str) {
    axvisor_api::host::emerg_write_bytes(msg.as_bytes());
    axvisor_api::host::emerg_write_bytes(b"\n");
}

#[cfg(target_arch = "x86_64")]
fn x86_inject_ioapic_irq(
    vcpu: &AxVCpuRef,
    vector: u8,
    level_triggered: bool,
) {
    if let Err(err) = vcpu.inject_interrupt_with_trigger(
        vector as _,
        if level_triggered {
            InterruptTriggerMode::LevelTriggered
        } else {
            InterruptTriggerMode::EdgeTriggered
        },
    ) {
        warn!(
            "x86 inject ioapic irq failed vector={:#x} level_triggered={} err={err:?}",
            vector, level_triggered
        );
    }
}

#[cfg(target_arch = "x86_64")]
fn x86_inject_due_pit_irq0(vm: &AxVM, vcpu: &AxVCpuRef) {
    const PIT_TIMER_GSI: usize = 0;

    if vm.interrupt_mode() != VMInterruptMode::Passthrough {
        return;
    }

    let now_ns = axvisor_api::time::current_time_nanos();
    if !vm.get_devices().x86_pit_consume_irq0_if_due(now_ns) {
        return;
    }

    let Some(irq) = vm.get_devices().x86_ioapic_assert_gsi(PIT_TIMER_GSI) else {
        trace!("x86 PIT IRQ0 due but vIOAPIC GSI0 is not ready");
        return;
    };

    x86_inject_ioapic_irq(vcpu, irq.vector, irq.level_triggered);
}

#[cfg(target_arch = "x86_64")]
fn x86_inject_pending_serial_irq(vm: &AxVM, vcpu: &AxVCpuRef) {
    const COM1_GSI: usize = 4;

    if vm.interrupt_mode() != VMInterruptMode::Passthrough {
        return;
    }

    if !vm.get_devices().x86_serial_poll_irq() {
        return;
    }

    let Some(irq) = vm.get_devices().x86_ioapic_assert_gsi(COM1_GSI) else {
        trace!("x86 COM1 RX pending but vIOAPIC GSI4 is not ready");
        return;
    };

    x86_inject_ioapic_irq(vcpu, irq.vector, irq.level_triggered);
}

#[cfg(target_arch = "x86_64")]
fn x86_vcpu_setup_config(config: &AxVMConfig) -> X86VCpuSetupConfig {
    let mut setup_config = X86VCpuSetupConfig::default();
    setup_config.enable_eoi_exits = matches!(
        config.interrupt_mode(),
        VMInterruptMode::Emulated | VMInterruptMode::Passthrough
    );

    for dev in config.emu_devices() {
        match dev.emu_type {
            axvmconfig::EmulatedDeviceType::Console => {
                setup_config.add_pio_intercept_range(0x3f8, 8);
            }
            axvmconfig::EmulatedDeviceType::X86Pit => {
                setup_config.add_pio_intercept_range(0x40, 4);
                setup_config.add_pio_intercept_range(0x61, 1);
            }
            axvmconfig::EmulatedDeviceType::X86Pic => {
                setup_config.add_pio_intercept_range(0x20, 2);
                setup_config.add_pio_intercept_range(0xa0, 2);
                setup_config.add_pio_intercept_range(0x4d0, 2);
            }
            _ => {}
        }
    }

    for port in config.pass_through_ports() {
        if !setup_config.add_pio_intercept_range(port.base, port.length) {
            warn!(
                "x86 vcpu setup failed to add passthrough pio intercept base={:#x} len={:#x}",
                port.base, port.length
            );
        }
    }
    info!(
        "x86 vcpu setup pio_intercept_range_count={}",
        setup_config.pio_intercept_range_count
    );
    setup_config
}

#[cfg(target_arch = "x86_64")]
fn decode_x86_mov_mmio(
    instr: &[u8],
    fault_gpa: GuestPhysAddr,
    access_flags: MappingFlags,
    arch_vcpu: &mut X86ArchVCpu,
) -> AxResult<Option<AxVCpuExitReason>> {
    let mut idx = 0;
    let mut rex = 0u8;
    let mut operand_size_override = false;

    while idx < instr.len() {
        match instr[idx] {
            0x66 => {
                operand_size_override = true;
                idx += 1;
            }
            0x40..=0x4f => {
                rex = instr[idx];
                idx += 1;
            }
            _ => break,
        }
    }
    if idx + 1 >= instr.len() {
        return Ok(None);
    }

    let opcode = instr[idx];
    idx += 1;
    if !matches!(opcode, 0x88 | 0x89 | 0x8a | 0x8b) {
        return Ok(None);
    }

    let instr_len = x86_modrm_instruction_len(instr, idx)?;
    let modrm = instr[idx];
    let mode = modrm >> 6;
    let reg = ((modrm >> 3) & 0x7) | ((rex & 0x4) << 1);
    if mode == 0b11 || reg == 4 {
        return Ok(None);
    }

    let width = match opcode {
        0x88 | 0x8a => AccessWidth::Byte,
        0x89 | 0x8b if rex & 0x8 != 0 => AccessWidth::Qword,
        0x89 | 0x8b if operand_size_override => AccessWidth::Word,
        _ => AccessWidth::Dword,
    };
    let is_read = matches!(opcode, 0x8a | 0x8b);
    if is_read && !access_flags.contains(MappingFlags::READ) {
        return Ok(None);
    }
    if !is_read && !access_flags.contains(MappingFlags::WRITE) {
        return Ok(None);
    }

    arch_vcpu.advance_rip(instr_len as u8)?;
    if is_read {
        Ok(Some(AxVCpuExitReason::MmioRead {
            addr: fault_gpa,
            width,
            reg: reg as usize,
            reg_width: if rex & 0x8 != 0 {
                AccessWidth::Qword
            } else {
                AccessWidth::Dword
            },
            signed_ext: false,
        }))
    } else {
        Ok(Some(AxVCpuExitReason::MmioWrite {
            addr: fault_gpa,
            width,
            data: arch_vcpu.regs().get_reg_of_index(reg) & width_mask_u64(width),
        }))
    }
}

#[cfg(target_arch = "x86_64")]
fn x86_modrm_instruction_len(instr: &[u8], modrm_idx: usize) -> AxResult<usize> {
    if modrm_idx >= instr.len() {
        return ax_err!(InvalidInput, "missing x86 ModRM byte");
    }
    let modrm = instr[modrm_idx];
    let mode = modrm >> 6;
    let rm = modrm & 0x7;
    if mode == 0b11 {
        return Ok(modrm_idx + 1);
    }

    let mut len = modrm_idx + 1;
    if rm == 4 {
        if len >= instr.len() {
            return ax_err!(InvalidInput, "missing x86 SIB byte");
        }
        let sib = instr[len];
        len += 1;
        let base = sib & 0x7;
        if mode == 0 && base == 5 {
            len += 4;
        }
    } else if mode == 0 && rm == 5 {
        len += 4;
    }

    match mode {
        0 => {}
        1 => len += 1,
        2 => len += 4,
        _ => {}
    }
    if len > instr.len() {
        return ax_err!(InvalidInput, "truncated x86 memory operand");
    }
    Ok(len)
}

#[cfg(target_arch = "x86_64")]
fn width_mask_u64(width: AccessWidth) -> u64 {
    match width {
        AccessWidth::Byte => 0xff,
        AccessWidth::Word => 0xffff,
        AccessWidth::Dword => 0xffff_ffff,
        AccessWidth::Qword => u64::MAX,
    }
}

#[cfg(target_arch = "x86_64")]
const X86_PAGE_PRESENT: u64 = 1 << 0;
#[cfg(target_arch = "x86_64")]
const X86_PAGE_SIZE: u64 = 1 << 7;
#[cfg(target_arch = "x86_64")]
const X86_PAGE_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

struct AxVMInnerConst {
    phys_cpu_ls: PhysCpuList,
    vcpu_list: Box<[AxVCpuRef]>,
    devices: AxVmDevices,
}

unsafe impl Send for AxVMInnerConst {}
unsafe impl Sync for AxVMInnerConst {}

/// Represents a memory region in a virtual machine.
#[derive(Debug, Clone)]
pub struct VMMemoryRegion {
    /// Guest physical address.
    pub gpa: GuestPhysAddr,
    /// Host virtual address.
    pub hva: HostVirtAddr,
    /// Memory layout of the region.
    pub layout: Layout,
    backing: VMMemoryRegionBacking,
}

impl VMMemoryRegion {
    /// Returns the size of the memory region.
    pub fn size(&self) -> usize {
        self.layout.size()
    }

    /// Returns the host physical address backing this guest memory region.
    pub fn host_paddr(&self) -> HostPhysAddr {
        axvisor_api::memory::virt_to_phys(self.hva)
    }

    /// Returns `true` if the guest physical address is identical to the host physical address.
    pub fn is_identical(&self) -> bool {
        self.gpa.as_usize() == self.host_paddr().as_usize()
    }

    /// Returns `true` if the region was allocated by the VM allocator.
    pub fn is_allocated(&self) -> bool {
        matches!(self.backing, VMMemoryRegionBacking::PagedAlloc)
    }
}

struct AxVMInnerMut {
    // Todo: use more efficient lock.
    address_space: AddrSpace<PagingHandlerImpl>,
    memory_regions: Vec<VMMemoryRegion>,
    config: AxVMConfig,
    vm_status: VMStatus,
}

/// VM status enumeration representing the lifecycle states of a virtual machine
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VMStatus {
    /// VM is being created/loaded
    Loading,
    /// VM is loaded but not yet started
    Loaded,
    /// VM is currently running
    Running,
    /// VM is suspended (paused but can be resumed)
    Suspended,
    /// VM is in the process of shutting down
    Stopping,
    /// VM is stopped
    Stopped,
}

impl VMStatus {
    /// Get status as a string (lowercase)
    pub fn as_str(&self) -> &'static str {
        match self {
            VMStatus::Loading => "loading",
            VMStatus::Loaded => "loaded",
            VMStatus::Running => "running",
            VMStatus::Suspended => "suspended",
            VMStatus::Stopping => "stopping",
            VMStatus::Stopped => "stopped",
        }
    }

    /// Get status with emoji icon
    pub fn as_str_with_icon(&self) -> &'static str {
        match self {
            VMStatus::Loading => "🔄 loading",
            VMStatus::Loaded => "📦 loaded",
            VMStatus::Running => "🚀 running",
            VMStatus::Suspended => "🛑 suspended",
            VMStatus::Stopping => "⏹️ stopping",
            VMStatus::Stopped => "💤 stopped",
        }
    }
}

impl fmt::Display for VMStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

const TEMP_MAX_VCPU_NUM: usize = 64;

/// A Virtual Machine.
pub struct AxVM {
    id: usize,
    inner_const: Once<AxVMInnerConst>,
    inner_mut: Mutex<AxVMInnerMut>,
}

impl AxVM {
    #[cfg(any(axvisor_host_riscv64, axvisor_host_x86_64))]
    fn register_linux_guest_ram(hpa: HostPhysAddr, size: usize) -> AxResult {
        if unsafe { axvisor_linux_bridge_register_guest_ram(hpa.as_usize(), size) } {
            Ok(())
        } else {
            Err(ax_err_type!(
                BadState,
                format!(
                    "failed to register Linux-host guest RAM [{:#x}~{:#x}]",
                    hpa.as_usize(),
                    hpa.as_usize().saturating_add(size)
                )
            ))
        }
    }

    #[cfg(not(any(axvisor_host_riscv64, axvisor_host_x86_64)))]
    fn register_linux_guest_ram(_hpa: HostPhysAddr, _size: usize) -> AxResult {
        Ok(())
    }

    /// Creates a new VM with the given configuration.
    /// Returns an error if the configuration is invalid.
    /// The VM is not started until `boot` is called.
    pub fn new(config: AxVMConfig) -> AxResult<AxVMRef> {
        let address_space = AddrSpace::new_empty(
            crate::vcpu::max_guest_page_table_levels(),
            GuestPhysAddr::from(VM_ASPACE_BASE),
            VM_ASPACE_SIZE,
        )?;

        let result = Arc::new(Self {
            id: config.id(),
            inner_const: Once::new(),
            inner_mut: Mutex::new(AxVMInnerMut {
                address_space,
                config,
                memory_regions: Vec::new(),
                vm_status: VMStatus::Loading,
            }),
        });

        info!("VM created: id={}", result.id());

        Ok(result)
    }

    /// Returns the VM id.
    #[inline]
    pub fn id(&self) -> usize {
        self.id
    }

    /// Returns the configured VM interrupt mode.
    pub fn interrupt_mode(&self) -> VMInterruptMode {
        self.inner_mut.lock().config.interrupt_mode()
    }

    /// Returns whether this VM maps passthrough devices or address ranges that
    /// can require exclusive host-resource ownership before VM start.
    pub fn has_host_fs_passthrough_conflict(&self) -> bool {
        let inner_mut = self.inner_mut.lock();
        !inner_mut.config.pass_through_devices().is_empty()
            || !inner_mut.config.pass_through_addresses().is_empty()
    }

    /// Sets up the VM before booting.
    pub fn init(&self) -> AxResult {
        info!("axvm::init enter vm_id={}", self.id());
        let mut inner_mut = self.inner_mut.lock();
        info!("axvm::init locked inner_mut vm_id={}", self.id());

        let dtb_addr = inner_mut.config.image_config().dtb_load_gpa;
        let vcpu_id_pcpu_sets = inner_mut.config.phys_cpu_ls.get_vcpu_affinities_pcpu_ids();
        info!(
            "axvm::init config ready vm_id={} dtb={:#x} vcpu_count={}",
            self.id(),
            dtb_addr.unwrap_or_default().as_usize(),
            vcpu_id_pcpu_sets.len()
        );

        info!("dtb_load_gpa: {dtb_addr:?}");
        debug!("id: {}, VCpuIdPCpuSets: {vcpu_id_pcpu_sets:#x?}", self.id());

        let mut vcpu_list = Vec::with_capacity(vcpu_id_pcpu_sets.len());
        for (vcpu_id, phys_cpu_set, _pcpu_id) in vcpu_id_pcpu_sets {
            info!(
                "axvm::init before VCpu::new vm_id={} vcpu_id={} phys_cpu_set={:?} pcpu_id={}",
                self.id(),
                vcpu_id,
                phys_cpu_set,
                _pcpu_id
            );
            #[cfg(target_arch = "aarch64")]
            let arch_config = AxVCpuCreateConfig {
                mpidr_el1: _pcpu_id as _,
                dtb_addr: dtb_addr.unwrap_or_default().as_usize(),
            };
            #[cfg(target_arch = "riscv64")]
            let arch_config = AxVCpuCreateConfig {
                hart_id: vcpu_id as _,
                dtb_addr: dtb_addr.unwrap_or_default().as_usize(),
            };
            #[cfg(target_arch = "loongarch64")]
            let arch_config = AxVCpuCreateConfig {
                cpu_id: vcpu_id,
                dtb_addr: dtb_addr.unwrap_or_default().as_usize(),
            };

            // FIXME: VCpu is neither `Send` nor `Sync` by design, check whether
            // 1. we should make it `Send` and `Sync`, or
            // 2. we can guarantee that no cross-thread access is performed
            #[allow(clippy::arc_with_non_send_sync)]
            vcpu_list.push(Arc::new(VCpu::new(
                self.id(),
                vcpu_id,
                0, // Currently not used.
                phys_cpu_set,
                #[cfg(target_arch = "aarch64")]
                arch_config,
                #[cfg(target_arch = "loongarch64")]
                arch_config,
                #[cfg(target_arch = "riscv64")]
                arch_config,
                #[cfg(target_arch = "x86_64")]
                (),
            )?));
            info!(
                "axvm::init after VCpu::new vm_id={} vcpu_id={}",
                self.id(),
                vcpu_id
            );
        }
        info!("axvm::init vcpu_list built vm_id={}", self.id());

        let mut pt_dev_region = Vec::new();
        for pt_device in inner_mut.config.pass_through_devices() {
            #[cfg(axvisor_host_riscv64)]
            if pt_device.base_hpa != 0 && pt_device.length != 0 {
                unsafe {
                    axvisor_linux_bridge_register_passthrough_device(
                        self.id(),
                        pt_device.base_hpa,
                        pt_device.length,
                        pt_device.irq_id,
                    );
                }
            }
            trace!(
                "PT dev {:?} region: [{:#x}~{:#x}] -> [{:#x}~{:#x}]",
                pt_device.name,
                pt_device.base_gpa,
                pt_device.base_gpa + pt_device.length,
                pt_device.base_hpa,
                pt_device.base_hpa + pt_device.length
            );
            // Align the base address and length to 4K boundaries.
            pt_dev_region.push((
                align_down_4k(pt_device.base_gpa),
                align_up_4k(pt_device.length),
            ));
        }

        for pt_addr in inner_mut.config.pass_through_addresses() {
            debug!(
                "PT addr region: [{:#x}~{:#x}]",
                pt_addr.base_gpa,
                pt_addr.base_gpa + pt_addr.length,
            );
            // Align the base address and length to 4K boundaries.
            pt_dev_region.push((align_down_4k(pt_addr.base_gpa), align_up_4k(pt_addr.length)));
        }

        pt_dev_region.sort_by_key(|(gpa, _)| *gpa);

        // Merge overlapping regions.
        let pt_dev_region =
            pt_dev_region
                .into_iter()
                .fold(Vec::<(usize, usize)>::new(), |mut acc, (gpa, len)| {
                    if let Some(last) = acc.last_mut() {
                        if last.0 + last.1 >= gpa {
                            // Merge with the last region.
                            last.1 = (last.0 + last.1).max(gpa + len) - last.0;
                        } else {
                            acc.push((gpa, len));
                        }
                    } else {
                        acc.push((gpa, len));
                    }
                    acc
                });

        for (gpa, len) in &pt_dev_region {
            info!(
                "axvm::init map passthrough region vm_id={} gpa={:#x} len={:#x}",
                self.id(),
                gpa,
                len
            );
            inner_mut.address_space.map_linear(
                GuestPhysAddr::from(*gpa),
                HostPhysAddr::from(*gpa),
                *len,
                MappingFlags::DEVICE
                    | MappingFlags::READ
                    | MappingFlags::WRITE
                    | MappingFlags::USER,
            )?;
        }

        #[cfg(all(target_arch = "x86_64", feature = "vmx"))]
        {
            if x86_vcpu::supports_apicv() {
                inner_mut.address_space.map_linear(
                    GuestPhysAddr::from(X86_APIC_ACCESS_GPA),
                    x86_apic_access_page_addr(),
                    ax_memory_addr::PAGE_SIZE_4K,
                    MappingFlags::DEVICE | MappingFlags::READ | MappingFlags::WRITE,
                )?;
            }
        }

        #[cfg_attr(not(target_arch = "aarch64"), expect(unused_mut))]
        let mut devices = axdevice::AxVmDevices::new(AxVmDeviceConfig {
            vm_id: self.id(),
            emu_configs: inner_mut.config.emu_devices().to_vec(),
        });
        info!("axvm::init devices created vm_id={}", self.id());

        #[cfg(target_arch = "x86_64")]
        for port in inner_mut.config.pass_through_ports() {
            let passthrough = Arc::new(crate::host::x86_port::HostPortPassthrough::new(
                port.base,
                port.length,
            )?);
            let range = passthrough.address_range();
            info!(
                "axvm::init register passthrough port vm_id={} range={:#x}",
                self.id(),
                range
            );
            devices.add_port_dev(passthrough);
        }

        #[cfg(target_arch = "aarch64")]
        {
            let passthrough =
                inner_mut.config.interrupt_mode() == axvmconfig::VMInterruptMode::Passthrough;
            if passthrough {
                let spis = inner_mut.config.pass_through_spis();
                let cpu_id = self.id() - 1; // FIXME: get the real CPU id.
                let mut gicd_found = false;

                for device in devices.iter_mmio_dev() {
                    if let Some(result) = axdevice_base::map_device_of_type(
                        device,
                        |gicd: &arm_vgic::v3::vgicd::VGicD| {
                            debug!("VGicD found, assigning SPIs...");

                            for spi in spis {
                                gicd.assign_irq(*spi + 32, cpu_id, (0, 0, 0, cpu_id as _))
                            }

                            AxResult::Ok(())
                        },
                    ) {
                        result?;
                        gicd_found = true;
                        break;
                    }
                }

                if !gicd_found {
                    warn!("Failed to assign SPIs: No VGicD found in device list");
                }
            } else {
                // non-passthrough mode, we need to set up the virtual timer.
                //
                // FIXME: maybe let `axdevice` handle this automatically?
                // how to let `axdevice` know whether the VM is in passthrough mode or not?
                for dev in get_sysreg_device() {
                    devices.add_sys_reg_dev(dev);
                }
            }
        }

        self.inner_const.call_once(|| AxVMInnerConst {
            phys_cpu_ls: inner_mut.config.phys_cpu_ls.clone(),
            vcpu_list: vcpu_list.into_boxed_slice(),
            devices,
        });
        info!("axvm::init inner_const ready vm_id={}", self.id());

        // Setup VCpus.
        for vcpu in self.vcpu_list() {
            info!(
                "axvm::init before vcpu.setup vm_id={} vcpu_id={}",
                self.id(),
                vcpu.id()
            );
            #[cfg(target_arch = "aarch64")]
            let setup_config = {
                let passthrough =
                    inner_mut.config.interrupt_mode() == axvmconfig::VMInterruptMode::Passthrough;
                crate::vcpu::AxVCpuSetupConfig {
                    passthrough_interrupt: passthrough,
                    passthrough_timer: passthrough,
                }
            };
            #[cfg(target_arch = "loongarch64")]
            let setup_config = {
                let passthrough =
                    inner_mut.config.interrupt_mode() == axvmconfig::VMInterruptMode::Passthrough;
                crate::vcpu::AxVCpuSetupConfig {
                    passthrough_interrupt: passthrough,
                    passthrough_timer: passthrough,
                }
            };
            #[cfg(not(any(
                target_arch = "aarch64",
                target_arch = "loongarch64",
                target_arch = "x86_64"
            )))]
            #[allow(clippy::let_unit_value)]
            let setup_config = <AxArchVCpuImpl as axvcpu::AxArchVCpu>::SetupConfig::default();
            #[cfg(target_arch = "x86_64")]
            let setup_config = x86_vcpu_setup_config(&inner_mut.config);

            let entry = if vcpu.id() == 0 {
                inner_mut.config.bsp_entry()
            } else {
                inner_mut.config.ap_entry()
            };

            debug!("Setting up vCPU[{}] entry at {:#x}", vcpu.id(), entry);

            vcpu.setup(
                entry,
                inner_mut.address_space.page_table_root(),
                setup_config,
            )?;
            info!(
                "axvm::init after vcpu.setup vm_id={} vcpu_id={}",
                self.id(),
                vcpu.id()
            );
        }
        info!("axvm::init leave vm_id={}", self.id());
        info!("VM setup: id={}", self.id());
        Ok(())
    }

    /// Sets the VM status.
    pub fn set_vm_status(&self, status: VMStatus) {
        let mut inner_mut = self.inner_mut.lock();
        inner_mut.vm_status = status;
    }

    /// Returns the current VM status.
    pub fn vm_status(&self) -> VMStatus {
        let inner_mut = self.inner_mut.lock();
        inner_mut.vm_status
    }

    /// Retrieves the vCPU corresponding to the given vcpu_id for the VM.
    /// Returns None if the vCPU does not exist.
    #[inline]
    pub fn vcpu(&self, vcpu_id: usize) -> Option<AxVCpuRef> {
        self.vcpu_list().get(vcpu_id).cloned()
    }

    /// Returns the number of vCPUs corresponding to the VM.
    #[inline]
    pub fn vcpu_num(&self) -> usize {
        self.inner_const().vcpu_list.len()
    }

    /// Applies x86 KVM ABI vCPU state to the architecture backend.
    #[cfg(target_arch = "x86_64")]
    pub fn apply_x86_kvm_vcpu_state(
        &self,
        vcpu_id: usize,
        state: &x86_vcpu::X86KvmVcpuState,
    ) -> AxResult {
        let vcpu = self
            .vcpu(vcpu_id)
            .ok_or_else(|| ax_err_type!(InvalidInput, "Invalid vcpu_id"))?;
        vcpu.with_arch_vcpu(|arch_vcpu| arch_vcpu.apply_kvm_state(state))
    }

    /// Complete a userspace-handled x86 port I/O read.
    #[cfg(target_arch = "x86_64")]
    pub fn complete_x86_io_read(&self, vcpu_id: usize, value: usize, width: usize) -> AxResult {
        let vcpu = self
            .vcpu(vcpu_id)
            .ok_or_else(|| ax_err_type!(InvalidInput, "Invalid vcpu_id"))?;
        vcpu.with_arch_vcpu(|arch_vcpu| {
            axvcpu::AxArchVCpu::complete_io_read(arch_vcpu, value, width);
            Ok(())
        })
    }

    /// Assert an x86 IOAPIC GSI and inject the routed interrupt into the BSP.
    #[cfg(target_arch = "x86_64")]
    pub fn inject_x86_gsi(&self, gsi: usize) -> AxResult {
        let irq = self
            .get_devices()
            .x86_ioapic_assert_gsi(gsi)
            .ok_or_else(|| ax_err_type!(NotFound, "No x86 IOAPIC route for GSI"))?;
        let vcpu = self
            .vcpu(0)
            .ok_or_else(|| ax_err_type!(InvalidInput, "BSP vCPU is not available"))?;
        vcpu.inject_interrupt_with_trigger(
            irq.vector as _,
            if irq.level_triggered {
                InterruptTriggerMode::LevelTriggered
            } else {
                InterruptTriggerMode::EdgeTriggered
            },
        )
    }

    fn inner_const(&self) -> &AxVMInnerConst {
        self.inner_const
            .get()
            .expect("VM inner_const not initialized")
    }

    /// Returns a reference to the list of vCPUs corresponding to the VM.
    #[inline]
    pub fn vcpu_list(&self) -> &[AxVCpuRef] {
        &self.inner_const().vcpu_list
    }

    /// Returns the base address of the two-stage address translation page table for the VM.
    pub fn ept_root(&self) -> HostPhysAddr {
        self.inner_mut.lock().address_space.page_table_root()
    }

    /// Rebuilds an x86 AP vCPU VMCS after SIPI selects the real-mode trampoline entry.
    #[cfg(target_arch = "x86_64")]
    pub fn setup_x86_ap_vcpu_entry(
        &self,
        vcpu: &AxVCpuRef,
        entry: GuestPhysAddr,
    ) -> AxResult {
        let inner = self.inner_mut.lock();
        let ept_root = inner.address_space.page_table_root();
        let setup_config = x86_vcpu_setup_config(&inner.config);
        drop(inner);

        vcpu.with_arch_vcpu(|arch_vcpu| {
            #[cfg(feature = "vmx")]
            {
                arch_vcpu.setup_sipi_with_config(ept_root, entry, setup_config)
            }
            #[cfg(not(feature = "vmx"))]
            {
                axvcpu::AxArchVCpu::set_entry(arch_vcpu, entry)?;
                axvcpu::AxArchVCpu::set_ept_root(arch_vcpu, ept_root)?;
                axvcpu::AxArchVCpu::setup(arch_vcpu, setup_config)
            }
        })
    }

    /// Returns to the VM's configuration.
    pub fn with_config<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut AxVMConfig) -> R,
    {
        let mut g = self.inner_mut.lock();
        f(&mut g.config)
    }

    /// Returns guest VM image load region in `Vec<&'static mut [u8]>`,
    /// according to the given `image_load_gpa` and `image_size.
    /// `Vec<&'static mut [u8]>` is a series of (HVA) address segments,
    /// which may correspond to non-contiguous physical addresses,
    ///
    /// FIXME:
    /// Find a more elegant way to manage potentially non-contiguous physical memory
    ///         instead of `Vec<&'static mut [u8]>`.
    pub fn get_image_load_region(
        &self,
        image_load_gpa: GuestPhysAddr,
        image_size: usize,
    ) -> AxResult<Vec<&'static mut [u8]>> {
        let g = self.inner_mut.lock();
        let image_load_hva = g
            .address_space
            .translated_byte_buffer(image_load_gpa, image_size)
            .expect("Failed to translate kernel image load address");
        Ok(image_load_hva)
    }

    /// Boots the VM by transitioning to Running state.
    pub fn boot(&self) -> AxResult {
        if !has_hardware_support() {
            ax_err!(Unsupported, "Hardware does not support virtualization")
        } else if self.running() {
            ax_err!(BadState, format!("VM[{}] is already running", self.id()))
        } else {
            info!("Booting VM[{}]", self.id());
            self.set_vm_status(VMStatus::Running);
            Ok(())
        }
    }

    /// Returns if the VM is running.
    pub fn running(&self) -> bool {
        self.vm_status() == VMStatus::Running
    }

    /// Returns if the VM is shutting down (in Stopping state).
    pub fn stopping(&self) -> bool {
        self.vm_status() == VMStatus::Stopping
    }

    /// Returns if the VM is suspended.
    pub fn suspending(&self) -> bool {
        self.vm_status() == VMStatus::Suspended
    }

    /// Returns if the VM is stopped.
    pub fn stopped(&self) -> bool {
        self.vm_status() == VMStatus::Stopped
    }

    /// Shuts down the VM by transitioning to Stopping state.
    ///
    /// This method sets the VM status to Stopping, which signals all vCPUs to exit.
    /// Currently, the "re-init" process of the VM is not implemented. Therefore, a VM can only be
    /// booted once. And after the VM is shut down, it cannot be booted again.
    pub fn shutdown(&self) -> AxResult {
        if self.stopping() {
            ax_err!(BadState, format!("VM[{}] is already stopping", self.id()))
        } else if self.stopped() {
            ax_err!(BadState, format!("VM[{}] is already stopped", self.id()))
        } else {
            info!("Shutting down VM[{}]", self.id());
            self.set_vm_status(VMStatus::Stopping);
            Ok(())
        }
    }

    // TODO: implement suspend/resume.
    // TODO: implement re-init.

    /// Returns this VM's emulated devices.
    pub fn get_devices(&self) -> &AxVmDevices {
        &self.inner_const().devices
    }

    /// Run a vCPU according to the given vcpu_id.
    ///
    /// ## Arguments
    /// * `vcpu_id` - the id of the vCPU to run.
    ///
    /// ## Returns
    /// * `AxVCpuExitReason` - the exit reason of the vCPU, wrapped in an `AxResult`.
    pub fn run_vcpu(&self, vcpu_id: usize) -> AxResult<AxVCpuExitReason> {
        let mut exit_reason = self.run_vcpu_raw(vcpu_id)?;
        let vcpu = self
            .vcpu(vcpu_id)
            .ok_or_else(|| ax_err_type!(InvalidInput, "Invalid vcpu_id"))?;

        let exit_reason = loop {
            trace!("run_vcpu: got vm-exit");
            match exit_reason {
                AxVCpuExitReason::MmioRead {
                    addr,
                    width,
                    reg,
                    reg_width,
                    signed_ext,
                } => {
                    let raw = match self.get_devices().handle_mmio_read(addr, width) {
                        Ok(raw) => raw,
                        Err(err) => {
                            let detail = format!(
                                "axvm::run_vcpu mmio_read_err vm={} vcpu={} gpa={:#x} width={width:?} reg={reg} reg_width={reg_width:?} signed_ext={signed_ext} err={err:?}",
                                self.id(),
                                vcpu_id,
                                addr.as_usize()
                            );
                            host_emerg_line(&detail);
                            return Err(err);
                        }
                    };
                    let masked = raw & width_mask(width);
                    let val = if signed_ext {
                        sign_extend_value(masked, width)
                    } else {
                        masked & width_mask(reg_width)
                    };
                    vcpu.set_gpr(reg, val);
                }
                AxVCpuExitReason::MmioWrite { addr, width, data } => {
                    if let Err(err) =
                        self.get_devices()
                            .handle_mmio_write(addr, width, data as usize)
                    {
                        let detail = format!(
                            "axvm::run_vcpu mmio_write_err vm={} vcpu={} gpa={:#x} width={width:?} data={data:#x} err={err:?}",
                            self.id(),
                            vcpu_id,
                            addr.as_usize()
                        );
                        host_emerg_line(&detail);
                        return Err(err);
                    }
                }
                AxVCpuExitReason::IoRead { port, width } => {
                    let val = match self.get_devices().handle_port_read(port, width) {
                        Ok(val) => val,
                        Err(err) => {
                            let detail = format!(
                                "axvm::run_vcpu io_read_err vm={} vcpu={} port={:#x} width={width:?} err={err:?}",
                                self.id(),
                                vcpu_id,
                                port.0,
                            );
                            host_emerg_line(&detail);
                            return Err(err);
                        }
                    };
                    #[cfg(not(target_arch = "riscv64"))]
                    vcpu.set_gpr(0, val); // The target is always eax/ax/al, todo: handle access_width correctly

                    #[cfg(target_arch = "riscv64")]
                    vcpu.set_gpr(riscv_vcpu::GprIndex::A0 as usize, val);
                }
                AxVCpuExitReason::IoWrite { port, width, data } => {
                    if let Err(err) =
                        self.get_devices()
                            .handle_port_write(port, width, data as usize)
                    {
                        let detail = format!(
                            "axvm::run_vcpu io_write_err vm={} vcpu={} port={:#x} width={width:?} data={data:#x} err={err:?}",
                            self.id(),
                            vcpu_id,
                            port.0,
                        );
                        host_emerg_line(&detail);
                        return Err(err);
                    }
                }
                AxVCpuExitReason::SysRegRead { addr, reg } => {
                    let val = match self.get_devices().handle_sys_reg_read(
                        addr,
                        // Generally speaking, the width of system register is fixed and needless to be specified.
                        // AccessWidth::Qword here is just a placeholder, may be changed in the future.
                        AccessWidth::Qword,
                    ) {
                        Ok(val) => val,
                        Err(err) => {
                            let detail = format!(
                                "axvm::run_vcpu sysreg_read_err vm={} vcpu={} addr={:#x} reg={reg} err={err:?}",
                                self.id(),
                                vcpu_id,
                                addr.addr(),
                            );
                            host_emerg_line(&detail);
                            return Err(err);
                        }
                    };
                    vcpu.set_gpr(reg, val);
                }
                AxVCpuExitReason::SysRegWrite { addr, value } => {
                    if let Err(err) = self.get_devices().handle_sys_reg_write(
                        addr,
                        AccessWidth::Qword,
                        value as usize,
                    ) {
                        let detail = format!(
                            "axvm::run_vcpu sysreg_write_err vm={} vcpu={} addr={:#x} value={value:#x} err={err:?}",
                            self.id(),
                            vcpu_id,
                            addr.addr(),
                        );
                        host_emerg_line(&detail);
                        return Err(err);
                    }
                }
                AxVCpuExitReason::ExternalInterrupt { .. } => {
                    #[cfg(target_arch = "x86_64")]
                    x86_inject_pending_serial_irq(self, &vcpu);
                    break exit_reason;
                }
                AxVCpuExitReason::PreemptionTimer => {
                    #[cfg(target_arch = "x86_64")]
                    {
                        x86_inject_due_pit_irq0(self, &vcpu);
                        x86_inject_pending_serial_irq(self, &vcpu);
                    }
                    break exit_reason;
                }
                AxVCpuExitReason::InterruptEnd { .. } => break exit_reason,
                AxVCpuExitReason::Halt => {
                    #[cfg(target_arch = "x86_64")]
                    {
                        x86_inject_due_pit_irq0(self, &vcpu);
                        x86_inject_pending_serial_irq(self, &vcpu);
                    }
                    break exit_reason;
                }
                exit_reason => break exit_reason,
            }

            exit_reason = self.run_vcpu_raw(vcpu_id)?;
        };

        Ok(exit_reason)
    }

    /// Run a vCPU and return raw VM exits that a userspace VMM must handle.
    ///
    /// This path is used by KVM ABI providers. Unlike [`Self::run_vcpu`], it does not
    /// consume MMIO or PIO exits through AxVisor's in-kernel emulated devices.
    pub fn run_vcpu_raw(&self, vcpu_id: usize) -> AxResult<AxVCpuExitReason> {
        let vcpu = self
            .vcpu(vcpu_id)
            .ok_or_else(|| ax_err_type!(InvalidInput, "Invalid vcpu_id"))?;

        if let Err(err) = vcpu.bind() {
            let detail = format!(
                "axvm::run_vcpu bind_err vm={} vcpu={} err={err:?}",
                self.id(),
                vcpu_id
            );
            host_emerg_line(&detail);
            return Err(err);
        }

        let exit_reason = loop {
            let exit_reason = match vcpu.run() {
                Ok(exit_reason) => exit_reason,
                Err(err) => {
                    let detail = format!(
                        "axvm::run_vcpu arch_run_err vm={} vcpu={} err={err:?}",
                        self.id(),
                        vcpu_id
                    );
                    host_emerg_line(&detail);
                    return Err(err);
                }
            };
            trace!("run_vcpu: got vm-exit");
            match exit_reason {
                AxVCpuExitReason::NestedPageFault { addr, access_flags } => {
                    // axvm inner address space may resolve the fault directly
                    // (statically-mapped guest RAM).
                    if self.handle_nested_page_fault(addr, access_flags) {
                        continue;
                    }
                    #[cfg(target_arch = "x86_64")]
                    {
                        // KVM ordering (arch/x86/kvm/mmu/mmu.c kvm_faultin_pfn):
                        // resolve the fault against a registered memslot FIRST;
                        // only a slotless GPA is emulated as MMIO. Our memslots
                        // are lazily populated (no eager pin), so a real RAM
                        // access surfaces here with no backing yet -> fault it in.
                        // Passing backend_vm=0 lets the C side resolve the VM via
                        // the running vCPU (we are on the vCPU's own thread).
                        let write = if access_flags.contains(MappingFlags::WRITE) {
                            1u32
                        } else {
                            0u32
                        };
                        let rc = unsafe {
                            axvisor_kvm_x86_bridge_fault_in_gpa(
                                0,
                                addr.as_usize() as u64,
                                write,
                            )
                        };
                        if rc == 0 {
                            // Page faulted in and mapped into the EPT -> re-enter.
                            continue;
                        }
                        // rc == -ENOENT (2 on Linux) means the GPA is not in any
                        // memslot: fall through to MMIO emulation, mirroring KVM's
                        // !slot -> kvm_handle_noslot_fault path. Any OTHER negative
                        // rc is a genuine fault-in error (pin/map/OOM) that must
                        // NOT be silently emulated as MMIO -> surface the fault.
                        const NEG_ENOENT: i32 = -2;
                        if rc != NEG_ENOENT {
                            host_emerg_line(&format!(
                                "axvm::run_vcpu fault_in error vm={} vcpu={} gpa={:#x} rc={}",
                                self.id(),
                                vcpu_id,
                                addr.as_usize(),
                                rc
                            ));
                            break AxVCpuExitReason::NestedPageFault { addr, access_flags };
                        }
                    }
                    if let Some(mmio_exit) = self.decode_x86_mmio_exit(vcpu_id, addr, access_flags)
                    {
                        break mmio_exit;
                    }
                    break AxVCpuExitReason::NestedPageFault { addr, access_flags };
                }
                exit_reason => break exit_reason,
            }
        };

        if let Err(err) = vcpu.unbind() {
            let detail = format!(
                "axvm::run_vcpu unbind_err vm={} vcpu={} err={err:?}",
                self.id(),
                vcpu_id
            );
            host_emerg_line(&detail);
            return Err(err);
        }
        Ok(exit_reason)
    }

    #[cfg(target_arch = "x86_64")]
    fn decode_x86_mmio_exit(
        &self,
        vcpu_id: usize,
        fault_gpa: GuestPhysAddr,
        access_flags: MappingFlags,
    ) -> Option<AxVCpuExitReason> {
        let vcpu = self.vcpu(vcpu_id)?;
        let mut instr = [0u8; 15];
        let decoded = vcpu
            .with_arch_vcpu(|arch_vcpu| {
                let rip = arch_vcpu.rip();
                let gla = arch_vcpu.gla2gva(GuestVirtAddr::from_usize(rip));
                let fetch_gpa = self.x86_guest_linear_to_phys(gla, &arch_vcpu.get_ptw_info())?;
                self.read_guest_bytes(fetch_gpa, &mut instr)?;
                decode_x86_mov_mmio(&instr, fault_gpa, access_flags, arch_vcpu)
            })
            .ok()?;
        decoded
    }

    #[cfg(target_arch = "x86_64")]
    fn x86_guest_linear_to_phys(
        &self,
        linear: GuestVirtAddr,
        ptw: &GuestPageWalkInfo,
    ) -> AxResult<GuestPhysAddr> {
        let va = linear.as_usize() as u64;

        if ptw.level == 0 {
            return Ok(GuestPhysAddr::from_usize(va as usize));
        }

        match ptw.level {
            4 => self.x86_walk_4level(va, ptw.top_entry as u64),
            3 => self.x86_walk_pae(va, ptw.top_entry as u64),
            2 => self.x86_walk_32bit(va, ptw.top_entry as u64, ptw.pse),
            _ => ax_err!(InvalidInput, "unsupported x86 guest paging level"),
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn x86_read_guest_pte(&self, table_base: u64, index: u64) -> AxResult<u64> {
        let gpa = GuestPhysAddr::from_usize((table_base + index * 8) as usize);
        self.read_from_guest_of::<u64>(gpa)
    }

    #[cfg(target_arch = "x86_64")]
    fn x86_read_guest_pde32(&self, table_base: u64, index: u64) -> AxResult<u32> {
        let gpa = GuestPhysAddr::from_usize((table_base + index * 4) as usize);
        self.read_from_guest_of::<u32>(gpa)
    }

    #[cfg(target_arch = "x86_64")]
    fn x86_require_present(entry: u64) -> AxResult {
        if entry & X86_PAGE_PRESENT == 0 {
            return ax_err!(NotFound, "x86 guest page table entry is not present");
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn x86_walk_4level(&self, va: u64, cr3: u64) -> AxResult<GuestPhysAddr> {
        let pml4_base = cr3 & X86_PAGE_ADDR_MASK;
        let pml4e = self.x86_read_guest_pte(pml4_base, (va >> 39) & 0x1ff)?;
        Self::x86_require_present(pml4e)?;

        let pdpt_base = pml4e & X86_PAGE_ADDR_MASK;
        let pdpte = self.x86_read_guest_pte(pdpt_base, (va >> 30) & 0x1ff)?;
        Self::x86_require_present(pdpte)?;
        if pdpte & X86_PAGE_SIZE != 0 {
            let page_base = pdpte & 0x000f_ffff_c000_0000;
            return Ok(GuestPhysAddr::from_usize((page_base | (va & 0x3fff_ffff)) as usize));
        }

        let pd_base = pdpte & X86_PAGE_ADDR_MASK;
        let pde = self.x86_read_guest_pte(pd_base, (va >> 21) & 0x1ff)?;
        Self::x86_require_present(pde)?;
        if pde & X86_PAGE_SIZE != 0 {
            let page_base = pde & 0x000f_ffff_ffe0_0000;
            return Ok(GuestPhysAddr::from_usize((page_base | (va & 0x1f_ffff)) as usize));
        }

        let pt_base = pde & X86_PAGE_ADDR_MASK;
        let pte = self.x86_read_guest_pte(pt_base, (va >> 12) & 0x1ff)?;
        Self::x86_require_present(pte)?;
        Ok(GuestPhysAddr::from_usize(
            ((pte & X86_PAGE_ADDR_MASK) | (va & 0xfff)) as usize,
        ))
    }

    #[cfg(target_arch = "x86_64")]
    fn x86_walk_pae(&self, va: u64, cr3: u64) -> AxResult<GuestPhysAddr> {
        let pdpt_base = cr3 & 0xffff_ffe0;
        let pdpte = self.x86_read_guest_pte(pdpt_base, (va >> 30) & 0x3)?;
        Self::x86_require_present(pdpte)?;

        let pd_base = pdpte & X86_PAGE_ADDR_MASK;
        let pde = self.x86_read_guest_pte(pd_base, (va >> 21) & 0x1ff)?;
        Self::x86_require_present(pde)?;
        if pde & X86_PAGE_SIZE != 0 {
            let page_base = pde & 0x000f_ffff_ffe0_0000;
            return Ok(GuestPhysAddr::from_usize((page_base | (va & 0x1f_ffff)) as usize));
        }

        let pt_base = pde & X86_PAGE_ADDR_MASK;
        let pte = self.x86_read_guest_pte(pt_base, (va >> 12) & 0x1ff)?;
        Self::x86_require_present(pte)?;
        Ok(GuestPhysAddr::from_usize(
            ((pte & X86_PAGE_ADDR_MASK) | (va & 0xfff)) as usize,
        ))
    }

    #[cfg(target_arch = "x86_64")]
    fn x86_walk_32bit(&self, va: u64, cr3: u64, pse: bool) -> AxResult<GuestPhysAddr> {
        let pd_base = cr3 & 0xffff_f000;
        let pde = self.x86_read_guest_pde32(pd_base, (va >> 22) & 0x3ff)? as u64;
        Self::x86_require_present(pde)?;
        if pse && pde & X86_PAGE_SIZE != 0 {
            let page_base = pde & 0xffc0_0000;
            return Ok(GuestPhysAddr::from_usize((page_base | (va & 0x3f_ffff)) as usize));
        }

        let pt_base = pde & 0xffff_f000;
        let pte = self.x86_read_guest_pde32(pt_base, (va >> 12) & 0x3ff)? as u64;
        Self::x86_require_present(pte)?;
        Ok(GuestPhysAddr::from_usize(
            ((pte & 0xffff_f000) | (va & 0xfff)) as usize,
        ))
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn decode_x86_mmio_exit(
        &self,
        _vcpu_id: usize,
        _fault_gpa: GuestPhysAddr,
        _access_flags: MappingFlags,
    ) -> Option<AxVCpuExitReason> {
        None
    }

    fn handle_nested_page_fault(&self, addr: GuestPhysAddr, access_flags: MappingFlags) -> bool {
        let mut guard = self.inner_mut.lock();
        let handled = guard.address_space.handle_page_fault(addr, access_flags);
        Self::debug_nested_page_fault(self.id(), &guard, addr, access_flags, handled);
        handled
    }

    fn debug_nested_page_fault(
        vm_id: usize,
        inner: &AxVMInnerMut,
        addr: GuestPhysAddr,
        access_flags: MappingFlags,
        handled: bool,
    ) {
        let _ = inner;
        let _ = access_flags;
        if handled {
            debug!("VM[{}] stage2 fault handled", vm_id);
        } else {
            warn!("VM[{}] stage2 fault unhandled gpa={:#x}", vm_id, addr.as_usize());
        }
    }

    /// Injects an interrupt to the vCPU.
    pub fn inject_interrupt_to_vcpu(
        &self,
        targets: CpuMask<TEMP_MAX_VCPU_NUM>,
        irq: usize,
    ) -> AxResult {
        axvisor_api::vmm::inject_interrupt_to_cpus(self.id(), targets, irq as InterruptVector);

        Ok(())
    }

    /// Returns vCpu id list and its corresponding pCpu affinity list, as well as its physical id.
    /// If the pCpu affinity is None, it means the vCpu will be allocated to any available pCpu randomly.
    /// if the pCPU id is not provided, the vCpu's physical id will be set as vCpu id.
    ///
    /// Returns a vector of tuples, each tuple contains:
    /// - The vCpu id.
    /// - The pCpu affinity mask, `None` if not set.
    /// - The physical id of the vCpu, equal to vCpu id if not provided.
    pub fn get_vcpu_affinities_pcpu_ids(&self) -> Vec<(usize, Option<usize>, usize)> {
        self.inner_const()
            .phys_cpu_ls
            .get_vcpu_affinities_pcpu_ids()
    }

    // /// Returns a reference to the VM's configuration.
    // pub fn config(&self) -> &AxVMConfig {
    //     &self.inner_const.config
    // }

    /// Maps a region of host physical memory to guest physical memory.
    pub fn map_region(
        &self,
        gpa: GuestPhysAddr,
        hpa: HostPhysAddr,
        size: usize,
        flags: MappingFlags,
    ) -> AxResult {
        self.inner_mut
            .lock()
            .address_space
            .map_linear(gpa, hpa, size, flags)?;
        Ok(())
    }

    /// Unmaps a region of guest physical memory.
    pub fn unmap_region(&self, gpa: GuestPhysAddr, size: usize) -> AxResult {
        self.inner_mut.lock().address_space.unmap(gpa, size)?;
        Ok(())
    }

    /// Reads an object of type `T` from the guest physical address.
    pub fn read_from_guest_of<T>(&self, gpa_ptr: GuestPhysAddr) -> AxResult<T> {
        let size = core::mem::size_of::<T>();

        // Ensure the address is properly aligned for the type.
        if !gpa_ptr
            .as_usize()
            .is_multiple_of(core::mem::align_of::<T>())
        {
            return ax_err!(InvalidInput, "Unaligned guest physical address");
        }

        let g = self.inner_mut.lock();
        match g.address_space.translated_byte_buffer(gpa_ptr, size) {
            Some(buffers) => {
                let mut data_bytes = Vec::with_capacity(size);
                for chunk in buffers {
                    let remaining = size - data_bytes.len();
                    let chunk_size = remaining.min(chunk.len());
                    data_bytes.extend_from_slice(&chunk[..chunk_size]);
                    if data_bytes.len() >= size {
                        break;
                    }
                }
                if data_bytes.len() < size {
                    return ax_err!(
                        InvalidInput,
                        "Insufficient data in guest memory to read the requested object"
                    );
                }
                let data: T = unsafe {
                    // Use `ptr::read_unaligned` for safety in case of unaligned memory.
                    core::ptr::read_unaligned(data_bytes.as_ptr() as *const T)
                };
                Ok(data)
            }
            None => ax_err!(
                InvalidInput,
                "Failed to translate guest physical address or insufficient buffer size"
            ),
        }
    }

    /// Reads raw bytes from a guest physical address.
    pub fn read_guest_bytes(&self, gpa_ptr: GuestPhysAddr, out: &mut [u8]) -> AxResult {
        if out.is_empty() {
            return Ok(());
        }

        let g = self.inner_mut.lock();
        match g.address_space.translated_byte_buffer(gpa_ptr, out.len()) {
            Some(buffers) => {
                let mut copied = 0;
                for chunk in buffers {
                    let remaining = out.len() - copied;
                    let chunk_size = remaining.min(chunk.len());
                    out[copied..copied + chunk_size].copy_from_slice(&chunk[..chunk_size]);
                    copied += chunk_size;
                    if copied >= out.len() {
                        return Ok(());
                    }
                }
                ax_err!(InvalidInput, "insufficient guest memory bytes")
            }
            None => ax_err!(InvalidInput, "failed to translate guest byte buffer"),
        }
    }

    /// Writes an object of type `T` to the guest physical address.
    pub fn write_to_guest_of<T>(&self, gpa_ptr: GuestPhysAddr, data: &T) -> AxResult {
        match self
            .inner_mut
            .lock()
            .address_space
            .translated_byte_buffer(gpa_ptr, core::mem::size_of::<T>())
        {
            Some(mut buffer) => {
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        data as *const T as *const u8,
                        core::mem::size_of::<T>(),
                    )
                };
                let mut copied_bytes = 0;
                for chunk in buffer.iter_mut() {
                    let end = copied_bytes + chunk.len();
                    chunk.copy_from_slice(&bytes[copied_bytes..end]);
                    copied_bytes += chunk.len();
                }
                Ok(())
            }
            None => ax_err!(InvalidInput, "Failed to translate guest physical address"),
        }
    }

    /// Allocates an IVC channel for inter-VM communication region.
    ///
    /// ## Arguments
    /// * `expected_size` - The expected size of the IVC channel in bytes.
    /// ## Returns
    /// * `AxResult<(GuestPhysAddr, usize)>` - A tuple containing the guest physical address of the allocated IVC channel and its actual size.
    pub fn alloc_ivc_channel(&self, expected_size: usize) -> AxResult<(GuestPhysAddr, usize)> {
        // Ensure the expected size is aligned to 4K.
        let size = align_up_4k(expected_size);
        let gpa = self.inner_const().devices.alloc_ivc_channel(size)?;
        Ok((gpa, size))
    }

    /// Releases an IVC channel for inter-VM communication region.
    /// ## Arguments
    /// * `gpa` - The guest physical address of the IVC channel to release.
    /// * `size` - The size of the IVC channel in bytes.
    /// ## Returns
    /// * `AxResult<()>` - An empty result indicating success or failure.
    pub fn release_ivc_channel(&self, gpa: GuestPhysAddr, size: usize) -> AxResult {
        self.inner_const().devices.release_ivc_channel(gpa, size)?;
        Ok(())
    }

    /// Allocates a new memory region for the VM.
    pub fn alloc_memory_region(
        &self,
        layout: Layout,
        gpa: Option<GuestPhysAddr>,
    ) -> AxResult<()> {
        assert!(
            layout.size() > 0,
            "Cannot allocate zero-sized memory region"
        );

        let gpa = gpa.ok_or(AxError::InvalidInput)?;

        let mut g = self.inner_mut.lock();
        g.address_space.map_alloc(
            gpa,
            layout.size(),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE | MappingFlags::USER,
            true,
        )?;
        let image_load_hva = g
            .address_space
            .translated_byte_buffer(gpa, layout.size())
            .ok_or(AxError::BadAddress)?;
        let hva = HostVirtAddr::from(image_load_hva[0].as_ptr() as usize);
        g.memory_regions.push(VMMemoryRegion {
            gpa,
            hva,
            layout,
            backing: VMMemoryRegionBacking::PagedAlloc,
        });

        Ok(())
    }

    /// Returns a list of all memory regions in the VM.
    pub fn memory_regions(&self) -> Vec<VMMemoryRegion> {
        self.inner_mut.lock().memory_regions.clone()
    }

    /// Maps a guest RAM region whose guest physical address is the host physical address.
    pub fn map_identical_memory_region(
        &self,
        layout: Layout,
        gpa: Option<GuestPhysAddr>,
    ) -> AxResult<()> {
        assert!(
            layout.size() > 0,
            "Cannot allocate zero-sized memory region"
        );
        let gpa =
            gpa.ok_or_else(|| ax_err_type!(InvalidInput, "identical memory gpa is missing"))?;
        let hpa = HostPhysAddr::from(gpa.as_usize());
        Self::register_linux_guest_ram(hpa, layout.size())?;
        let hva = axvisor_api::memory::phys_to_virt(hpa);
        if hva.as_usize() == 0 {
            return Err(ax_err_type!(
                BadAddress,
                format!(
                    "failed to translate identical memory GPA {:#x} into host-accessible HVA",
                    gpa.as_usize()
                )
            ));
        }

        let mut g = self.inner_mut.lock();
        g.address_space.map_linear(
            gpa,
            hpa,
            layout.size(),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE | MappingFlags::USER,
        )?;
        g.memory_regions.push(VMMemoryRegion {
            gpa,
            hva,
            layout,
            backing: VMMemoryRegionBacking::Reserved,
        });
        Ok(())
    }

    /// Maps a reserved memory region for the VM.
    pub fn map_reserved_memory_region(
        &self,
        layout: Layout,
        gpa: Option<GuestPhysAddr>,
    ) -> AxResult<()> {
        assert!(
            layout.size() > 0,
            "Cannot allocate zero-sized memory region"
        );
        let gpa = gpa.ok_or_else(|| ax_err_type!(InvalidInput, "reserved memory gpa is missing"))?;
        let hpa = HostPhysAddr::from(gpa.as_usize());
        Self::register_linux_guest_ram(hpa, layout.size())?;
        let hva = axvisor_api::memory::phys_to_virt(hpa);
        if hva.as_usize() == 0 {
            return Err(ax_err_type!(
                BadAddress,
                format!(
                    "failed to translate reserved memory GPA {:#x} into host-accessible HVA",
                    gpa.as_usize()
                )
            ));
        }
        // In Linux-host mode, reserved guest RAM may come from a `no-map`
        // host physical carveout. We only need a base HVA here for bookkeeping;
        // actual page-by-page writable mappings are materialized later through
        // `translated_byte_buffer()` and `phys_to_virt()`.
        let mut g = self.inner_mut.lock();
        g.address_space.map_linear(
            gpa,
            hpa,
            layout.size(),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE | MappingFlags::USER,
        )?;
        g.memory_regions.push(VMMemoryRegion {
            gpa,
            hva,
            layout,
            backing: VMMemoryRegionBacking::Reserved,
        });
        Ok(())
    }

    /// Cleanup resources for the VM before drop.
    /// This is called internally by the Drop implementation.
    fn cleanup_resources(&self) {
        info!("Cleaning up VM[{}] resources...", self.id());

        // 1. Ensure the VM is in Stopping or Stopped state
        let current_status = self.vm_status();
        if !matches!(current_status, VMStatus::Stopping | VMStatus::Stopped) {
            warn!(
                "VM[{}] is being dropped without explicit shutdown (status: {:?}), marking as \
                 stopping",
                self.id(),
                current_status
            );
            self.set_vm_status(VMStatus::Stopping);
        }

        let mut inner_mut = self.inner_mut.lock();

        // First, collect all memory regions to clean up
        // We need to clone the regions to avoid borrowing issues
        let regions_to_cleanup: Vec<VMMemoryRegion> = inner_mut.memory_regions.clone();

        // Unmap all memory regions from the address space
        // This must be done BEFORE deallocating memory to avoid use-after-free
        for region in &regions_to_cleanup {
            debug!(
                "VM[{}] unmapping memory region: GPA={:#x}, size={:#x}",
                self.id(),
                region.gpa.as_usize(),
                region.size()
            );
            // Unmap the region from guest physical address space
            if let Err(e) = inner_mut.address_space.unmap(region.gpa, region.size()) {
                warn!(
                    "VM[{}] failed to unmap region at GPA={:#x}: {:?}",
                    self.id(),
                    region.gpa.as_usize(),
                    e
                );
            }
        }

        // Now it's safe to deallocate the memory
        for region in &regions_to_cleanup {
            match region.backing {
                VMMemoryRegionBacking::PagedAlloc => {
                    debug!(
                        "VM[{}] paged allocation already released by unmap: GPA={:#x}, size={:#x}",
                        self.id(),
                        region.gpa.as_usize(),
                        region.size()
                    );
                }
                VMMemoryRegionBacking::Reserved => {
                    debug!(
                        "VM[{}] skipping dealloc for reserved memory region: GPA={:#x}, HVA={:#x}, \
                         size={:#x}",
                        self.id(),
                        region.gpa.as_usize(),
                        region.hva.as_usize(),
                        region.size()
                    );
                }
            }
        }
        inner_mut.memory_regions.clear();

        // Clear remaining address space mappings
        // This includes:
        // - Passthrough device MMIO mappings
        // - Emulated device MMIO mappings
        // - Reserved memory mappings
        // - All other page table entries
        debug!(
            "VM[{}] clearing remaining address space mappings",
            self.id()
        );
        inner_mut.address_space.clear();

        // Release the lock before accessing inner_const
        drop(inner_mut);

        // Device cleanup
        // Although devices will be automatically dropped when inner_const is dropped,
        // we should perform explicit cleanup if devices hold resources like:
        // - Hardware interrupt registrations
        // - DMA mappings
        // - Background threads or timers
        if let Some(inner_const) = self.inner_const.get() {
            debug!(
                "VM[{}] devices cleanup: {} MMIO devices, {} SysReg devices",
                self.id(),
                inner_const.devices.iter_mmio_dev().count(),
                inner_const.devices.iter_sys_reg_dev().count()
            );

            // TODO: Add device-specific cleanup if needed
            // For example:
            // - Stop device background tasks
            // - Unregister interrupts
            // - Release device-specific resources

            // Note: Device Arc references will be dropped automatically when
            // inner_const is dropped at the end of AxVM's drop
        }

        info!("VM[{}] resources cleanup completed", self.id());
    }
}

impl Drop for AxVM {
    fn drop(&mut self) {
        info!("Dropping VM[{}]", self.id());

        // Clean up all allocated resources
        self.cleanup_resources();

        info!("VM[{}] dropped", self.id());
    }
}
