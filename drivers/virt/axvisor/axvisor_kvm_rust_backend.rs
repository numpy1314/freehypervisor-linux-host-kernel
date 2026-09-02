// SPDX-License-Identifier: GPL-2.0

//! Rust-side backend symbols for the AxVisor KVM ABI provider.
//!
//! This stage owns the Rust VM/vCPU handle lifecycle and memslot bookkeeping.
//! `run_vcpu` still returns `-EOPNOTSUPP`; the C KVM provider translates that
//! into the existing deterministic `KVM_EXIT_FAIL_ENTRY` until AxVM execution is
//! wired into these handles.

#![allow(missing_docs)]

use kernel::alloc::{flags::GFP_KERNEL, KBox, KVec};

const EINVAL: i32 = 22;
const ENOMEM: i32 = 12;
const EOPNOTSUPP: i32 = 95;
const EEXIST: i32 = 17;
const AXKVM_BACKEND_EXIT_FAIL_ENTRY: u32 = 7;
const MAX_CPUID_ENTRIES: usize = 256;
const MAX_MSR_ENTRIES: usize = 256;
const NUM_SEGMENTS: usize = 8;
const NUM_DTABLES: usize = 2;

#[derive(Clone, Copy)]
struct Mapping {
    gpa: u64,
    hpa: u64,
    flags: u32,
}

#[derive(Clone, Copy)]
struct VcpuEntry {
    id: u32,
    handle: u64,
}

struct BackendVm {
    mappings: KVec<Mapping>,
    vcpus: KVec<VcpuEntry>,
    booted: bool,
    state: BackendVmState,
}

struct BackendVcpu {
    vm_handle: u64,
    id: u32,
    state: BackendVcpuState,
    regs: BackendRegs,
    sregs: BackendSregs,
    segments: [BackendSegment; NUM_SEGMENTS],
    dtables: [BackendDtable; NUM_DTABLES],
    cpuid_entries: KVec<CpuidEntry>,
    msr_entries: KVec<MsrEntry>,
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
struct BackendVmState {
    version: u32,
    arch: u32,
    irqchip_created: bool,
    pit_created: bool,
    pit_flags: u32,
    tss_addr: u32,
    identity_map_addr: u64,
    nr_irqchips: u32,
}

impl BackendVmState {
    const fn empty() -> Self {
        Self {
            version: 0,
            arch: 0,
            irqchip_created: false,
            pit_created: false,
            pit_flags: 0,
            tss_addr: 0,
            identity_map_addr: 0,
            nr_irqchips: 0,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
struct BackendVcpuState {
    version: u32,
    arch: u32,
    rip: u64,
    rsp: u64,
    rflags: u64,
    cr0: u64,
    cr3: u64,
    cr4: u64,
    efer: u64,
    apic_base: u64,
    cpuid_nent: u32,
    nmsrs: u32,
    tsc_khz: u32,
}

impl BackendVcpuState {
    const fn empty() -> Self {
        Self {
            version: 0,
            arch: 0,
            rip: 0,
            rsp: 0,
            rflags: 0,
            cr0: 0,
            cr3: 0,
            cr4: 0,
            efer: 0,
            apic_base: 0,
            cpuid_nent: 0,
            nmsrs: 0,
            tsc_khz: 0,
        }
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
#[derive(Clone, Copy)]
struct MsrEntry {
    index: u32,
    data: u64,
}

fn vm_from_handle(handle: u64) -> Result<&'static mut BackendVm, i32> {
    if handle == 0 {
        return Err(-EINVAL);
    }

    // SAFETY: Handles returned by `axvisor_kvm_rs_create_vm` are KBox-owned
    // `BackendVm` pointers. C owns synchronization before calling backend ops.
    Ok(unsafe { &mut *(handle as *mut BackendVm) })
}

fn vcpu_from_handle(handle: u64) -> Result<&'static mut BackendVcpu, i32> {
    if handle == 0 {
        return Err(-EINVAL);
    }

    // SAFETY: Handles returned by `axvisor_kvm_rs_create_vcpu` are KBox-owned
    // `BackendVcpu` pointers. C owns synchronization before calling backend ops.
    Ok(unsafe { &mut *(handle as *mut BackendVcpu) })
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

    let vm = match KBox::new(
        BackendVm {
            mappings: KVec::new(),
            vcpus: KVec::new(),
            booted: false,
            state: BackendVmState::empty(),
        },
        GFP_KERNEL,
    ) {
        Ok(vm) => vm,
        Err(_) => return -ENOMEM,
    };

    // SAFETY: `backend_vm` is checked non-null and owned by the C caller.
    unsafe { *backend_vm = KBox::into_raw(vm) as u64 };
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_destroy_vm(backend_vm: u64) {
    if backend_vm == 0 {
        return;
    }

    // SAFETY: `backend_vm` was produced by `KBox::into_raw` in create_vm and
    // destroy_vm is the terminal owner transition from C.
    drop(unsafe { KBox::from_raw(backend_vm as *mut BackendVm) });
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
) -> i32 {
    let vm = match vm_from_handle(backend_vm) {
        Ok(vm) => vm,
        Err(err) => return err,
    };

    vm.state = BackendVmState {
        version,
        arch,
        irqchip_created: irqchip_created != 0,
        pit_created: pit_created != 0,
        pit_flags,
        tss_addr,
        identity_map_addr,
        nr_irqchips,
    };
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_map_page(
    backend_vm: u64,
    gpa: u64,
    hpa: u64,
    flags: u32,
) -> i32 {
    let vm = match vm_from_handle(backend_vm) {
        Ok(vm) => vm,
        Err(err) => return err,
    };

    for mapping in vm.mappings.as_mut_slice() {
        if mapping.gpa == gpa {
            mapping.hpa = hpa;
            mapping.flags = flags;
            return 0;
        }
    }

    match vm.mappings.push(Mapping { gpa, hpa, flags }, GFP_KERNEL) {
        Ok(()) => 0,
        Err(_) => -ENOMEM,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_unmap_range(backend_vm: u64, gpa: u64, size: u64) -> i32 {
    let vm = match vm_from_handle(backend_vm) {
        Ok(vm) => vm,
        Err(err) => return err,
    };

    let end = gpa.saturating_add(size);
    vm.mappings
        .retain(|mapping| mapping.gpa < gpa || mapping.gpa >= end);
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

    let vm = match vm_from_handle(backend_vm) {
        Ok(vm) => vm,
        Err(err) => return err,
    };

    if vm.vcpus.as_slice().iter().any(|entry| entry.id == vcpu_id) {
        return -EEXIST;
    }

    let vcpu = match KBox::new(
        BackendVcpu {
            vm_handle: backend_vm,
            id: vcpu_id,
            state: BackendVcpuState::empty(),
            regs: BackendRegs::empty(),
            sregs: BackendSregs::empty(),
            segments: [BackendSegment::empty(); NUM_SEGMENTS],
            dtables: [BackendDtable::empty(); NUM_DTABLES],
            cpuid_entries: KVec::new(),
            msr_entries: KVec::new(),
        },
        GFP_KERNEL,
    ) {
        Ok(vcpu) => vcpu,
        Err(_) => return -ENOMEM,
    };

    let handle = KBox::into_raw(vcpu) as u64;
    if vm.vcpus.push(VcpuEntry { id: vcpu_id, handle }, GFP_KERNEL).is_err() {
        // SAFETY: `handle` was just produced by `KBox::into_raw` above and has
        // not been published to C yet.
        drop(unsafe { KBox::from_raw(handle as *mut BackendVcpu) });
        return -ENOMEM;
    }

    // SAFETY: `backend_vcpu` is checked non-null and owned by the C caller.
    unsafe { *backend_vcpu = handle };
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_destroy_vcpu(backend_vcpu: u64) {
    if backend_vcpu == 0 {
        return;
    }

    if let Ok(vcpu) = vcpu_from_handle(backend_vcpu) {
        if let Ok(vm) = vm_from_handle(vcpu.vm_handle) {
            vm.vcpus.retain(|entry| entry.handle != backend_vcpu);
        }
    }

    // SAFETY: `backend_vcpu` was produced by `KBox::into_raw` in create_vcpu
    // and destroy_vcpu is the terminal owner transition from C.
    drop(unsafe { KBox::from_raw(backend_vcpu as *mut BackendVcpu) });
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_state(
    backend_vcpu: u64,
    version: u32,
    arch: u32,
    rip: u64,
    rsp: u64,
    rflags: u64,
    cr0: u64,
    cr3: u64,
    cr4: u64,
    efer: u64,
    apic_base: u64,
    cpuid_nent: u32,
    nmsrs: u32,
    tsc_khz: u32,
) -> i32 {
    let vcpu = match vcpu_from_handle(backend_vcpu) {
        Ok(vcpu) => vcpu,
        Err(err) => return err,
    };

    vcpu.state = BackendVcpuState {
        version,
        arch,
        rip,
        rsp,
        rflags,
        cr0,
        cr3,
        cr4,
        efer,
        apic_base,
        cpuid_nent,
        nmsrs,
        tsc_khz,
    };
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
    let vcpu = match vcpu_from_handle(backend_vcpu) {
        Ok(vcpu) => vcpu,
        Err(err) => return err,
    };

    vcpu.regs = BackendRegs {
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
    let vcpu = match vcpu_from_handle(backend_vcpu) {
        Ok(vcpu) => vcpu,
        Err(err) => return err,
    };

    vcpu.sregs = BackendSregs {
        cr0,
        cr2,
        cr3,
        cr4,
        cr8,
        efer,
        apic_base,
    };
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
    let vcpu = match vcpu_from_handle(backend_vcpu) {
        Ok(vcpu) => vcpu,
        Err(err) => return err,
    };
    let segment_id = segment_id as usize;
    if segment_id >= NUM_SEGMENTS {
        return -EINVAL;
    }

    vcpu.segments[segment_id] = BackendSegment {
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
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_dtable(
    backend_vcpu: u64,
    table_id: u32,
    base: u64,
    limit: u32,
) -> i32 {
    let vcpu = match vcpu_from_handle(backend_vcpu) {
        Ok(vcpu) => vcpu,
        Err(err) => return err,
    };
    let table_id = table_id as usize;
    if table_id >= NUM_DTABLES {
        return -EINVAL;
    }

    vcpu.dtables[table_id] = BackendDtable { base, limit };
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
    let vcpu = match vcpu_from_handle(backend_vcpu) {
        Ok(vcpu) => vcpu,
        Err(err) => return err,
    };
    let entry_index = entry_index as usize;
    if entry_index >= MAX_CPUID_ENTRIES {
        return -EINVAL;
    }

    let entry = CpuidEntry {
        function,
        index,
        flags,
        eax,
        ebx,
        ecx,
        edx,
    };

    if entry_index < vcpu.cpuid_entries.len() {
        vcpu.cpuid_entries.as_mut_slice()[entry_index] = entry;
        return 0;
    }

    while vcpu.cpuid_entries.len() < entry_index {
        if vcpu
            .cpuid_entries
            .push(
                CpuidEntry {
                    function: 0,
                    index: 0,
                    flags: 0,
                    eax: 0,
                    ebx: 0,
                    ecx: 0,
                    edx: 0,
                },
                GFP_KERNEL,
            )
            .is_err()
        {
            return -ENOMEM;
        }
    }

    match vcpu.cpuid_entries.push(entry, GFP_KERNEL) {
        Ok(()) => 0,
        Err(_) => -ENOMEM,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_set_vcpu_msr_entry(
    backend_vcpu: u64,
    entry_index: u32,
    index: u32,
    data: u64,
) -> i32 {
    let vcpu = match vcpu_from_handle(backend_vcpu) {
        Ok(vcpu) => vcpu,
        Err(err) => return err,
    };
    let entry_index = entry_index as usize;
    if entry_index >= MAX_MSR_ENTRIES {
        return -EINVAL;
    }

    let entry = MsrEntry { index, data };
    if entry_index < vcpu.msr_entries.len() {
        vcpu.msr_entries.as_mut_slice()[entry_index] = entry;
        return 0;
    }

    while vcpu.msr_entries.len() < entry_index {
        if vcpu.msr_entries.push(MsrEntry { index: 0, data: 0 }, GFP_KERNEL).is_err() {
            return -ENOMEM;
        }
    }

    match vcpu.msr_entries.push(entry, GFP_KERNEL) {
        Ok(()) => 0,
        Err(_) => -ENOMEM,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_boot_vm(backend_vm: u64) -> i32 {
    let vm = match vm_from_handle(backend_vm) {
        Ok(vm) => vm,
        Err(err) => return err,
    };

    vm.booted = true;
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
    let vcpu = match vcpu_from_handle(backend_vcpu) {
        Ok(vcpu) => vcpu,
        Err(err) => return err,
    };

    if reason.is_null()
        || width.is_null()
        || addr.is_null()
        || data.is_null()
        || hardware_entry_failure_reason.is_null()
    {
        return -EINVAL;
    }

    // SAFETY: all output pointers were validated non-null and are owned by the
    // C bridge for the duration of this call.
    unsafe {
        *reason = AXKVM_BACKEND_EXIT_FAIL_ENTRY;
        *width = 0;
        *addr = 0;
        *data = 0;
        *hardware_entry_failure_reason = 0;
    }

    let _vcpu_id = vcpu.id;
    let _rip = vcpu.state.rip;
    -EOPNOTSUPP
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_complete_mmio_read(
    _backend_vcpu: u64,
    _data: *const core::ffi::c_void,
    _len: u32,
) -> i32 {
    -EOPNOTSUPP
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_kvm_rs_inject_irq(_backend_vm: u64, _gsi: u32) -> i32 {
    -EOPNOTSUPP
}
