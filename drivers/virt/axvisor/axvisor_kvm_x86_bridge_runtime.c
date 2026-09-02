// SPDX-License-Identifier: GPL-2.0-only
/*
 * Runtime helpers for the no_std x86 AxVisor KVM bridge.
 *
 * The Rust bridge is built as AxVisor-style no_std objects, not as Linux
 * kernel Rust objects. These helpers provide the small allocator and logging
 * surface required by those objects.
 */

#define pr_fmt(fmt) "axvisor_kvm_x86_bridge: " fmt

#include <linux/align.h>
#include <linux/cpu.h>
#include <linux/delay.h>
#include <linux/gfp.h>
#include <linux/ktime.h>
#include <linux/minmax.h>
#include <linux/mm.h>
#include <linux/printk.h>
#include <linux/sched.h>
#include <linux/smp.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/types.h>
#include <linux/vmalloc.h>
#include <asm/page.h>
#ifdef CONFIG_X86_64
#include <asm/fpu/api.h>
#endif

#include "axvisor_kvm_x86_bridge_runtime.h"

#define AXKVM_X86_BRIDGE_ALLOC_MAGIC 0x41584b564d583836ULL

struct axkvm_x86_bridge_alloc_header {
	u64 magic;
	void *raw;
	size_t size;
};

static struct axkvm_x86_bridge_alloc_header *
axkvm_x86_bridge_header_from_ptr(const void *ptr)
{
	struct axkvm_x86_bridge_alloc_header *header;

	if (!ptr)
		return NULL;

	header = (struct axkvm_x86_bridge_alloc_header *)ptr - 1;
	if (header->magic != AXKVM_X86_BRIDGE_ALLOC_MAGIC)
		return NULL;

	return header;
}

void *axvisor_kvm_x86_bridge_alloc(size_t size, size_t align)
{
	struct axkvm_x86_bridge_alloc_header *header;
	unsigned long raw_addr;
	unsigned long aligned_addr;
	size_t total;
	void *raw;

	if (!size)
		return ZERO_SIZE_PTR;

	align = max_t(size_t, align, __alignof__(*header));
	if (check_add_overflow(size, align, &total) ||
	    check_add_overflow(total, sizeof(*header), &total))
		return NULL;

	raw = kvmalloc(total, GFP_KERNEL);
	if (!raw)
		return NULL;

	raw_addr = (unsigned long)raw + sizeof(*header);
	aligned_addr = ALIGN(raw_addr, align);
	header = (struct axkvm_x86_bridge_alloc_header *)aligned_addr - 1;
	header->magic = AXKVM_X86_BRIDGE_ALLOC_MAGIC;
	header->raw = raw;
	header->size = size;

	return (void *)aligned_addr;
}

void axvisor_kvm_x86_bridge_dealloc(void *ptr, size_t align)
{
	struct axkvm_x86_bridge_alloc_header *header;

	if (!ptr || ptr == ZERO_SIZE_PTR)
		return;

	header = axkvm_x86_bridge_header_from_ptr(ptr);
	if (!header) {
		pr_err("dealloc rejected unknown ptr=%px align=%zu\n", ptr, align);
		return;
	}

	header->magic = 0;
	kvfree(header->raw);
}

void *axvisor_kvm_x86_bridge_realloc(void *ptr, size_t new_size, size_t align)
{
	struct axkvm_x86_bridge_alloc_header *old_header;
	void *new_ptr;

	if (!ptr || ptr == ZERO_SIZE_PTR)
		return axvisor_kvm_x86_bridge_alloc(new_size, align);
	if (!new_size) {
		axvisor_kvm_x86_bridge_dealloc(ptr, align);
		return ZERO_SIZE_PTR;
	}

	old_header = axkvm_x86_bridge_header_from_ptr(ptr);
	if (!old_header)
		return NULL;

	new_ptr = axvisor_kvm_x86_bridge_alloc(new_size, align);
	if (!new_ptr)
		return NULL;

	memcpy(new_ptr, ptr, min(old_header->size, new_size));
	axvisor_kvm_x86_bridge_dealloc(ptr, align);
	return new_ptr;
}

void axvisor_kvm_x86_bridge_log(const u8 *bytes, size_t len)
{
	size_t clipped = min_t(size_t, len, 256);

	if (!bytes)
		return;

	pr_err("%.*s\n", (int)clipped, bytes);
}

size_t axvisor_kvm_x86_bridge_get_cpu_num(void)
{
	return num_online_cpus();
}

size_t axvisor_kvm_x86_bridge_current_cpu_id(void)
{
	return smp_processor_id();
}

size_t axvisor_kvm_x86_bridge_current_task_id(void)
{
	return (size_t)current;
}

void axvisor_kvm_x86_bridge_migrate_disable(void)
{
	migrate_disable();
}

void axvisor_kvm_x86_bridge_migrate_enable(void)
{
	migrate_enable();
}

u64 axvisor_kvm_x86_bridge_current_time_nanos(void)
{
	return ktime_get_ns();
}

u64 axvisor_kvm_x86_bridge_alloc_frame(void)
{
	struct page *page;

	page = alloc_pages(GFP_KERNEL | __GFP_ZERO, 0);
	if (!page)
		return 0;

	return (u64)page_to_phys(page);
}

void axvisor_kvm_x86_bridge_dealloc_frame(u64 paddr)
{
	struct page *page;

	if (!paddr || !pfn_valid(PHYS_PFN(paddr)))
		return;

	page = phys_to_page((phys_addr_t)paddr);
	__free_pages(page, 0);
}

u64 axvisor_kvm_x86_bridge_phys_to_virt(u64 paddr)
{
	struct page *page;
	u64 offset;
	void *vaddr;

	if (!paddr || !pfn_valid(PHYS_PFN(paddr)))
		return 0;

	page = phys_to_page((phys_addr_t)paddr);
	vaddr = page_address(page);
	if (!vaddr)
		return 0;

	offset = paddr & (PAGE_SIZE - 1);
	return (u64)(unsigned long)vaddr + offset;
}

u64 axvisor_kvm_x86_bridge_virt_to_phys(u64 vaddr)
{
	void *ptr;

	if (!vaddr)
		return 0;

	ptr = (void *)(unsigned long)vaddr;
	if (!virt_addr_valid(ptr))
		return 0;

	return (u64)__pa(ptr);
}

void axvisor_kvm_x86_bridge_yield_now(void)
{
	yield();
}

/*
 * Truly deschedule the current vCPU thread for one tick, releasing its host
 * core so CFS can run another runnable vCPU thread. Unlike yield() -- whose
 * CFS backend yield_task_fair() is a no-op when the runqueue has only this
 * task (kernel/sched/fair.c), which is exactly the one-thread-per-core layout
 * under oversubscription -- schedule_timeout blocks this task off the runqueue.
 * This mirrors the effect of KVM's kvm_vcpu_block()/schedule() freeing the
 * pCPU for a halted or long-spinning vCPU. Interruptible so a pending signal
 * (KVM_RUN abort) still breaks out promptly.
 */
void axvisor_kvm_x86_bridge_park_now(void)
{
	schedule_timeout_interruptible(1);
}

/*
 * Voluntarily invoke the scheduler while staying TASK_RUNNING. Unlike
 * park_now()'s schedule_timeout_interruptible(1) -- which sets
 * TASK_INTERRUPTIBLE and removes this vCPU thread from the runnable set for a
 * full jiffy (observed to make a parked AP never resume useful work under
 * oversubscription) -- schedule() enters __schedule() unconditionally
 * (kernel/sched/core.c __schedule_loop, SM_NONE) with the task still runnable,
 * so it is re-enqueued and immediately eligible again. Unlike cond_resched()
 * -- gated by should_resched()/TIF_NEED_RESCHED (core.c:7694), a no-op on a
 * lone-task runqueue (nr_running==1, the one-thread-per-core oversubscription
 * layout) -- schedule() always runs the picker.
 *
 * This is the KVM-faithful primitive for the between-guest-entries reschedule
 * point: KVM's vcpu_run outer loop runs preemption-enabled and hits a real
 * schedule() on _TIF_NEED_RESCHED after every VM-exit (kernel/entry/virt.c:13,
 * arch/x86/kvm/x86.c:11793); frequent exits are guaranteed by external-
 * interrupt exiting / the preemption timer, so every vCPU thread -- BSP and
 * busy-polling AP alike -- periodically yields the picker. kvm_vcpu_on_spin()
 * itself never blocks or sleeps (virt/kvm/kvm_main.c:3959): it only tries
 * yield_to() a few times and returns. We mirror that: keep the thread runnable
 * and let CFS co-schedule the sibling that must run (the BSP driving
 * cpuhp_bp_sync_alive) onto a freed core, rather than block-parking the AP.
 */
void axvisor_kvm_x86_bridge_schedule_now(void)
{
	schedule();
}

/*
 * Honor a pending host reschedule WITHOUT leaving the runqueue. cond_resched()
 * yields the CPU only when TIF_NEED_RESCHED is set (the host scheduler tick /
 * load balancer wants to run another task here) and returns as soon as this
 * task is picked again -- the vCPU thread stays RUNNABLE throughout. This is
 * the KVM-faithful oversubscription primitive: KVM keeps every vCPU thread
 * runnable and relies on CFS time-slicing (need_resched from the host tick) to
 * spread N vCPU threads across M<N cores, rather than blocking a spinning vCPU
 * off the runqueue (which removes it from CFS's balancing set). Unlike yield()
 * -- a no-op when the runqueue holds only this task, i.e. the one-thread-per-
 * core layout under oversubscription -- and unlike park_now()'s
 * schedule_timeout() block, cond_resched() lets the busy-polling AP (e.g. in
 * cpuhp_ap_sync_alive) give its core to a sibling only when the scheduler
 * actually asked, then immediately resume. Returns 1 if it rescheduled.
 */
int axvisor_kvm_x86_bridge_cond_resched(void)
{
	return cond_resched();
}

int axvisor_kvm_x86_bridge_guest_fpu_begin(void)
{
#ifdef CONFIG_X86_64
	if (!irq_fpu_usable())
		return -EBUSY;

	kernel_fpu_begin_mask(KFPU_387 | KFPU_MXCSR);
#endif
	return 0;
}

void axvisor_kvm_x86_bridge_guest_fpu_end(void)
{
#ifdef CONFIG_X86_64
	kernel_fpu_end();
#endif
}
