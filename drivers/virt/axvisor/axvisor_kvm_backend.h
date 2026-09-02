/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef _AXVISOR_KVM_BACKEND_H
#define _AXVISOR_KVM_BACKEND_H

#include <linux/types.h>
#ifdef CONFIG_X86_64
#include <linux/kvm.h>
#endif

struct module;

#define AXKVM_BACKEND_STATE_VERSION 1
#define AXKVM_BACKEND_VCPU_STATE_VERSION AXKVM_BACKEND_STATE_VERSION
#define AXKVM_BACKEND_MAX_CPUID_ENTRIES 256
#define AXKVM_BACKEND_MAX_MSR_ENTRIES 256

enum axkvm_backend_arch {
	AXKVM_BACKEND_ARCH_UNKNOWN = 0,
	AXKVM_BACKEND_ARCH_X86_64 = 1,
};

enum axkvm_backend_exit_reason {
	AXKVM_BACKEND_EXIT_UNKNOWN = 0,
	AXKVM_BACKEND_EXIT_MMIO_READ,
	AXKVM_BACKEND_EXIT_MMIO_WRITE,
	AXKVM_BACKEND_EXIT_IO_READ,
	AXKVM_BACKEND_EXIT_IO_WRITE,
	AXKVM_BACKEND_EXIT_HLT,
	AXKVM_BACKEND_EXIT_SHUTDOWN,
	AXKVM_BACKEND_EXIT_FAIL_ENTRY,
	AXKVM_BACKEND_EXIT_INTERNAL_ERROR,
	AXKVM_BACKEND_EXIT_CPU_UP,
};

struct axkvm_backend_exit {
	u32 reason;
	u32 width;
	u64 addr;
	u64 data;
	u64 hardware_entry_failure_reason;
};

struct axkvm_backend_vm_state {
	u32 version;
	u32 arch;
#ifdef CONFIG_X86_64
	/*
	 * Pointer fields are valid only for the duration of set_vm_state().
	 * Backends must copy any state that needs to survive the callback.
	 */
	bool irqchip_created;
	bool pit_created;
	u32 pit_flags;
	u32 tss_addr;
	u64 identity_map_addr;
	const struct kvm_clock_data *clock;
	const struct kvm_irqchip *irqchips;
	u32 nr_irqchips;
	const struct kvm_pit_state2 *pit_state;
#endif
};

struct axkvm_backend_vcpu_state {
	u32 version;
	u32 arch;
	u64 rip;
	u64 rsp;
	u64 rflags;
	u64 cr0;
	u64 cr3;
	u64 cr4;
	u64 efer;
	u64 apic_base;
	u64 xcr0;
#ifdef CONFIG_X86_64
	/*
	 * Pointer fields are valid only for the duration of set_vcpu_state().
	 * Backends must copy any state that needs to survive the callback.
	 */
	const struct kvm_regs *regs;
	const struct kvm_sregs *sregs;
	bool fpu_valid;
	const struct kvm_fpu *fpu;
	const struct kvm_lapic_state *lapic;
	const struct kvm_mp_state *mp_state;
	const struct kvm_debugregs *debugregs;
	bool xsave_valid;
	const struct kvm_xsave *xsave;
	const struct kvm_xcrs *xcrs;
	const struct kvm_vcpu_events *events;
	const struct kvm_cpuid_entry2 *cpuid_entries;
	u32 cpuid_nent;
	const struct kvm_msr_entry *msrs;
	u32 nmsrs;
	u32 tsc_khz;
#endif
};

struct axvisor_kvm_backend_ops {
	struct module *owner;
	int (*create_vm)(u64 *backend_vm);
	void (*destroy_vm)(u64 backend_vm);
	int (*set_vm_state)(u64 backend_vm,
			    const struct axkvm_backend_vm_state *state);
	int (*map_page)(u64 backend_vm, u64 gpa, u64 hpa, u32 flags);
	/*
	 * Same as map_page but does NOT record the mapping in the backend's
	 * replay table. Used by the lazy on-demand fault-in path where the
	 * number of pages (a huge sparse slot, e.g. gvisor's 8 GiB) can exceed
	 * the bounded replay table; these pages are faulted in after boot so
	 * they never need replay on backend re-init.
	 */
	int (*map_page_nolog)(u64 backend_vm, u64 gpa, u64 hpa, u32 flags);
	int (*unmap_range)(u64 backend_vm, u64 gpa, u64 size);
	int (*create_vcpu)(u64 backend_vm, u32 vcpu_id, u64 *backend_vcpu);
	void (*destroy_vcpu)(u64 backend_vcpu);
	int (*set_vcpu_state)(u64 backend_vcpu,
			      const struct axkvm_backend_vcpu_state *state);
	int (*boot_vm)(u64 backend_vm);
	int (*run_vcpu)(u64 backend_vcpu, struct axkvm_backend_exit *exit);
	int (*complete_mmio_read)(u64 backend_vcpu, const void *data, u32 len);
	int (*complete_io_read)(u64 backend_vcpu, const void *data, u32 len);
	int (*inject_irq)(u64 backend_vm, u32 gsi);
};

int axvisor_kvm_backend_register(const struct axvisor_kvm_backend_ops *ops);
void axvisor_kvm_backend_unregister(const struct axvisor_kvm_backend_ops *ops);

int axvisor_kvm_backend_create_vm(u64 *backend_vm);
void axvisor_kvm_backend_destroy_vm(u64 backend_vm);
int axvisor_kvm_backend_set_vm_state(u64 backend_vm,
				     const struct axkvm_backend_vm_state *state);
int axvisor_kvm_backend_map_page(u64 backend_vm, u64 gpa, u64 hpa, u32 flags);
int axvisor_kvm_backend_map_page_nolog(u64 backend_vm, u64 gpa, u64 hpa,
				       u32 flags);
int axvisor_kvm_backend_unmap_range(u64 backend_vm, u64 gpa, u64 size);
int axvisor_kvm_backend_create_vcpu(u64 backend_vm, u32 vcpu_id,
				    u64 *backend_vcpu);
void axvisor_kvm_backend_destroy_vcpu(u64 backend_vcpu);
int axvisor_kvm_backend_set_vcpu_state(u64 backend_vcpu,
				       const struct axkvm_backend_vcpu_state *state);
int axvisor_kvm_backend_boot_vm(u64 backend_vm);
int axvisor_kvm_backend_run_vcpu(u64 backend_vcpu,
				 struct axkvm_backend_exit *exit);
int axvisor_kvm_backend_complete_mmio_read(u64 backend_vcpu,
					   const void *data, u32 len);
int axvisor_kvm_backend_complete_io_read(u64 backend_vcpu,
					 const void *data, u32 len);
int axvisor_kvm_backend_inject_irq(u64 backend_vm, u32 gsi);

int axvisor_kvm_builtin_backend_init(void);
void axvisor_kvm_builtin_backend_exit(void);

#endif
