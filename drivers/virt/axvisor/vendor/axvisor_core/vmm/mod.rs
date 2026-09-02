// SPDX-License-Identifier: GPL-2.0

pub(crate) mod timer;

unsafe extern "C" {
    fn axvisor_linux_bridge_inject_x86_gsi(vm_id: usize, gsi: usize) -> bool;
    fn axvisor_linux_bridge_mark_x86_gsi_pending(vm_id: usize, gsi: usize) -> bool;
    fn axvisor_linux_bridge_register_x86_passthrough_gsi(gsi: usize) -> bool;
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn inject_x86_gsi(vm_id: usize, gsi: usize) -> bool {
    unsafe { axvisor_linux_bridge_inject_x86_gsi(vm_id, gsi) }
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn mark_x86_gsi_pending(vm_id: usize, gsi: usize) -> bool {
    unsafe { axvisor_linux_bridge_mark_x86_gsi_pending(vm_id, gsi) }
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn register_x86_passthrough_gsi(gsi: usize) -> bool {
    unsafe { axvisor_linux_bridge_register_x86_passthrough_gsi(gsi) }
}
