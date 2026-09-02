// SPDX-License-Identifier: GPL-2.0-only
/*
 * Built-in AxVisor backend bridge for the KVM ABI provider.
 *
 * This C layer owns the Linux-facing backend ops table. The Rust side owns
 * AxVM/vCPU state and provides the axvisor_kvm_rs_* symbols when it is linked
 * into axvisor_kvm.ko.
 */

#define pr_fmt(fmt) "axvisor_kvm_axvisor_backend: " fmt

#include <linux/errno.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/printk.h>
#include <linux/sched.h>
#include <linux/types.h>

#include "axvisor_kvm_backend.h"

extern int axvisor_kvm_rs_backend_init(void) __weak;
extern void axvisor_kvm_rs_backend_exit(void) __weak;
extern int axvisor_kvm_rs_create_vm(u64 *backend_vm) __weak;
extern void axvisor_kvm_rs_destroy_vm(u64 backend_vm) __weak;
extern int axvisor_kvm_rs_set_vm_state(
	u64 backend_vm, u32 version, u32 arch, u32 irqchip_created,
	u32 pit_created, u32 pit_flags, u32 tss_addr,
	u64 identity_map_addr, u32 nr_irqchips,
	const u64 *ioapic_redirtbl, u32 ioapic_redirtbl_count) __weak;
extern int axvisor_kvm_rs_map_page(u64 backend_vm, u64 gpa, u64 hpa,
				   u32 flags) __weak;
extern int axvisor_kvm_rs_map_page_nolog(u64 backend_vm, u64 gpa, u64 hpa,
					 u32 flags) __weak;
extern int axvisor_kvm_rs_unmap_range(u64 backend_vm, u64 gpa,
				      u64 size) __weak;
extern int axvisor_kvm_rs_create_vcpu(u64 backend_vm, u32 vcpu_id,
				      u64 *backend_vcpu) __weak;
extern void axvisor_kvm_rs_destroy_vcpu(u64 backend_vcpu) __weak;
extern int axvisor_kvm_rs_set_vcpu_state(
	u64 backend_vcpu, u32 version, u32 arch, u64 rip, u64 rsp,
	u64 rflags, u64 cr0, u64 cr3, u64 cr4, u64 efer,
	u64 apic_base, u64 xcr0, u32 cpuid_nent, u32 nmsrs,
	u32 tsc_khz) __weak;
extern int axvisor_kvm_rs_set_vcpu_regs(
	u64 backend_vcpu, u64 rax, u64 rbx, u64 rcx, u64 rdx, u64 rsi,
	u64 rdi, u64 rsp, u64 rbp, u64 r8, u64 r9, u64 r10, u64 r11,
	u64 r12, u64 r13, u64 r14, u64 r15, u64 rip, u64 rflags) __weak;
extern int axvisor_kvm_rs_set_vcpu_sregs_control(
	u64 backend_vcpu, u64 cr0, u64 cr2, u64 cr3, u64 cr4, u64 cr8,
	u64 efer, u64 apic_base) __weak;
extern int axvisor_kvm_rs_set_vcpu_segment(
	u64 backend_vcpu, u32 segment_id, u64 base, u32 limit, u32 selector,
	u32 type, u32 present, u32 dpl, u32 db, u32 s, u32 l, u32 g,
	u32 avl, u32 unusable) __weak;
extern int axvisor_kvm_rs_set_vcpu_dtable(u64 backend_vcpu, u32 table_id,
					  u64 base, u32 limit) __weak;
extern int axvisor_kvm_rs_set_vcpu_cpuid_entry(
	u64 backend_vcpu, u32 entry_index, u32 function, u32 index, u32 flags,
	u32 eax, u32 ebx, u32 ecx, u32 edx) __weak;
extern int axvisor_kvm_rs_set_vcpu_msr_entry(u64 backend_vcpu,
					     u32 entry_index, u32 index,
					     u64 data) __weak;
extern int axvisor_kvm_rs_set_vcpu_fpu(
	u64 backend_vcpu, u32 fcw, u32 fsw, u32 ftwx, u32 last_opcode,
	u64 last_ip, u64 last_dp, u32 mxcsr, const u8 *fpr, u32 fpr_len,
	const u8 *xmm, u32 xmm_len) __weak;
extern int axvisor_kvm_rs_set_vcpu_xsave_legacy(u64 backend_vcpu,
						const u32 *region,
						u32 region_u32s) __weak;
extern int axvisor_kvm_rs_boot_vm(u64 backend_vm) __weak;
extern int axvisor_kvm_rs_run_vcpu(u64 backend_vcpu, u32 *reason, u32 *width,
				   u64 *addr, u64 *data,
				   u64 *hardware_entry_failure_reason) __weak;
extern int axvisor_kvm_rs_complete_mmio_read(u64 backend_vcpu,
					     const void *data, u32 len) __weak;
extern int axvisor_kvm_rs_complete_io_read(u64 backend_vcpu, const void *data,
					   u32 len) __weak;
extern int axvisor_kvm_rs_inject_irq(u64 backend_vm, u32 gsi) __weak;

static bool axvisor_kvm_axvisor_backend_registered;
static DEFINE_MUTEX(axvisor_kvm_boot_mutex);

static bool axvisor_kvm_rs_backend_available(void)
{
	return axvisor_kvm_rs_backend_init && axvisor_kvm_rs_backend_exit &&
	       axvisor_kvm_rs_create_vm && axvisor_kvm_rs_destroy_vm &&
	       axvisor_kvm_rs_map_page && axvisor_kvm_rs_map_page_nolog &&
	       axvisor_kvm_rs_unmap_range &&
	       axvisor_kvm_rs_create_vcpu && axvisor_kvm_rs_destroy_vcpu &&
	       axvisor_kvm_rs_boot_vm && axvisor_kvm_rs_run_vcpu;
}

static int axvisor_backend_create_vm(u64 *backend_vm)
{
	return axvisor_kvm_rs_create_vm(backend_vm);
}

static void axvisor_backend_destroy_vm(u64 backend_vm)
{
	axvisor_kvm_rs_destroy_vm(backend_vm);
}

static int axvisor_backend_set_vm_state(
	u64 backend_vm, const struct axkvm_backend_vm_state *state)
{
	const u64 *ioapic_redirtbl = NULL;
	u32 ioapic_redirtbl_count = 0;

	if (!axvisor_kvm_rs_set_vm_state)
		return -EOPNOTSUPP;
	if (!state)
		return -EINVAL;

#ifdef CONFIG_X86_64
	if (state->irqchips && state->nr_irqchips > KVM_IRQCHIP_IOAPIC) {
		ioapic_redirtbl =
			&state->irqchips[KVM_IRQCHIP_IOAPIC].chip.ioapic.redirtbl[0].bits;
		ioapic_redirtbl_count = KVM_IOAPIC_NUM_PINS;
	}
#endif

	return axvisor_kvm_rs_set_vm_state(
		backend_vm, state->version, state->arch,
		state->irqchip_created ? 1 : 0, state->pit_created ? 1 : 0,
		state->pit_flags, state->tss_addr, state->identity_map_addr,
		state->nr_irqchips, ioapic_redirtbl, ioapic_redirtbl_count);
}

static int axvisor_backend_map_page(u64 backend_vm, u64 gpa, u64 hpa,
				    u32 flags)
{
	return axvisor_kvm_rs_map_page(backend_vm, gpa, hpa, flags);
}

static int axvisor_backend_map_page_nolog(u64 backend_vm, u64 gpa, u64 hpa,
					  u32 flags)
{
	return axvisor_kvm_rs_map_page_nolog(backend_vm, gpa, hpa, flags);
}

static int axvisor_backend_unmap_range(u64 backend_vm, u64 gpa, u64 size)
{
	return axvisor_kvm_rs_unmap_range(backend_vm, gpa, size);
}

static int axvisor_backend_create_vcpu(u64 backend_vm, u32 vcpu_id,
				       u64 *backend_vcpu)
{
	return axvisor_kvm_rs_create_vcpu(backend_vm, vcpu_id, backend_vcpu);
}

static void axvisor_backend_destroy_vcpu(u64 backend_vcpu)
{
	axvisor_kvm_rs_destroy_vcpu(backend_vcpu);
}

static int axvisor_backend_set_vcpu_state(
	u64 backend_vcpu, const struct axkvm_backend_vcpu_state *state)
{
	u32 i;
	int ret;

	if (!axvisor_kvm_rs_set_vcpu_state)
		return -EOPNOTSUPP;
	if (!state)
		return -EINVAL;

	ret = axvisor_kvm_rs_set_vcpu_state(
		backend_vcpu, state->version, state->arch, state->rip,
		state->rsp, state->rflags, state->cr0, state->cr3,
		state->cr4, state->efer, state->apic_base,
		state->xcr0, state->cpuid_nent, state->nmsrs,
		state->tsc_khz);
	if (ret)
		goto out;

	if (axvisor_kvm_rs_set_vcpu_regs && state->regs) {
		const struct kvm_regs *regs = state->regs;

		ret = axvisor_kvm_rs_set_vcpu_regs(
			backend_vcpu, regs->rax, regs->rbx, regs->rcx,
			regs->rdx, regs->rsi, regs->rdi, regs->rsp,
			regs->rbp, regs->r8, regs->r9, regs->r10, regs->r11,
			regs->r12, regs->r13, regs->r14, regs->r15,
			regs->rip, regs->rflags);
		if (ret)
			goto out;
	}

	if (state->sregs) {
		const struct kvm_sregs *sregs = state->sregs;
		const struct kvm_segment *segments[] = {
			&sregs->cs, &sregs->ds, &sregs->es, &sregs->fs,
			&sregs->gs, &sregs->ss, &sregs->tr, &sregs->ldt,
		};

		if (axvisor_kvm_rs_set_vcpu_sregs_control) {
			ret = axvisor_kvm_rs_set_vcpu_sregs_control(
				backend_vcpu, sregs->cr0, sregs->cr2,
				sregs->cr3, sregs->cr4, sregs->cr8,
				sregs->efer, sregs->apic_base);
			if (ret)
				goto out;
		}

		if (axvisor_kvm_rs_set_vcpu_segment) {
			for (i = 0; i < ARRAY_SIZE(segments); i++) {
				const struct kvm_segment *seg = segments[i];

				ret = axvisor_kvm_rs_set_vcpu_segment(
					backend_vcpu, i, seg->base, seg->limit,
					seg->selector, seg->type, seg->present,
					seg->dpl, seg->db, seg->s, seg->l,
					seg->g, seg->avl, seg->unusable);
				if (ret)
					goto out;
			}
		}

		if (axvisor_kvm_rs_set_vcpu_dtable) {
			ret = axvisor_kvm_rs_set_vcpu_dtable(
				backend_vcpu, 0, sregs->gdt.base,
				sregs->gdt.limit);
			if (ret)
				goto out;
			ret = axvisor_kvm_rs_set_vcpu_dtable(
				backend_vcpu, 1, sregs->idt.base,
				sregs->idt.limit);
			if (ret)
				goto out;
		}
	}

	/*
	 * Keep the C/Rust ABI scalar-only: Linux UAPI structs remain on the C
	 * side, and backend bridges receive stable per-field entries.
	 */
	if (axvisor_kvm_rs_set_vcpu_cpuid_entry && state->cpuid_entries) {
		for (i = 0; i < state->cpuid_nent; i++) {
			const struct kvm_cpuid_entry2 *entry =
				&state->cpuid_entries[i];

			ret = axvisor_kvm_rs_set_vcpu_cpuid_entry(
				backend_vcpu, i, entry->function, entry->index,
				entry->flags, entry->eax, entry->ebx, entry->ecx,
				entry->edx);
			if (ret)
				goto out;
		}
	}

	if (axvisor_kvm_rs_set_vcpu_msr_entry && state->msrs) {
		for (i = 0; i < state->nmsrs; i++) {
			const struct kvm_msr_entry *entry = &state->msrs[i];

			ret = axvisor_kvm_rs_set_vcpu_msr_entry(
				backend_vcpu, i, entry->index, entry->data);
			if (ret)
				goto out;
		}
	}

	if (axvisor_kvm_rs_set_vcpu_fpu && state->fpu_valid && state->fpu) {
		const struct kvm_fpu *fpu = state->fpu;

		ret = axvisor_kvm_rs_set_vcpu_fpu(
			backend_vcpu, fpu->fcw, fpu->fsw, fpu->ftwx,
			fpu->last_opcode, fpu->last_ip, fpu->last_dp,
			fpu->mxcsr, (const u8 *)fpu->fpr, sizeof(fpu->fpr),
			(const u8 *)fpu->xmm, sizeof(fpu->xmm));
		if (ret)
			goto out;
	}

	if (axvisor_kvm_rs_set_vcpu_xsave_legacy && state->xsave_valid &&
	    state->xsave) {
		ret = axvisor_kvm_rs_set_vcpu_xsave_legacy(
			backend_vcpu, state->xsave->region,
			ARRAY_SIZE(state->xsave->region));
		if (ret)
			goto out;
	}

out:
	return ret;
}

static int axvisor_backend_boot_vm(u64 backend_vm)
{
	int ret;

	mutex_lock(&axvisor_kvm_boot_mutex);
	migrate_disable();
	ret = axvisor_kvm_rs_boot_vm(backend_vm);
	migrate_enable();
	mutex_unlock(&axvisor_kvm_boot_mutex);
	return ret;
}

static int axvisor_backend_run_vcpu(u64 backend_vcpu,
				    struct axkvm_backend_exit *exit)
{
	u64 hardware_entry_failure_reason = 0;
	u32 reason = AXKVM_BACKEND_EXIT_FAIL_ENTRY;
	u32 width = 0;
	u64 addr = 0;
	u64 data = 0;
	int ret;

	if (!exit)
		return -EINVAL;

	migrate_disable();
	ret = axvisor_kvm_rs_run_vcpu(backend_vcpu, &reason, &width, &addr,
				      &data, &hardware_entry_failure_reason);
	migrate_enable();
	if (ret)
		return ret;

	exit->reason = reason;
	exit->width = width;
	exit->addr = addr;
	exit->data = data;
	exit->hardware_entry_failure_reason = hardware_entry_failure_reason;
	return 0;
}

static int axvisor_backend_complete_mmio_read(u64 backend_vcpu,
					      const void *data, u32 len)
{
	if (!axvisor_kvm_rs_complete_mmio_read)
		return -EOPNOTSUPP;
	return axvisor_kvm_rs_complete_mmio_read(backend_vcpu, data, len);
}

static int axvisor_backend_complete_io_read(u64 backend_vcpu,
					    const void *data, u32 len)
{
	if (!axvisor_kvm_rs_complete_io_read)
		return -EOPNOTSUPP;
	return axvisor_kvm_rs_complete_io_read(backend_vcpu, data, len);
}

static int axvisor_backend_inject_irq(u64 backend_vm, u32 gsi)
{
	if (!axvisor_kvm_rs_inject_irq)
		return -EOPNOTSUPP;
	return axvisor_kvm_rs_inject_irq(backend_vm, gsi);
}

static const struct axvisor_kvm_backend_ops axvisor_backend_ops = {
	.owner = THIS_MODULE,
	.create_vm = axvisor_backend_create_vm,
	.destroy_vm = axvisor_backend_destroy_vm,
	.set_vm_state = axvisor_backend_set_vm_state,
	.map_page = axvisor_backend_map_page,
	.map_page_nolog = axvisor_backend_map_page_nolog,
	.unmap_range = axvisor_backend_unmap_range,
	.create_vcpu = axvisor_backend_create_vcpu,
	.destroy_vcpu = axvisor_backend_destroy_vcpu,
	.set_vcpu_state = axvisor_backend_set_vcpu_state,
	.boot_vm = axvisor_backend_boot_vm,
	.run_vcpu = axvisor_backend_run_vcpu,
	.complete_mmio_read = axvisor_backend_complete_mmio_read,
	.complete_io_read = axvisor_backend_complete_io_read,
	.inject_irq = axvisor_backend_inject_irq,
};

int axvisor_kvm_builtin_backend_init(void)
{
	int ret;

	if (!axvisor_kvm_rs_backend_available()) {
		pr_info("Rust AxVM backend is not linked; keeping fail-entry backend\n");
		return 0;
	}

	ret = axvisor_kvm_rs_backend_init();
	if (ret)
		return ret;

	ret = axvisor_kvm_backend_register(&axvisor_backend_ops);
	if (ret) {
		axvisor_kvm_rs_backend_exit();
		return ret;
	}

	axvisor_kvm_axvisor_backend_registered = true;
	pr_info("registered built-in AxVisor backend\n");
	return 0;
}

void axvisor_kvm_builtin_backend_exit(void)
{
	if (!axvisor_kvm_axvisor_backend_registered)
		return;

	axvisor_kvm_backend_unregister(&axvisor_backend_ops);
	axvisor_kvm_rs_backend_exit();
	axvisor_kvm_axvisor_backend_registered = false;
	pr_info("unregistered built-in AxVisor backend\n");
}
