// SPDX-License-Identifier: GPL-2.0

#![no_std]
#![feature(alloc_error_handler)]
#![allow(unused_extern_crates)]

extern crate alloc;
extern crate ax_percpu;

use alloc::boxed::Box;
use core::{
    alloc::{GlobalAlloc, Layout},
    ffi::c_void,
    fmt::{self, Write},
    ptr::null_mut,
};

use ax_errno::ax_err_type;
use axvisor_api::{
    api_impl, arch as api_arch, console, host, irq, memory,
    memory::{PhysAddr, VirtAddr},
    sync, task, time,
};

struct HostIfImpl;
struct ConsoleIfImpl;
struct TimeIfImpl;
struct SyncIfImpl;
struct TaskIfImpl;
struct IrqIfImpl;
struct MemoryIfImpl;
struct ArchIfImpl;

struct LinuxHostAllocator;

fn bridge_emerg(msg: &str) {
    unsafe { axvisor_linux_host_emerg_write_bytes(msg.as_ptr(), msg.len()) };
}

unsafe extern "C" {
    fn axvisor_linux_host_get_cpu_num() -> usize;
    fn axvisor_linux_host_current_cpu_id() -> usize;
    fn axvisor_linux_host_init_percpu();
    fn axvisor_linux_host_release_host_filesystems() -> i32;
    fn axvisor_linux_host_exit(exit_code: i32) -> !;

    fn axvisor_linux_console_write_bytes(bytes: *const u8, len: usize);
    fn axvisor_linux_console_read_bytes(bytes: *mut u8, len: usize) -> usize;
    fn axvisor_linux_guest_console_write_bytes(bytes: *const u8, len: usize);
    fn axvisor_linux_host_emerg_write_bytes(bytes: *const u8, len: usize);

    fn axvisor_linux_time_current_time_nanos() -> u64;
    fn axvisor_linux_time_set_oneshot_timer(deadline_nanos: u64);

    fn axvisor_linux_sync_create_wait_queue() -> usize;
    fn axvisor_linux_sync_destroy_wait_queue(queue: usize);
    fn axvisor_linux_sync_wait_queue_wait(queue: usize);
    fn axvisor_linux_sync_wait_queue_wait_until(
        queue: usize,
        condition_ctx: *mut c_void,
        condition_fn: unsafe extern "C" fn(*mut c_void) -> bool,
    );
    fn axvisor_linux_sync_wait_queue_wake_one(queue: usize);
    fn axvisor_linux_sync_wait_queue_wake_all(queue: usize);

    fn axvisor_linux_task_spawn_raw(
        name_ptr: *const u8,
        name_len: usize,
        stack_size: usize,
        cpu_set_present: bool,
        cpu_set: usize,
        entry_ctx: *mut c_void,
        entry_fn: unsafe extern "C" fn(*mut c_void),
    ) -> usize;
    fn axvisor_linux_task_join(handle_raw: usize);
    fn axvisor_linux_task_current() -> usize;
    fn axvisor_linux_task_yield_now();

    fn axvisor_linux_irq_handle(vector: usize) -> bool;
    fn axvisor_linux_irq_register(
        vector: usize,
        handler_ctx: *mut c_void,
        handler_fn: unsafe extern "C" fn(usize, *mut c_void),
    ) -> bool;

    fn axvisor_linux_memory_alloc_frame() -> u64;
    fn axvisor_linux_memory_dealloc_frame(paddr: u64);
    fn axvisor_linux_memory_phys_to_virt(paddr: u64) -> u64;
    fn axvisor_linux_memory_virt_to_phys(vaddr: u64) -> u64;
    fn axvisor_linux_memory_register_guest_ram(paddr: u64, size: u64) -> bool;
    fn axvisor_linux_memory_mmio_read32(paddr: u64) -> u32;
    fn axvisor_linux_memory_mmio_write32(paddr: u64, value: u32);
    fn axvisor_linux_riscv_plic_complete_passthrough_irq(irq_id: usize);
    fn axvisor_linux_host_register_passthrough_device(
        vm_id: usize,
        base_hpa: u64,
        length: u64,
        irq_id: usize,
    ) -> bool;

    fn axvisor_linux_arch_host_fdt_vaddr() -> u64;
    fn axvisor_linux_arch_host_fdt_size() -> usize;
    fn axvisor_adapter_host_tsc_frequency_mhz() -> u32;

    fn axvisor_adapter_runtime_alloc(size: usize, align: usize) -> *mut c_void;
    fn axvisor_adapter_runtime_realloc(
        ptr: *mut c_void,
        new_size: usize,
        align: usize,
    ) -> *mut c_void;
    fn axvisor_adapter_runtime_dealloc(ptr: *mut c_void, align: usize);
}

#[global_allocator]
static GLOBAL_ALLOCATOR: LinuxHostAllocator = LinuxHostAllocator;

unsafe impl GlobalAlloc for LinuxHostAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            return layout.align() as *mut u8;
        }
        unsafe { axvisor_adapter_runtime_alloc(layout.size(), layout.align()).cast::<u8>() }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        unsafe { axvisor_adapter_runtime_dealloc(ptr.cast::<c_void>(), layout.align()) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.size() == 0 {
            if new_size == 0 {
                return layout.align() as *mut u8;
            }
            return unsafe { axvisor_adapter_runtime_alloc(new_size, layout.align()).cast::<u8>() };
        }
        if new_size == 0 {
            unsafe { axvisor_adapter_runtime_dealloc(ptr.cast::<c_void>(), layout.align()) };
            return null_mut();
        }
        unsafe {
            axvisor_adapter_runtime_realloc(ptr.cast::<c_void>(), new_size, layout.align())
                .cast::<u8>()
        }
    }
}

struct PanicWriter;
struct EmergWriter;

impl Write for PanicWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        unsafe { axvisor_linux_console_write_bytes(s.as_ptr(), s.len()) };
        Ok(())
    }
}

impl Write for EmergWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        unsafe { axvisor_linux_host_emerg_write_bytes(s.as_ptr(), s.len()) };
        Ok(())
    }
}

#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    let _ = writeln!(
        PanicWriter,
        "[axvisor_linux_bridge] allocation failure: size={} align={}",
        layout.size(),
        layout.align()
    );
    let _ = writeln!(
        EmergWriter,
        "[axvisor_linux_bridge] allocation failure: size={} align={}",
        layout.size(),
        layout.align()
    );
    unsafe { axvisor_linux_host_exit(-12) }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let _ = writeln!(PanicWriter, "[axvisor_linux_bridge] panic: {}", info);
    let _ = writeln!(EmergWriter, "[axvisor_linux_bridge] panic: {}", info);
    unsafe { axvisor_linux_host_exit(-1) }
}

struct HostTaskEntry(Box<dyn FnOnce() + Send + 'static>);
struct HostIrqHandler(irq::IrqHandler);
struct HostWaitCondition(Box<dyn Fn() -> bool + Send + 'static>);

unsafe extern "C" fn host_task_entry_trampoline(ctx: *mut c_void) {
    let entry = unsafe { Box::from_raw(ctx.cast::<HostTaskEntry>()) };
    (entry.0)();
}

unsafe extern "C" fn host_wait_condition_trampoline(ctx: *mut c_void) -> bool {
    let condition = unsafe { &*(ctx.cast::<HostWaitCondition>()) };
    (condition.0)()
}

unsafe extern "C" fn host_irq_handler_trampoline(vector: usize, ctx: *mut c_void) {
    let handler = unsafe { &*(ctx.cast::<HostIrqHandler>()) };
    (handler.0)(vector);
}

#[api_impl]
impl host::HostIf for HostIfImpl {
    fn get_host_cpu_num() -> usize {
        unsafe { axvisor_linux_host_get_cpu_num() }
    }

    fn current_host_cpu_id() -> usize {
        unsafe { axvisor_linux_host_current_cpu_id() }
    }

    fn init_percpu() {
        ax_percpu::init_percpu_reg(unsafe { axvisor_linux_host_current_cpu_id() });
        unsafe { axvisor_linux_host_init_percpu() };
    }

    fn release_host_filesystems() -> ax_errno::AxResult {
        let ret = unsafe { axvisor_linux_host_release_host_filesystems() };
        if ret == 0 {
            Ok(())
        } else {
            Err(ax_err_type!(BadState, "failed to release host resources"))
        }
    }

    #[cfg(feature = "shell")]
    fn exit(exit_code: i32) -> ! {
        unsafe { axvisor_linux_host_exit(exit_code) }
    }

    #[cfg(feature = "shell")]
    fn emerg_write_bytes(bytes: &[u8]) {
        unsafe { axvisor_linux_host_emerg_write_bytes(bytes.as_ptr(), bytes.len()) };
    }
}

#[api_impl]
impl console::ConsoleIf for ConsoleIfImpl {
    fn write_bytes(bytes: &[u8]) {
        unsafe { axvisor_linux_guest_console_write_bytes(bytes.as_ptr(), bytes.len()) };
    }

    fn read_bytes(bytes: &mut [u8]) -> usize {
        unsafe { axvisor_linux_console_read_bytes(bytes.as_mut_ptr(), bytes.len()) }
    }
}

#[api_impl]
impl time::TimeIf for TimeIfImpl {
    fn current_time_nanos() -> time::Nanos {
        unsafe { axvisor_linux_time_current_time_nanos() }
    }

    fn set_oneshot_timer(deadline: time::TimeValue) {
        let nanos = deadline.as_nanos().min(u64::MAX as u128) as u64;
        unsafe { axvisor_linux_time_set_oneshot_timer(nanos) };
    }
}

#[api_impl]
impl sync::SyncIf for SyncIfImpl {
    fn create_wait_queue() -> usize {
        unsafe { axvisor_linux_sync_create_wait_queue() }
    }

    fn destroy_wait_queue(queue: usize) {
        unsafe { axvisor_linux_sync_destroy_wait_queue(queue) };
    }

    fn wait_queue_wait(queue: usize) {
        unsafe { axvisor_linux_sync_wait_queue_wait(queue) };
    }

    fn wait_queue_wait_until(queue: usize, condition: Box<dyn Fn() -> bool + Send + 'static>) {
        let condition = Box::new(HostWaitCondition(condition));
        let raw = Box::into_raw(condition);
        unsafe {
            axvisor_linux_sync_wait_queue_wait_until(
                queue,
                raw.cast::<c_void>(),
                host_wait_condition_trampoline,
            );
            drop(Box::from_raw(raw));
        }
    }

    fn wait_queue_wake_one(queue: usize) {
        unsafe { axvisor_linux_sync_wait_queue_wake_one(queue) };
    }

    fn wait_queue_wake_all(queue: usize) {
        unsafe { axvisor_linux_sync_wait_queue_wake_all(queue) };
    }
}

#[api_impl]
impl task::TaskIf for TaskIfImpl {
    fn spawn_task_raw(
        options: task::TaskOptions,
        entry: Box<dyn FnOnce() + Send + 'static>,
    ) -> task::TaskHandle {
        let raw_name = options.name.as_bytes();
        let entry = Box::new(HostTaskEntry(entry));
        let entry_ctx = Box::into_raw(entry).cast::<c_void>();
        let handle_raw = unsafe {
            axvisor_linux_task_spawn_raw(
                raw_name.as_ptr(),
                raw_name.len(),
                options.stack_size,
                options.cpu_set.is_some(),
                options.cpu_set.unwrap_or(0),
                entry_ctx,
                host_task_entry_trampoline,
            )
        };
        if handle_raw == 0 {
            // SAFETY: the host did not accept ownership when spawn failed.
            unsafe { drop(Box::from_raw(entry_ctx.cast::<HostTaskEntry>())) };
        }
        task::TaskHandle::from_raw(handle_raw)
    }

    fn join_task(task: task::TaskHandle) {
        unsafe { axvisor_linux_task_join(task.as_raw()) };
    }

    fn current_task() -> Option<task::TaskHandle> {
        let raw = unsafe { axvisor_linux_task_current() };
        (raw != 0).then_some(task::TaskHandle::from_raw(raw))
    }

    fn yield_now() {
        unsafe { axvisor_linux_task_yield_now() };
    }
}

#[api_impl]
impl irq::IrqIf for IrqIfImpl {
    fn handle_irq(vector: usize) -> bool {
        unsafe { axvisor_linux_irq_handle(vector) }
    }

    fn register_irq_handler(vector: usize, handler: irq::IrqHandler) -> bool {
        let raw = Box::into_raw(Box::new(HostIrqHandler(handler)));
        let ok = unsafe {
            axvisor_linux_irq_register(vector, raw.cast::<c_void>(), host_irq_handler_trampoline)
        };
        if !ok {
            // SAFETY: the host did not accept ownership when registration failed.
            unsafe { drop(Box::from_raw(raw)) };
        }
        ok
    }
}

#[api_impl]
impl memory::MemoryIf for MemoryIfImpl {
    fn alloc_frame() -> Option<PhysAddr> {
        let raw = unsafe { axvisor_linux_memory_alloc_frame() };
        (raw != 0).then_some(PhysAddr::from_usize(raw as usize))
    }

    fn dealloc_frame(addr: PhysAddr) {
        unsafe { axvisor_linux_memory_dealloc_frame(addr.as_usize() as u64) };
    }

    fn phys_to_virt(addr: PhysAddr) -> VirtAddr {
        VirtAddr::from_usize(unsafe { axvisor_linux_memory_phys_to_virt(addr.as_usize() as u64) as usize })
    }

    fn virt_to_phys(addr: VirtAddr) -> PhysAddr {
        PhysAddr::from_usize(unsafe { axvisor_linux_memory_virt_to_phys(addr.as_usize() as u64) as usize })
    }
}

#[unsafe(no_mangle)]
pub extern "Rust" fn axvisor_linux_bridge_mmio_read32(addr: PhysAddr) -> u32 {
    unsafe { axvisor_linux_memory_mmio_read32(addr.as_usize() as u64) }
}

#[unsafe(no_mangle)]
pub extern "Rust" fn axvisor_linux_bridge_mmio_write32(addr: PhysAddr, value: u32) {
    unsafe { axvisor_linux_memory_mmio_write32(addr.as_usize() as u64, value) };
}

#[unsafe(no_mangle)]
pub extern "Rust" fn axvisor_linux_bridge_complete_passthrough_irq(irq_id: usize) {
    unsafe { axvisor_linux_riscv_plic_complete_passthrough_irq(irq_id) };
}

#[unsafe(no_mangle)]
pub extern "Rust" fn axvisor_linux_bridge_register_guest_ram(base_hpa: usize, length: usize) -> bool {
    unsafe { axvisor_linux_memory_register_guest_ram(base_hpa as u64, length as u64) }
}

#[unsafe(no_mangle)]
pub extern "Rust" fn axvisor_linux_bridge_register_passthrough_device(
    vm_id: usize,
    base_hpa: usize,
    length: usize,
    irq_id: usize,
) -> bool {
    unsafe {
        axvisor_linux_host_register_passthrough_device(
            vm_id,
            base_hpa as u64,
            length as u64,
            irq_id,
        )
    }
}

#[cfg(axvisor_host_riscv64)]
#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_handle_irq(vector: usize) -> bool {
    unsafe { axvisor_linux_irq_handle(vector) }
}

#[api_impl]
impl api_arch::ArchIf for ArchIfImpl {
    #[cfg(axvisor_host_riscv64)]
    fn host_fdt_bytes() -> Option<&'static [u8]> {
        let vaddr = unsafe { axvisor_linux_arch_host_fdt_vaddr() };
        let size = unsafe { axvisor_linux_arch_host_fdt_size() };
        if vaddr == 0 || size == 0 {
            return None;
        }
        Some(unsafe { core::slice::from_raw_parts(vaddr as *const u8, size) })
    }

    fn host_tsc_frequency_mhz() -> Option<u32> {
        let mhz = unsafe { axvisor_adapter_host_tsc_frequency_mhz() };
        if mhz == 0 { None } else { Some(mhz) }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_boot_run() {
    axvisor_core::boot::run();
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_timer_check_events() {
    axvisor_core::vmm::timer::check_events();
    axvisor_core::vmm::vcpus::notify_all_registered_vcpus();
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_inject_current_interrupt(irq_id: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        let _ = irq_id;
        return false;
    }

    #[cfg(target_arch = "riscv64")]
    {
        return axvisor_core::arch::riscv64::inject_current_interrupt(irq_id);
    }

    #[allow(unreachable_code)]
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_inject_interrupt(vm_id: usize, irq_id: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        let _ = (vm_id, irq_id);
        return false;
    }

    #[cfg(target_arch = "riscv64")]
    {
        return axvisor_core::arch::riscv64::inject_interrupt(vm_id, irq_id);
    }

    #[allow(unreachable_code)]
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_inject_x86_gsi(vm_id: usize, gsi: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        return axvisor_core::vmm::with_vm(vm_id, |vm| vm.inject_x86_gsi(gsi))
            .is_some_and(|ret| ret.is_ok());
    }

    #[allow(unreachable_code)]
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_mark_x86_gsi_pending(vm_id: usize, gsi: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        return axvisor_core::vmm::devices::x86::mark_passthrough_gsi_pending(vm_id, gsi);
    }

    #[allow(unreachable_code)]
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_register_x86_passthrough_gsi(gsi: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        return axvisor_core::vmm::devices::x86::register_passthrough_gsi(gsi);
    }

    #[allow(unreachable_code)]
    false
}

#[cfg(target_arch = "riscv64")]
#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_inject_current_interrupt_riscv64(irq_id: usize) -> bool {
    axvisor_core::arch::riscv64::inject_current_interrupt(irq_id)
}

#[cfg(target_arch = "riscv64")]
#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_inject_interrupt_riscv64(vm_id: usize, irq_id: usize) -> bool {
    axvisor_core::arch::riscv64::inject_interrupt(vm_id, irq_id)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_bridge_current_vm_id() -> usize {
    axvisor_core::context::try_current_vcpu_context()
        .map(|context| context.vm_id)
        .unwrap_or(usize::MAX)
}

#[cfg(target_arch = "x86_64")]
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
