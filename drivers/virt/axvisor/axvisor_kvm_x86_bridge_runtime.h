/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef _AXVISOR_KVM_X86_BRIDGE_RUNTIME_H
#define _AXVISOR_KVM_X86_BRIDGE_RUNTIME_H

#include <linux/types.h>

void *axvisor_kvm_x86_bridge_alloc(size_t size, size_t align);
void *axvisor_kvm_x86_bridge_realloc(void *ptr, size_t new_size, size_t align);
void axvisor_kvm_x86_bridge_dealloc(void *ptr, size_t align);
void axvisor_kvm_x86_bridge_log(const u8 *bytes, size_t len);
size_t axvisor_kvm_x86_bridge_get_cpu_num(void);
size_t axvisor_kvm_x86_bridge_current_cpu_id(void);
size_t axvisor_kvm_x86_bridge_current_task_id(void);
void axvisor_kvm_x86_bridge_migrate_disable(void);
void axvisor_kvm_x86_bridge_migrate_enable(void);
u64 axvisor_kvm_x86_bridge_current_time_nanos(void);
u64 axvisor_kvm_x86_bridge_alloc_frame(void);
void axvisor_kvm_x86_bridge_dealloc_frame(u64 paddr);
u64 axvisor_kvm_x86_bridge_phys_to_virt(u64 paddr);
u64 axvisor_kvm_x86_bridge_virt_to_phys(u64 vaddr);
void axvisor_kvm_x86_bridge_yield_now(void);
void axvisor_kvm_x86_bridge_park_now(void);
void axvisor_kvm_x86_bridge_schedule_now(void);
int axvisor_kvm_x86_bridge_cond_resched(void);
int axvisor_kvm_x86_bridge_guest_fpu_begin(void);
void axvisor_kvm_x86_bridge_guest_fpu_end(void);

#endif
