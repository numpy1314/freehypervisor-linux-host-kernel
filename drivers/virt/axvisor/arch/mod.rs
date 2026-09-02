// SPDX-License-Identifier: GPL-2.0

#[cfg(any(target_arch = "riscv64", target_arch = "riscv32"))]
mod riscv64;

#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(any(target_arch = "riscv64", target_arch = "riscv32"))]
pub(crate) use riscv64::{
    dispatch_external_irq,
    host_fdt_size,
    host_fdt_vaddr,
    host_tsc_frequency_mhz,
    init_backend,
    init_percpu,
    is_supervisor_external_vector,
    last_percpu_cpu_id,
    percpu_ready,
    register_irq_vector,
    register_timer_bridge,
    riscv_supervisor_external_vector,
    set_oneshot_timer,
    timer_fire_count,
};

#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{
    dispatch_external_irq,
    host_fdt_size,
    host_fdt_vaddr,
    host_tsc_frequency_mhz,
    init_backend,
    init_percpu,
    last_percpu_cpu_id,
    percpu_ready,
    register_irq_vector,
    register_timer_bridge,
    set_oneshot_timer,
    timer_fire_count,
};

#[cfg(target_arch = "x86_64")]
pub(crate) fn riscv_supervisor_external_vector() -> usize {
    usize::MAX
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn is_supervisor_external_vector(_vector: usize) -> bool {
    false
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn init_backend() {}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn register_timer_bridge(_handler: fn(usize, u64)) {}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn init_percpu(_cpu_id: usize) -> bool {
    false
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn set_oneshot_timer(_deadline_nanos: u64) {}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn timer_fire_count() -> u64 {
    0
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn percpu_ready() -> bool {
    false
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn last_percpu_cpu_id() -> u64 {
    u64::MAX
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn dispatch_external_irq(_vector: usize) -> bool {
    false
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn register_irq_vector(_vector: usize) -> bool {
    true
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn riscv_supervisor_external_vector() -> usize {
    usize::MAX
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn is_supervisor_external_vector(_vector: usize) -> bool {
    false
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn host_fdt_vaddr() -> u64 {
    0
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn host_fdt_size() -> usize {
    0
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "riscv32", target_arch = "x86_64")))]
pub(crate) fn host_tsc_frequency_mhz() -> u32 {
    0
}
