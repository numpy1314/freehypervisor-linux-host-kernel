// SPDX-License-Identifier: GPL-2.0-only
/*
 * AxVisor KVM ABI compatibility provider.
 *
 * This file starts the /dev/kvm object model required by userspace VMMs.
 * It intentionally stays separate from axvisor_adapter.ko so the existing
 * RISC-V Linux-hosted AxVisor path is not changed while the KVM ABI provider
 * is brought up phase by phase.
 */

#define pr_fmt(fmt) "axvisor_kvm: " fmt

#include <linux/anon_inodes.h>
#include <linux/atomic.h>
#include <linux/bits.h>
#include <linux/bitops.h>
#include <linux/errno.h>
#include <linux/eventfd.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/hrtimer.h>
#include <linux/bitmap.h>
#include <linux/kref.h>
#include <linux/kthread.h>
#include <linux/kvm.h>
#include <linux/ktime.h>
#include <linux/delay.h>
#include <linux/log2.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/pid.h>
#include <linux/poll.h>
#include <linux/printk.h>
#include <linux/sched.h>
#include <uapi/linux/sched/types.h>
#include <linux/sched/signal.h>
#include <linux/sched/stat.h>
#include <linux/sched/task.h>
#include <linux/signal.h>
#include <linux/timekeeping.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/uio.h>
#include <linux/vmalloc.h>
#include <linux/wait.h>
#include <linux/workqueue.h>
#include <asm/rwonce.h>
#include <asm/io.h>
#ifdef CONFIG_X86_64
#include <asm/msr-index.h>
#include <asm/msr.h>
#include <asm/pvclock-abi.h>
#endif

#include "axvisor_kvm_backend.h"

#define AXKVM_MAX_MEMSLOTS 32
#define AXKVM_MAX_VCPUS 32
#define AXKVM_MAX_IOEVENTS 128
#define AXKVM_MAX_IRQFDS 128
#define AXKVM_MAX_IRQ_ROUTES 256
#define AXKVM_MAX_PENDING_IRQS BITS_PER_LONG
#define AXKVM_MAX_CPUID_ENTRIES 256
#define AXKVM_MAX_MSR_ENTRIES 256
#define AXKVM_MAX_BACKEND_VMS 16
#define AXKVM_DEFAULT_TSC_KHZ 3000000U

#define AXKVM_VCPU_MMAP_SIZE (2 * PAGE_SIZE)
#define AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX BIT(0)
#define AXKVM_X86_APIC_DEFAULT_PHYS_BASE 0xfee00000ULL

/*
 * kvm-clock / pvclock support. Firecracker boots the guest with tsc=unstable
 * and no TSC clocksource, and axvisor previously exposed no paravirt clock, so
 * the guest fell back to refined-jiffies -- whose global tick is advanced only
 * by per-tick timer IRQ delivery. Under vCPU oversubscription the tick owner
 * gets starved and guest jiffies freeze. Exposing kvm-clock lets the guest read
 * time from a TSC + shared page, immune to per-tick IRQ starvation.
 */
#define AXKVM_CPUID_SIGNATURE 0x40000000U
#define AXKVM_CPUID_FEATURES 0x40000001U
#define AXKVM_FEATURE_CLOCKSOURCE2 3
#define AXKVM_MSR_KVM_WALL_CLOCK_NEW 0x4b564d00U
#define AXKVM_MSR_KVM_SYSTEM_TIME_NEW 0x4b564d01U
#define AXKVM_KVM_SYSTEM_TIME_ENABLE 0x1ULL
#define AXKVM_PVCLOCK_TSC_STABLE_BIT 0x1U

static char *axkvm_dev_name = "kvm";
module_param_named(dev_name, axkvm_dev_name, charp, 0444);
MODULE_PARM_DESC(dev_name,
		 "misc device name; default 'kvm' registers /dev/kvm, other names use a dynamic minor for smoke testing");

/*
 * AP admission budget for SMP bringup under oversubscription (guest vCPUs >
 * host CPUs). Linux 6.x parallel bringup (CONFIG_HOTPLUG_PARALLEL) kicks every
 * AP at once; each AP must then reach cpuhp_ap_sync_alive() and report
 * SYNC_STATE_ALIVE within the boot CPU's per-AP ~10s window. With N threads
 * fairly sharing M<N cores the losers get starved past that window and spin in
 * cpuhp_ap_sync_alive() forever. We throttle how many APs are simultaneously
 * "admitted but not yet ALIVE" so each admitted AP gets enough CPU to make
 * the first half of the handshake, then admit the next in kick order. 0 == auto
 * (host CPUs - 4, reserving cores for the L1 kernel/RCU/BSP). This does NOT lower the
 * oversubscription ratio -- all APs still boot, just in ordered batches.
 */
static unsigned int axkvm_ap_admit_budget;
module_param_named(ap_admit_budget, axkvm_ap_admit_budget, uint, 0644);
MODULE_PARM_DESC(ap_admit_budget,
		 "max APs simultaneously admitted-but-not-ALIVE during SMP bringup; 0 = auto (online CPUs - 4)");

/*
 * Optional guest RIP that identifies the AP-side cpuhp_ap_sync_alive() wait
 * loop after the AP has written SYNC_STATE_ALIVE and is waiting for the boot
 * CPU to write SYNC_STATE_SHOULD_ONLINE. 0 disables RIP-based ALIVE detection.
 *
 * This is intentionally a parameter instead of a hard-coded address: the value
 * is guest-kernel-build specific. With nokaslr it can be derived from vmlinux,
 * e.g. cpuhp_ap_sync_alive+36 for the current Linux 7.1-rc6 test kernel.
 */
static unsigned long long axkvm_ap_alive_spin_rip;
module_param_named(ap_alive_spin_rip, axkvm_ap_alive_spin_rip, ullong, 0644);
MODULE_PARM_DESC(ap_alive_spin_rip,
		 "guest RIP that means an AP has reached cpuhp_ap_sync_alive ALIVE wait; 0 disables");

static void axkvm_backend_schedule_point(void)
{
	/*
	 * Reached with migration/preemption enabled (the backend run returned
	 * to C and leave_percpu() dropped migrate_disable). Use cond_resched()
	 * rather than yield(): yield()'s CFS backend is a no-op when the
	 * runqueue holds only this task (the one-thread-per-core layout under
	 * oversubscription), so it never freed a core; cond_resched() reschedules
	 * whenever the host tick/load-balancer set NEED_RESCHED, mirroring KVM's
	 * vcpu_run() honoring xfer_to_guest_mode work (need_resched) each pass.
	 */
	cond_resched();
}

#ifdef CONFIG_X86_64
struct axkvm_vm;

static DEFINE_MUTEX(axkvm_backend_vm_registry_lock);
static struct axkvm_vm *axkvm_backend_vm_registry[AXKVM_MAX_BACKEND_VMS + 1];
static DEFINE_SPINLOCK(axkvm_backend_timer_lock);
static struct hrtimer axkvm_backend_timer;
static u64 axkvm_backend_timer_deadline_ns;
static bool axkvm_backend_timer_active;

/*
 * DIAG (bounded, one-shot, hardirq-safe). Lockless raw handle to the most
 * recently created VM, published with WRITE_ONCE at creation. Read from the
 * periodic hrtimer callback (hardirq: cannot take axkvm_get_backend_vm's mutex)
 * to dump the A-vs-B CALL_FUNCTION CSD counters exactly once, ~20s in, from a
 * context that is IMMUNE to the CFS kworker starvation that silences the workfn
 * during the hang. The VM outlives the guest run, so a non-freed stale pointer
 * during the run window is safe for read-only atomic sampling. Remove after
 * diagnosis.
 */
static struct axkvm_vm *axkvm_dbg_vm;
static unsigned long axkvm_dbg_first_tick_jiffies;
static bool axkvm_dbg_ab_dumped;
static atomic64_t axkvm_dbg_periodic_cb_cnt;
static atomic64_t axkvm_dbg_periodic_kick_cnt;

/*
 * Independent periodic backend tick. The one-shot axkvm_backend_timer is a
 * low-latency wake optimization that a healthy always-running vCPU can keep
 * pushing into the future (its re-arm cancels+restarts the one-shot before it
 * expires), so it cannot be relied on for liveness. This self-forwarding
 * periodic hrtimer fires unconditionally and drives the drain-all workfn, so a
 * vCPU starved off-core under oversubscription still gets its due LAPIC ticks
 * expired/injected. Period is < the 500us guest LAPIC period so worst-case
 * extra latency is under half a tick.
 */
static struct hrtimer axkvm_backend_periodic_timer;
#define AXKVM_BACKEND_PERIODIC_NS 250000ULL
#define AXKVM_PERIODIC_KICK_DEFAULT_NS 2000000U
#define AXKVM_PERIODIC_KICK_MIN_NS 500000U
static unsigned int axkvm_periodic_kick_ns = AXKVM_PERIODIC_KICK_DEFAULT_NS;
module_param_named(periodic_kick_ns, axkvm_periodic_kick_ns, uint, 0644);
MODULE_PARM_DESC(periodic_kick_ns,
		 "oversubscription-only directed kick interval for running vCPU tasks in ns; 0 disables");

/*
 * Verbose diagnostic printk toggle. The oversubscription-era heartbeat/backstop
 * traces (halt backstop, backend timer wake, periodic alive, lazy-memslot
 * registration) flood dmesg and can crowd out other guest/host output in the
 * serial tail. Off by default; set debug_verbose=1 to re-enable. Gates only
 * diagnostic prints -- no wake/timer/fault-in logic depends on it.
 */
static bool axkvm_debug_verbose;
module_param_named(debug_verbose, axkvm_debug_verbose, bool, 0644);
MODULE_PARM_DESC(debug_verbose,
		 "enable verbose oversubscription diagnostic printk (default off)");

/*
 * KVM-style halt tuning (see the AXKVM_BACKEND_EXIT_HLT path). A brief
 * busy-poll catches an imminent wake without the block/unblock cost; the
 * block then releases the host core. KVM's kvm_vcpu_block() blocks with NO
 * timeout because every wake path issues a reliable kvm_vcpu_kick(). This
 * out-of-tree module cannot fully guarantee no missed kick, so the block
 * carries a SHORT backstop timeout: a missed kick then recovers within one
 * jiffy rather than stalling a synchronous cross-call (smp_call_function)
 * that waits on this vCPU to run. A long backstop froze early boot at the
 * first on_each_cpu() broadcast; keep it at one jiffy.
 */
#define AXKVM_HALT_POLL_ITERS 4000
#define AXKVM_HALT_BLOCK_TIMEOUT_JIFFIES 1

static int axkvm_register_backend_vm(struct axkvm_vm *vm);
static void axkvm_unregister_backend_vm(struct axkvm_vm *vm);
static enum hrtimer_restart axkvm_backend_timer_cb(struct hrtimer *timer);
static enum hrtimer_restart axkvm_backend_periodic_cb(struct hrtimer *timer);
static void axkvm_backend_timer_workfn(struct work_struct *work);
static void axkvm_wake_vcpu_in_vm(struct axkvm_vm *vm, u32 vcpu_id);
static DECLARE_WORK(axkvm_backend_timer_work, axkvm_backend_timer_workfn);
/*
 * Dedicated high-priority, unbound, mem-reclaim workqueue for the backend
 * LAPIC-timer drain. Under CPU oversubscription (more vCPU threads than host
 * CPUs) the shared system_wq gives no timeliness guarantee, and a delayed drain
 * re-starves the very vCPU whose tick we are trying to deliver. WQ_HIGHPRI +
 * WQ_UNBOUND lets the worker preempt onto any idle host CPU promptly;
 * max_active=1 serializes drains so concurrent full-table sweeps cannot pile up.
 * Falls back to system_wq if allocation fails.
 */
static struct workqueue_struct *axkvm_backend_timer_wq;
void axvisor_kvm_x86_bridge_program_timer(u64 deadline_ns);
void axvisor_kvm_x86_bridge_reprogram_timer(u64 deadline_ns);
void axvisor_kvm_x86_bridge_cancel_timer(void);
void axvisor_kvm_x86_bridge_wake_vcpu(u64 backend_vm, u32 vcpu_id);
void axvisor_kvm_x86_bridge_boost_vcpu(u64 backend_vm, u32 vcpu_id);
bool axvisor_kvm_x86_bridge_directed_yield(u64 backend_vm, u32 cur_vcpu_id);
void axvisor_kvm_x86_bridge_spin_demote(u64 backend_vm, u32 vcpu_id);
void axvisor_kvm_x86_bridge_spin_restore(u64 backend_vm, u32 vcpu_id);
int axvisor_kvm_x86_bridge_spin_park(u64 backend_vm, u32 vcpu_id);
int axvisor_kvm_x86_bridge_fault_in_gpa(u64 backend_vm, u64 gpa, u32 write);
/* DIAG (gvisor signal-interruptibility): report signal_pending() of the caller. */
int axvisor_kvm_x86_bridge_signal_pending(void);
void axvisor_kvm_x86_bridge_note_ap_alive_spin(u64 backend_vm, u32 vcpu_id,
					       u64 rip);
void axvisor_kvm_x86_bridge_pvclock_write(u64 backend_vm, u32 vcpu_id, u32 msr,
					  u64 value);
void axvisor_kvm_x86_bridge_pvclock_refresh(u64 backend_vm, u32 vcpu_id);
void axvisor_kvm_x86_bridge_expire_all_due_timers(void);

static const u32 axkvm_default_msr_indices[] = {
	MSR_IA32_TSC,
	MSR_IA32_SYSENTER_CS,
	MSR_IA32_SYSENTER_ESP,
	MSR_IA32_SYSENTER_EIP,
	MSR_STAR,
	MSR_LSTAR,
	MSR_CSTAR,
	MSR_SYSCALL_MASK,
	MSR_EFER,
	MSR_FS_BASE,
	MSR_GS_BASE,
	MSR_KERNEL_GS_BASE,
	MSR_IA32_APICBASE,
	MSR_IA32_TSC_DEADLINE,
	MSR_IA32_CR_PAT,
	MSR_IA32_MISC_ENABLE,
	MSR_IA32_SPEC_CTRL,
	MSR_IA32_ARCH_CAPABILITIES,
	AXKVM_MSR_KVM_WALL_CLOCK_NEW,
	AXKVM_MSR_KVM_SYSTEM_TIME_NEW,
};

struct axkvm_eventfd_binding {
	bool valid;
	bool irqfd;
	bool irqfd_wait_registered;
	struct eventfd_ctx *ctx;
	struct axkvm_vm *vm;
	struct work_struct irqfd_inject_work;
	wait_queue_entry_t irqfd_wait;
	poll_table irqfd_pt;
	u64 addr;
	u64 datamatch;
	u32 len;
	u32 flags;
	s32 fd;
	u32 gsi;
	s32 resamplefd;
	atomic64_t signal_count;
	atomic64_t wake_count;
	atomic64_t inject_count;
};

struct axkvm_irq_route {
	bool valid;
	u32 type;
	u32 irqchip;
	u32 pin;
};

static bool axkvm_trace_count(u64 count)
{
	return count <= 8 || is_power_of_2(count);
}

static void axkvm_normalize_x86_sregs(struct kvm_sregs *sregs)
{
	struct kvm_segment *ldt = &sregs->ldt;

	/*
	 * Linux KVM reports an absent LDTR as unusable. Firecracker starts from
	 * KVM_GET_SREGS, edits the boot segments, then writes the full struct
	 * back with KVM_SET_SREGS. Returning or accepting an all-zero usable LDT
	 * descriptor makes VMX reject VM entry as invalid guest state.
	 */
	if (!ldt->selector && !ldt->base && !ldt->limit && !ldt->type &&
	    !ldt->present)
		ldt->unusable = 1;
}

#endif

struct axkvm_memslot {
	bool valid;
	u32 slot;
	u32 flags;
	u64 guest_phys_addr;
	u64 memory_size;
	u64 userspace_addr;
	struct page **pages;
	/*
	 * Lazy fault-in tracking. pages[idx] holds the GUP-pinned struct page
	 * for ordinary anonymous/file RAM (unpinned at teardown). But VM_IO /
	 * VM_PFNMAP backing (e.g. gvisor's sentry vvar mapping identity-mapped
	 * into guest-physical) has no refcounted struct page -- it is resolved
	 * via follow_pfnmap_start() to a raw PFN and inserted straight into the
	 * backend EPT. Such pages leave pages[idx] == NULL, so pages[] alone
	 * cannot answer "is this index already mapped?". mapped[] is the
	 * authoritative per-page "installed in EPT" bitmap (covers both the
	 * pinned and the remapped case); writable[] records whether the EPT leaf
	 * was installed writable, so a later write fault on a read-only leaf can
	 * be detected instead of being silently treated as already-resolved.
	 */
	unsigned long *mapped;
	unsigned long *writable;
	unsigned long nr_pages;
};

/* Shim-internal backend map flag: install the EPT leaf read-only. Placed at
 * BIT(31) to avoid colliding with the KVM_MEM_* flag bits (LOG_DIRTY_PAGES=
 * BIT(0), READONLY=BIT(1)) that flow through the same u32. */
#define AXKVM_MAP_RDONLY BIT(31)

/*
 * SMP bringup admission state for an AP vCPU (oversubscription throttle).
 *
 *   AP_BOOT_NONE     initial / BSP; not part of the bringup handshake.
 *   AP_BOOT_KICKED   CPU_UP (INIT/SIPI) seen for this AP, queued but not yet
 *                    admitted -- its KVM_RUN thread is held so it does not race
 *                    the already-admitted batch for host CPUs.
 *   AP_BOOT_ADMITTED admitted: allowed to run and briefly nice-boosted so it
 *                    reaches SYNC_STATE_ALIVE inside the boot CPU's window.
 *   AP_BOOT_ALIVE    observed at the ALIVE wait loop; budget and AP boost have
 *                    been released because the AP is now waiting for BSP.
 *   AP_BOOT_SETTLED  observed its first HLT (guaranteed past ALIVE and fully
 *                    online). Budget may already have been released at ALIVE.
 */
enum axkvm_ap_boot_state {
	AP_BOOT_NONE = 0,
	AP_BOOT_KICKED,
	AP_BOOT_ADMITTED,
	AP_BOOT_ALIVE,
	AP_BOOT_SETTLED,
};

struct axkvm_vm {
	struct kref refcount;
	struct mutex lock;
	u64 backend_vm;
	bool backend_ready;
	bool backend_booted;
	struct axkvm_memslot memslots[AXKVM_MAX_MEMSLOTS];
	struct axkvm_vcpu *vcpus[AXKVM_MAX_VCPUS];
#ifdef CONFIG_X86_64
	bool irqchip_created;
	bool pit_created;
	u32 pit_flags;
	u32 tss_addr;
	u64 identity_map_addr;
	struct kvm_clock_data clock;
	struct kvm_irqchip irqchips[KVM_NR_IRQCHIPS];
	struct kvm_pit_state2 pit_state;
	struct axkvm_irq_route irq_routes[AXKVM_MAX_IRQ_ROUTES];
	unsigned long pending_irq_gsis;
	struct axkvm_eventfd_binding ioevents[AXKVM_MAX_IOEVENTS];
	struct axkvm_eventfd_binding irqfds[AXKVM_MAX_IRQFDS];
	unsigned int last_boosted_vcpu;	/* round-robin cursor for directed yield */
	/*
	 * The vCPU that most recently issued a CPU_UP (INIT/SIPI) to bring up an
	 * AP -- almost always the boot CPU (vcpu 0). During serial SMP bringup an
	 * AP spinning in cpuhp_ap_sync_alive() is waiting for *this* controller to
	 * advance the handshake (write SYNC_STATE_SHOULD_ONLINE), not for a random
	 * sibling. A spinning AP therefore directed-yields to the controller first.
	 * -1 == none recorded yet.
	 */
	int boot_controller_id;
	/*
	 * The AP vcpu id that the boot controller most recently SIPI'd, i.e. the
	 * AP the BSP is *currently* blocking on inside do_boot_cpu() waiting for it
	 * to report online. Linux 6.x brings APs up effectively one at a time from
	 * the BSP's point of view; if that specific AP cannot get a physical core
	 * under oversubscription the whole bringup stalls and later APs are never
	 * SIPI'd. When the BSP directed-yields we boost *this* AP first (Priority
	 * 0) rather than a random RUNNABLE sibling. -1 == none recorded yet.
	 */
	int current_bringup_target;
	/*
	 * Oversubscription spinner-park rotation. A confirmed spinner (an AP stuck
	 * in cpuhp_ap_sync_alive that is neither the BSP nor the current bringup
	 * target) is briefly blocked out of the run queue on spin_park_wq so the
	 * freed core goes through newidle-balance and pulls the starved target AP /
	 * BSP. spin_park_gen is bumped by any waker so parked threads re-evaluate.
	 * Serialised by vm->lock (writers); readers use READ_ONCE.
	 */
	wait_queue_head_t spin_park_wq;
	unsigned int spin_park_gen;
	/*
	 * AP admission throttle (see axkvm_ap_admit_budget). ap_boot_queue is the
	 * FIFO of AP vcpu ids in BSP kick order; APs must be admitted strictly in
	 * this order because the boot CPU waits on each AP by cpu number, so an
	 * out-of-order gap would start an un-admitted AP's 10s window. ap_admitted
	 * counts APs in AP_BOOT_ADMITTED (admitted but not yet ALIVE). All fields
	 * are serialised by vm->lock.
	 */
	unsigned int ap_boot_queue[AXKVM_MAX_VCPUS];
	unsigned int ap_boot_queue_head;
	unsigned int ap_boot_queue_tail;
	unsigned int ap_admitted;
	/*
	 * kvm-clock wall clock page GPA (MSR_KVM_WALL_CLOCK_NEW). VM-scoped:
	 * written once by the guest with the guest-physical address of a
	 * struct pvclock_wall_clock. 0 == not set.
	 */
	u64 pvclock_wall_clock_gpa;
	/*
	 * VM-wide kvm-clock master reference, mirroring KVM's
	 * kvm_arch::{master_cycle_now,master_kernel_ns} (arch/x86/kvm/x86.c:
	 * pvclock_update_vm_gtod_copy). A single (host_tsc, kernel_ns) pair is
	 * snapshotted once for the whole VM and EVERY vCPU derives its
	 * pvclock_vcpu_time_info from it, so all vCPU pages are mutually
	 * consistent and monotonic regardless of which host core samples or
	 * whether a vCPU is currently on-core. This is what makes it safe to
	 * assert PVCLOCK_TSC_STABLE_BIT: the guest then trusts kvm-clock and does
	 * NOT run the clocksource watchdog against it (the watchdog + cross-CPU
	 * remote reads were what marked TSC unstable and stalled RCU under
	 * oversubscription). master_valid guards the one-time capture.
	 */
	u64 pvclock_master_tsc;
	u64 pvclock_master_kernel_ns;
	bool pvclock_master_valid;
	bool pvclock_master_stable;
	/*
	 * DIAG (bounded, one-shot). Set after the A-vs-B CALL_FUNCTION CSD dump
	 * has fired once, so the periodic backstop prints per-vCPU
	 * dbg_wake/dbg_run_after_wake exactly once in the stall window (no flood).
	 * first_run_jiffies anchors the ~30s dump trigger. Remove after diagnosis.
	 */
	unsigned long dbg_first_backstop_jiffies;
	bool dbg_ab_dumped;
#endif
};

struct axkvm_vcpu {
	struct kref refcount;
	struct axkvm_vm *vm;
	unsigned int id;
	unsigned long run_pages;
	struct kvm_run *run;
	u64 backend_vcpu;
	bool backend_ready;
	bool backend_state_dirty;
	bool pending_mmio_read;
	u32 pending_mmio_read_len;
	bool pending_io_read;
	u32 pending_io_read_len;
	u32 pending_io_read_offset;
	bool signal_mask_valid;
	sigset_t signal_mask;
#ifdef CONFIG_X86_64
	struct mutex lock;
	struct kvm_regs regs;
	struct kvm_sregs sregs;
	bool fpu_valid;
	struct kvm_fpu fpu;
	struct kvm_lapic_state lapic;
	struct kvm_mp_state mp_state;
	wait_queue_head_t mp_state_wq;
	wait_queue_head_t halt_wq;
	atomic_t irq_pending_wakeup;
	/*
	 * Oversubscription halt backstop (see axkvm_backend_timer_workfn). Set to
	 * 1 immediately before this vCPU blocks in the HLT wait and cleared right
	 * after it returns. An idle NO_HZ AP that HLT-blocks has NO armed LAPIC
	 * timer, so the per-vCPU timer drain never re-kicks it; if the BSP then
	 * issues a CALL_FUNCTION IPI and spins in csd_lock_wait, the halted AP
	 * never runs flush_smp_call_function_queue and the guest deadlocks (RCU
	 * stall). Mirrors KVM, where __kvm_vcpu_kick wakes a blocked vCPU on every
	 * event; here the 250us workfn gives each halted vCPU a periodic chance to
	 * run its idle/scheduler path. Rate-limited to at most one wake per jiffy
	 * (last_halt_backstop_jiffies) so it cannot reintroduce the HLT/wake spin
	 * storm, and gated on oversubscription so 1/2/4/8/16 stay a no-op.
	 */
	atomic_t in_halt_wait;
	unsigned long last_halt_backstop_jiffies;
	struct kvm_debugregs debugregs;
	bool xsave_valid;
	struct kvm_xsave xsave;
	struct kvm_xcrs xcrs;
	struct kvm_vcpu_events events;
	/*
	 * Deferred directed-yield hint (IPI-boost). Set lock-free from the
	 * atomic guest-APIC-write injection path to (target vcpu id + 1); 0 ==
	 * none. Drained + converted into a real yield_to() at this vCPU's
	 * run-loop safe point (axkvm_vcpu_drain_boost), where scheduling is
	 * legal. yield_to() must never be called from the injection path, which
	 * runs with preemption/IRQs disabled ("scheduling while atomic").
	 */
	atomic_t boost_target;
	struct kvm_cpuid_entry2 cpuid_entries[AXKVM_MAX_CPUID_ENTRIES];
	u32 cpuid_nent;
	struct kvm_msr_entry msrs[AXKVM_MAX_MSR_ENTRIES];
	u32 nmsrs;
	u32 tsc_khz;
	u64 backend_run_calls;
	u64 backend_cpu_ups;
	struct pid *run_pid;	/* KVM_RUN thread pid, refcounted; for directed yield */
	/*
	 * Scheme-B bounded residency backstop. If L0 does not expose the VMX
	 * preemption timer to this L1 module, a busy L2 vCPU can remain in guest
	 * long enough to starve L1's timer/RCU delivery. The periodic hardirq
	 * sends a rate-limited reschedule kick to this task only under VM
	 * oversubscription; this timestamp keeps that from becoming a wake storm.
	 */
	u64 last_periodic_kick_ns;
	atomic64_t dbg_periodic_kicks;
	/*
	 * Set while this vCPU's KVM_RUN task has a temporarily lowered nice value
	 * to guarantee bounded scheduling latency during SMP bringup (see
	 * axkvm_bringup_boost/restore). Under oversubscription an AP needs only a
	 * few microseconds of CPU to write SYNC_STATE_ALIVE in cpuhp_ap_sync_alive,
	 * but must get scheduled within the boot CPU's ~10s release window; plain
	 * CFS fairness across 32 threads on 18 cores can starve it past that. The
	 * boost is bounded: it is dropped when the AP reaches ALIVE or settles and
	 * a watchdog forcibly restores it after AXKVM_BRINGUP_BOOST_MS so an AP that
	 * spins in cpuhp_ap_sync_alive() forever cannot keep nice -20 and starve the
	 * L1 kernel's own threads (e.g. rcu_preempt).
	 */
	bool bringup_boosted;
	/*
	 * Stronger, target-only bringup boost. The generic bringup_boosted flag
	 * is a CFS nice boost and may be applied to several admitted APs. The AP
	 * currently waited on by the boot controller needs a strict scheduling
	 * edge over those siblings, so it is temporarily promoted to low-priority
	 * SCHED_FIFO until it settles or the boost watchdog expires.
	 */
	bool bringup_rt_boosted;
	/*
	 * Set while this vCPU's KVM_RUN task is temporarily demoted to SCHED_IDLE
	 * because it has been confirmed spinning in a guest busy-poll (an AP that
	 * has written SYNC_STATE_ALIVE and now spins in cpuhp_ap_sync_alive waiting
	 * for the BSP to write SHOULD_ONLINE). Under oversubscription such a spinner
	 * otherwise owns an L1 core alone (nr_running==1), where yield()/yield_to()/
	 * cond_resched()/bare schedule() cannot hand the core off. SCHED_IDLE keeps
	 * it RUNNABLE (so it never leaves CFS's balancing set -- block-park froze the
	 * whole L1) yet lets ANY normal/RT task woken onto that core preempt it
	 * instantly, so the BSP (nice-boosted via axkvm_bringup_boost) or a migrated
	 * runnable sibling actually gets the core and can advance SMP bringup.
	 * Restored the moment the guest RIP leaves the spin window (real progress),
	 * on HLT/settle, and on teardown -- never via a timer/work path, which is
	 * itself starved under this wedge.
	 */
	bool spin_demoted;
	/*
	 * True while this vCPU thread is blocked in axkvm_spin_park (parked out of
	 * the run queue to free its core for a starved AP/BSP). Advisory only.
	 */
	bool spin_parked;
	/* Admission/handshake state for SMP bringup throttle; see axkvm_vm. */
	enum axkvm_ap_boot_state boot_state;
	/* AP KVM_RUN thread waits here until admitted (state != AP_BOOT_KICKED). */
	wait_queue_head_t admit_wq;
	/* Forcibly restores an over-long bringup boost; armed on admit. */
	struct delayed_work boost_watchdog;
	/*
	 * kvm-clock / pvclock (see AXKVM_CPUID_* / AXKVM_MSR_KVM_*). When the
	 * guest enables kvm-clock it writes MSR_KVM_SYSTEM_TIME_NEW with the
	 * guest-physical address of a per-vCPU struct pvclock_vcpu_time_info
	 * (bit 0 = enable). We keep the raw MSR value, cache the GPA, and bump a
	 * monotonically increasing version each time we refresh the page.
	 */
	u64 pvclock_system_time_msr;
	u64 pvclock_gpa;
	bool pvclock_enabled;
	u32 pvclock_version;
	/*
	 * DIAG (oversub CALL_FUNCTION CSD-lock A-vs-B distinction; bounded, no
	 * flood). dbg_wake counts every wake_vcpu delivered to this target (any
	 * pending IPI/timer); dbg_run_after_wake counts run-loop re-entries that
	 * consumed a pending wake (proves the woken vCPU actually got scheduled
	 * and VM-entered). If dbg_wake climbs for CPU 15/18 but dbg_run_after_wake
	 * stalls => candidate B (delivered+woken but host never scheduled it). If
	 * dbg_wake itself stays 0 => candidate A (dest drop, never selected).
	 * Remove after diagnosis.
	 */
	atomic_t dbg_wake;
	atomic_t dbg_wake_pending;
	atomic_t dbg_run_after_wake;
#endif
};

static void axkvm_vm_put(struct axkvm_vm *vm);

#ifdef CONFIG_X86_64
static void axkvm_vm_wake_halted_vcpus(struct axkvm_vm *vm)
{
	int i;

	mutex_lock(&vm->lock);
	for (i = 0; i < AXKVM_MAX_VCPUS; i++) {
		struct axkvm_vcpu *vcpu = vm->vcpus[i];

		if (!vcpu)
			continue;
		atomic_set(&vcpu->irq_pending_wakeup, 1);
		wake_up_all(&vcpu->halt_wq);
	}
	mutex_unlock(&vm->lock);
}

/*
 * True when this VM has more vCPUs than the host has online CPUs, i.e. the
 * one-thread-per-vCPU model cannot keep every vCPU on a core simultaneously
 * and CFS must time-slice them. Used to gate oversubscription-only fairness
 * paths (the HLT yield park) so the non-oversubscribed 1/2/4/8/16 baseline
 * keeps its original behavior exactly.
 */
static bool __maybe_unused axkvm_vm_oversubscribed(struct axkvm_vm *vm)
{
	unsigned int online = num_online_cpus();
	unsigned int nr_vcpus = 0;
	int i;

	if (!online)
		return false;
	for (i = 0; i < AXKVM_MAX_VCPUS; i++)
		if (READ_ONCE(vm->vcpus[i]))
			nr_vcpus++;
	return nr_vcpus > online;
}

/*
 * Scheme B: force a bounded L2 guest residency break when L0 does not expose a
 * usable VMX preemption timer to this L1 module. This is deliberately NOT a
 * waitqueue wake: kick_process() only pokes a task that is currently running, so
 * it forces a reschedule/interrupt opportunity for a vCPU thread already owning
 * a core without making blocked vCPUs runnable.
 *
 * Called from the periodic hrtimer hardirq. Do not take sleeping locks here.
 */
static void axkvm_periodic_kick_vcpu_task(struct axkvm_vcpu *vcpu, u64 now_ns,
					  u64 interval_ns)
{
	struct task_struct *task;
	struct pid *pid;
	u64 last;

	if (!vcpu || !READ_ONCE(vcpu->backend_ready))
		return;

	last = READ_ONCE(vcpu->last_periodic_kick_ns);
	if (last && now_ns - last < interval_ns)
		return;
	WRITE_ONCE(vcpu->last_periodic_kick_ns, now_ns);

	rcu_read_lock();
	pid = READ_ONCE(vcpu->run_pid);
	task = pid ? get_pid_task(pid, PIDTYPE_PID) : NULL;
	rcu_read_unlock();
	if (!task)
		return;

	kick_process(task);
	put_task_struct(task);
	atomic64_inc(&vcpu->dbg_periodic_kicks);
	atomic64_inc(&axkvm_dbg_periodic_kick_cnt);
}

static void axkvm_vm_periodic_kick_running_vcpus(struct axkvm_vm *vm, u64 now_ns,
						 u64 interval_ns)
{
	int i;

	if (!vm || !READ_ONCE(vm->backend_ready))
		return;
	if (!axkvm_vm_oversubscribed(vm))
		return;

	for (i = 0; i < AXKVM_MAX_VCPUS; i++)
		axkvm_periodic_kick_vcpu_task(READ_ONCE(vm->vcpus[i]), now_ns,
					      interval_ns);
}

/*
 * Oversubscription-only, rate-limited backstop for HLT-blocked vCPUs, invoked
 * from the 250us backend-timer workfn AFTER the per-vCPU LAPIC timer drain.
 *
 * Under CPU oversubscription an idle NO_HZ AP that HLT-blocks stops arming its
 * own LAPIC tick, so the per-vCPU timer drain has no entry to kick it. If the
 * BSP then broadcasts a CALL_FUNCTION IPI and spins in csd_lock_wait, the
 * halted AP is never woken to run flush_smp_call_function_queue() (from the
 * idle loop, kernel/sched/idle.c) or to report an RCU quiescent state, and the
 * whole guest deadlocks into an RCU stall. Real KVM avoids this because
 * __kvm_vcpu_kick wakes a blocked vCPU on every event; this gives each halted
 * vCPU an equivalent periodic chance to execute its idle/scheduler path.
 *
 * Guards against reintroducing the earlier HLT/wake spin storm:
 *   - gated on oversubscription: a no-op for nr_vcpus <= online (1/2/4/8/16);
 *   - only wakes vCPUs actually parked in the HLT wait (in_halt_wait == 1);
 *   - at most one wake per vCPU per jiffy (last_halt_backstop_jiffies), not
 *     every 250us tick -- so an idle band is nudged ~HZ times/sec, enough to
 *     drain a late CSD/RCU obligation but far below the tight re-wake rate
 *     that previously monopolised the spare cores.
 */
static void axkvm_vm_backstop_halted_vcpus(struct axkvm_vm *vm)
{
	unsigned long now = jiffies;
	int woke = 0, halted = 0;
	int i;

	if (!axkvm_vm_oversubscribed(vm))
		return;

	mutex_lock(&vm->lock);
	for (i = 0; i < AXKVM_MAX_VCPUS; i++) {
		struct axkvm_vcpu *vcpu = vm->vcpus[i];

		if (!vcpu)
			continue;
		if (!atomic_read(&vcpu->in_halt_wait))
			continue;
		halted++;
		if (!time_after_eq(now, vcpu->last_halt_backstop_jiffies + 1))
			continue;
		vcpu->last_halt_backstop_jiffies = now;
		atomic_set(&vcpu->irq_pending_wakeup, 1);
		wake_up_all(&vcpu->halt_wq);
		woke++;
	}
	mutex_unlock(&vm->lock);
	if (unlikely(axkvm_debug_verbose))
		pr_info_ratelimited("halt backstop vm=%llu halted=%d woke=%d\n",
				    (unsigned long long)vm->backend_vm, halted, woke);
}

static void axkvm_vm_queue_pending_irq(struct axkvm_vm *vm, u32 gsi)
{
	if (gsi >= AXKVM_MAX_PENDING_IRQS) {
		pr_warn("drop pending irq gsi=%u max=%u\n", gsi,
			AXKVM_MAX_PENDING_IRQS);
		return;
	}

	set_bit(gsi, &vm->pending_irq_gsis);
	axkvm_vm_wake_halted_vcpus(vm);
}

static int axkvm_vcpu_drain_pending_irqs(struct axkvm_vcpu *vcpu)
{
	struct axkvm_vm *vm = vcpu->vm;
	unsigned long bit;
	u32 gsi;
	int ret;

	/*
	 * irqfd callbacks run in a host workqueue, while AxVCpu internals are
	 * owned by the KVM_RUN thread. Drain external irqfd events from BSP's
	 * vCPU thread so the Rust backend never mutates a vCPU from an unrelated
	 * Linux task. Current Firecracker virtio-mmio routing targets BSP.
	 */
	if (vcpu->id != 0)
		return 0;

	for_each_set_bit(bit, &vm->pending_irq_gsis, AXKVM_MAX_PENDING_IRQS) {
		if (!test_and_clear_bit(bit, &vm->pending_irq_gsis))
			continue;

		gsi = (u32)bit;
		ret = axvisor_kvm_backend_inject_irq(vm->backend_vm, gsi);
		if (ret) {
			pr_err("pending irq inject failed vcpu=%u gsi=%u ret=%d\n",
			       vcpu->id, gsi, ret);
			return ret;
		}
		if (axkvm_trace_count(vcpu->backend_run_calls))
			pr_info("pending irq inject vcpu=%u gsi=%u backend_calls=%llu\n",
				vcpu->id, gsi, vcpu->backend_run_calls);
	}

	return 0;
}

static void axkvm_vm_init_default_irq_routes(struct axkvm_vm *vm)
{
	u32 i;

	memset(vm->irq_routes, 0, sizeof(vm->irq_routes));

	for (i = 0; i < KVM_IOAPIC_NUM_PINS && i < AXKVM_MAX_IRQ_ROUTES; i++) {
		vm->irq_routes[i].valid = true;
		vm->irq_routes[i].type = KVM_IRQ_ROUTING_IRQCHIP;
		vm->irq_routes[i].irqchip = KVM_IRQCHIP_IOAPIC;
		vm->irq_routes[i].pin = i;
	}
}

static u32 axkvm_vm_route_irqfd_gsi(struct axkvm_vm *vm, u32 gsi)
{
	struct axkvm_irq_route *route;

	if (gsi >= AXKVM_MAX_IRQ_ROUTES)
		return gsi;

	route = &vm->irq_routes[gsi];
	if (READ_ONCE(route->valid) &&
	    READ_ONCE(route->type) == KVM_IRQ_ROUTING_IRQCHIP &&
	    READ_ONCE(route->irqchip) == KVM_IRQCHIP_IOAPIC)
		return READ_ONCE(route->pin);

	return gsi;
}

static u32 axkvm_lapic_id(const struct kvm_lapic_state *lapic)
{
	const u8 *regs = lapic->regs;

	return regs[0x23];
}

static void axkvm_set_lapic_id(struct kvm_lapic_state *lapic, u32 id)
{
	u8 *regs = lapic->regs;

	regs[0x20] = 0;
	regs[0x21] = 0;
	regs[0x22] = 0;
	regs[0x23] = (u8)id;
}

static void axkvm_trace_vcpu_backend_state(const char *tag,
					   const struct axkvm_vcpu *vcpu)
{
	pr_info("%s vcpu=%u mp_state=%u rip=%llx cr0=%llx cr3=%llx cr4=%llx efer=%llx apic_base=%llx lapic_id=%x\n",
		tag, vcpu->id, vcpu->mp_state.mp_state, vcpu->regs.rip,
		vcpu->sregs.cr0, vcpu->sregs.cr3, vcpu->sregs.cr4,
		vcpu->sregs.efer, vcpu->sregs.apic_base,
		axkvm_lapic_id(&vcpu->lapic));
}

static void axkvm_init_x86_vcpu_state(struct axkvm_vcpu *vcpu)
{
	vcpu->backend_state_dirty = true;
	vcpu->mp_state.mp_state = vcpu->id == 0 ? KVM_MP_STATE_RUNNABLE :
						  KVM_MP_STATE_UNINITIALIZED;
	vcpu->regs.rflags = 0x2;
	vcpu->fpu_valid = true;
	vcpu->fpu.fcw = 0x37f;
	vcpu->fpu.mxcsr = 0x1f80;
	vcpu->sregs.apic_base = AXKVM_X86_APIC_DEFAULT_PHYS_BASE |
				 MSR_IA32_APICBASE_ENABLE;
	if (vcpu->id == 0)
		vcpu->sregs.apic_base |= MSR_IA32_APICBASE_BSP;
	axkvm_set_lapic_id(&vcpu->lapic, vcpu->id);
	vcpu->sregs.ldt.unusable = 1;
	axkvm_normalize_x86_sregs(&vcpu->sregs);
	vcpu->xcrs.nr_xcrs = 1;
	vcpu->xcrs.xcrs[0].xcr = 0;
	vcpu->xcrs.xcrs[0].value = 1;
	vcpu->tsc_khz = AXKVM_DEFAULT_TSC_KHZ;
}

static bool axkvm_x86_xcr0_valid(u64 xcr0)
{
	const u64 XCR0_X87 = BIT_ULL(0);
	const u64 XCR0_SSE = BIT_ULL(1);
	const u64 XCR0_AVX = BIT_ULL(2);
	const u64 XCR0_BNDREGS = BIT_ULL(3);
	const u64 XCR0_BNDCSR = BIT_ULL(4);
	const u64 XCR0_OPMASK = BIT_ULL(5);
	const u64 XCR0_ZMM_HI256 = BIT_ULL(6);
	const u64 XCR0_HI16_ZMM = BIT_ULL(7);
	const u64 XCR0_SUPPORTED = XCR0_X87 | XCR0_SSE | XCR0_AVX |
				    XCR0_BNDREGS | XCR0_BNDCSR |
				    XCR0_OPMASK | XCR0_ZMM_HI256 |
				    XCR0_HI16_ZMM;
	const u64 XCR0_AVX512 = XCR0_OPMASK | XCR0_ZMM_HI256 |
				 XCR0_HI16_ZMM;

	if ((xcr0 & ~XCR0_SUPPORTED) != 0)
		return false;
	if ((xcr0 & XCR0_X87) == 0)
		return false;
	if ((xcr0 & XCR0_AVX) && ((xcr0 & XCR0_SSE) == 0))
		return false;
	if (!!(xcr0 & XCR0_BNDREGS) != !!(xcr0 & XCR0_BNDCSR))
		return false;
	if ((xcr0 & XCR0_AVX512) &&
	    ((xcr0 & XCR0_AVX512) != XCR0_AVX512 ||
	     (xcr0 & (XCR0_SSE | XCR0_AVX)) != (XCR0_SSE | XCR0_AVX)))
		return false;

	return true;
}

#endif

#ifdef CONFIG_X86_64
static void axkvm_irqfd_inject_work(struct work_struct *work)
{
	struct axkvm_eventfd_binding *binding =
		container_of(work, struct axkvm_eventfd_binding,
			     irqfd_inject_work);
	struct axkvm_vm *vm = binding->vm;

	if (!binding->valid || !binding->irqfd || !vm || !vm->backend_ready)
		return;

	{
		u64 count = atomic64_inc_return(&binding->inject_count);
		u32 routed_gsi = axkvm_vm_route_irqfd_gsi(vm, binding->gsi);

		axkvm_vm_queue_pending_irq(vm, routed_gsi);
		if (axkvm_trace_count(count))
			pr_info("irqfd queue fd=%d gsi=%u routed_gsi=%u count=%llu\n",
				binding->fd, binding->gsi, routed_gsi, count);
	}
}

static int axkvm_irqfd_wakeup(wait_queue_entry_t *wait, unsigned int mode,
			      int sync, void *key)
{
	struct axkvm_eventfd_binding *binding =
		container_of(wait, struct axkvm_eventfd_binding, irqfd_wait);
	__poll_t flags = key_to_poll(key);
	u64 cnt;
	int ret = 0;

	if (flags & EPOLLIN) {
		eventfd_ctx_do_read(binding->ctx, &cnt);
		if (cnt) {
			u64 count = atomic64_inc_return(&binding->wake_count);

			if (axkvm_trace_count(count))
				pr_info("irqfd wake fd=%d gsi=%u count=%llu eventfd_count=%llu\n",
					binding->fd, binding->gsi, count, cnt);
			schedule_work(&binding->irqfd_inject_work);
		}
		ret = 1;
	}

	return ret;
}

static void axkvm_irqfd_poll_func(struct file *file, wait_queue_head_t *wqh,
				  poll_table *pt)
{
	struct axkvm_eventfd_binding *binding =
		container_of(pt, struct axkvm_eventfd_binding, irqfd_pt);

	add_wait_queue(wqh, &binding->irqfd_wait);
	binding->irqfd_wait_registered = true;
}

static void axkvm_eventfd_binding_release(struct axkvm_eventfd_binding *binding)
{
	u64 cnt;

	if (!binding->valid)
		return;

	if (binding->irqfd && binding->irqfd_wait_registered)
		eventfd_ctx_remove_wait_queue(binding->ctx,
					      &binding->irqfd_wait, &cnt);
	if (binding->irqfd)
		flush_work(&binding->irqfd_inject_work);
	eventfd_ctx_put(binding->ctx);
	memset(binding, 0, sizeof(*binding));
}

static void axkvm_vm_release_eventfds(struct axkvm_vm *vm)
{
	unsigned int i;

	for (i = 0; i < AXKVM_MAX_IOEVENTS; i++)
		axkvm_eventfd_binding_release(&vm->ioevents[i]);
	for (i = 0; i < AXKVM_MAX_IRQFDS; i++)
		axkvm_eventfd_binding_release(&vm->irqfds[i]);
}
#else
static void axkvm_vm_release_eventfds(struct axkvm_vm *vm)
{
}
#endif

static void axkvm_memslot_release(struct axkvm_memslot *slot)
{
	if (!slot->valid)
		return;

	if (slot->pages) {
		unsigned long i;

		/*
		 * Lazy on-demand mapping leaves a sparse pages[] array: only
		 * pages that were actually GUP-pinned are non-NULL; VM_IO/
		 * VM_PFNMAP remapped pages have no struct page and are NULL.
		 * Unpin only the populated entries, and mark dirty only those
		 * we mapped writable (a read-only pin must not be reported
		 * dirty).
		 */
		for (i = 0; i < slot->nr_pages; i++) {
			bool dirty = slot->writable &&
				     test_bit(i, slot->writable);

			if (slot->pages[i])
				unpin_user_pages_dirty_lock(&slot->pages[i], 1,
							    dirty);
		}
		kvfree(slot->pages);
	}
	bitmap_free(slot->mapped);
	bitmap_free(slot->writable);

	memset(slot, 0, sizeof(*slot));
}

static void axkvm_backend_unmap_memslot(struct axkvm_vm *vm,
					const struct axkvm_memslot *slot)
{
	if (vm->backend_ready && slot->valid && slot->memory_size)
		axvisor_kvm_backend_unmap_range(vm->backend_vm,
						slot->guest_phys_addr,
						slot->memory_size);
}

#ifdef CONFIG_X86_64
static void axkvm_vm_backend_state(struct axkvm_vm *vm,
				   struct axkvm_backend_vm_state *state)
{
	memset(state, 0, sizeof(*state));
	state->version = AXKVM_BACKEND_STATE_VERSION;
	state->arch = AXKVM_BACKEND_ARCH_X86_64;
	state->irqchip_created = vm->irqchip_created;
	state->pit_created = vm->pit_created;
	state->pit_flags = vm->pit_flags;
	state->tss_addr = vm->tss_addr;
	state->identity_map_addr = vm->identity_map_addr;
	state->clock = &vm->clock;
	state->irqchips = vm->irqchips;
	state->nr_irqchips = KVM_NR_IRQCHIPS;
	state->pit_state = &vm->pit_state;
}
#else
static void axkvm_vm_backend_state(struct axkvm_vm *vm,
				   struct axkvm_backend_vm_state *state)
{
	memset(state, 0, sizeof(*state));
	state->version = AXKVM_BACKEND_STATE_VERSION;
	state->arch = AXKVM_BACKEND_ARCH_UNKNOWN;
}
#endif

static int axkvm_check_extension(long cap)
{
	switch (cap) {
#ifdef CONFIG_X86_64
	case KVM_CAP_IRQCHIP:
	case KVM_CAP_SET_TSS_ADDR:
	case KVM_CAP_EXT_CPUID:
	case KVM_CAP_MP_STATE:
	case KVM_CAP_IRQFD:
	case KVM_CAP_PIT2:
#ifdef KVM_CAP_PIT_STATE2
	case KVM_CAP_PIT_STATE2:
#endif
	case KVM_CAP_IOEVENTFD:
	case KVM_CAP_SET_IDENTITY_MAP_ADDR:
	case KVM_CAP_ADJUST_CLOCK:
#ifdef KVM_CAP_VCPU_EVENTS
	case KVM_CAP_VCPU_EVENTS:
#endif
#ifdef KVM_CAP_DEBUGREGS
	case KVM_CAP_DEBUGREGS:
#endif
#ifdef KVM_CAP_XSAVE
	case KVM_CAP_XSAVE:
#endif
#ifdef KVM_CAP_XCRS
	case KVM_CAP_XCRS:
#endif
	case KVM_CAP_TSC_CONTROL:
#ifdef KVM_CAP_GET_TSC_KHZ
	case KVM_CAP_GET_TSC_KHZ:
#endif
#ifdef KVM_CAP_IOEVENTFD_NO_LENGTH
	case KVM_CAP_IOEVENTFD_NO_LENGTH:
#endif
#ifdef KVM_CAP_IOEVENTFD_ANY_LENGTH
	case KVM_CAP_IOEVENTFD_ANY_LENGTH:
#endif
#ifdef KVM_CAP_GET_MSR_FEATURES
	case KVM_CAP_GET_MSR_FEATURES:
#endif
		return 1;
	case KVM_CAP_IRQ_ROUTING:
		return AXKVM_MAX_IRQ_ROUTES;
#endif
	case KVM_CAP_USER_MEMORY:
		return 1;
	case KVM_CAP_NR_MEMSLOTS:
		return AXKVM_MAX_MEMSLOTS;
	case KVM_CAP_NR_VCPUS:
		return AXKVM_MAX_VCPUS;
#ifdef KVM_CAP_MAX_VCPUS
	case KVM_CAP_MAX_VCPUS:
		return AXKVM_MAX_VCPUS;
#endif
#ifdef KVM_CAP_IMMEDIATE_EXIT
	case KVM_CAP_IMMEDIATE_EXIT:
		return 1;
#endif
	default:
		return 0;
	}
}

static void axkvm_vm_release_kref(struct kref *kref)
{
	struct axkvm_vm *vm = container_of(kref, struct axkvm_vm, refcount);
	unsigned int i;

	/* DIAG: retract the lockless handle before this VM is freed. */
	if (READ_ONCE(axkvm_dbg_vm) == vm)
		WRITE_ONCE(axkvm_dbg_vm, NULL);

	for (i = 0; i < AXKVM_MAX_MEMSLOTS; i++) {
		axkvm_backend_unmap_memslot(vm, &vm->memslots[i]);
		axkvm_memslot_release(&vm->memslots[i]);
	}
	axkvm_vm_release_eventfds(vm);
	if (vm->backend_ready)
		axvisor_kvm_backend_destroy_vm(vm->backend_vm);

	kfree(vm);
}

static struct axkvm_vm *axkvm_vm_get(struct axkvm_vm *vm)
{
	kref_get(&vm->refcount);
	return vm;
}

static void axkvm_vm_put(struct axkvm_vm *vm)
{
	kref_put(&vm->refcount, axkvm_vm_release_kref);
}

#ifdef CONFIG_X86_64
static int axkvm_register_backend_vm(struct axkvm_vm *vm)
{
	u64 handle = vm->backend_vm;
	int ret = 0;

	if (!vm->backend_ready)
		return 0;
	if (!handle || handle > AXKVM_MAX_BACKEND_VMS)
		return -ERANGE;

	mutex_lock(&axkvm_backend_vm_registry_lock);
	if (axkvm_backend_vm_registry[handle]) {
		ret = -EEXIST;
	} else {
		axkvm_vm_get(vm);
		/* WRITE_ONCE pairs with the lockless wake-path READ_ONCE. */
		WRITE_ONCE(axkvm_backend_vm_registry[handle], vm);
	}
	mutex_unlock(&axkvm_backend_vm_registry_lock);

	return ret;
}

static void axkvm_unregister_backend_vm(struct axkvm_vm *vm)
{
	u64 handle = vm->backend_vm;
	struct axkvm_vm *registered = NULL;

	if (!vm->backend_ready || !handle || handle > AXKVM_MAX_BACKEND_VMS)
		return;

	mutex_lock(&axkvm_backend_vm_registry_lock);
	if (axkvm_backend_vm_registry[handle] == vm) {
		registered = vm;
		/* WRITE_ONCE pairs with the lockless wake-path READ_ONCE. */
		WRITE_ONCE(axkvm_backend_vm_registry[handle], NULL);
	}
	mutex_unlock(&axkvm_backend_vm_registry_lock);

	if (registered)
		axkvm_vm_put(registered);
}

static struct axkvm_vm *axkvm_get_backend_vm(u64 backend_vm)
{
	struct axkvm_vm *vm = NULL;

	if (!backend_vm || backend_vm > AXKVM_MAX_BACKEND_VMS)
		return NULL;

	mutex_lock(&axkvm_backend_vm_registry_lock);
	vm = axkvm_backend_vm_registry[backend_vm];
	if (vm)
		axkvm_vm_get(vm);
	mutex_unlock(&axkvm_backend_vm_registry_lock);

	return vm;
}

/*
 * Lock-free registry lookup for the atomic/hardirq wake path. Returns the VM
 * pointer WITHOUT taking a kref -- the caller must only touch it while it is
 * known live (i.e. during guest runtime; a VM is removed from the registry only
 * at teardown, after all in-flight timer drains are synchronised and vCPUs are
 * quiesced). Mirrors KVM's lock-free target lookup on the kick path
 * (virt/kvm/kvm_main.c: no kvm->lock, no refcount per kick). Safe in hardirq
 * and workqueue context because it takes no sleeping lock.
 */
static struct axkvm_vm *axkvm_lookup_backend_vm_locklessly(u64 backend_vm)
{
	if (!backend_vm || backend_vm > AXKVM_MAX_BACKEND_VMS)
		return NULL;

	return READ_ONCE(axkvm_backend_vm_registry[backend_vm]);
}

static void axkvm_backend_timer_workfn(struct work_struct *work)
{
	u64 deadline_ns;
	int i;

	spin_lock_irq(&axkvm_backend_timer_lock);
	deadline_ns = axkvm_backend_timer_deadline_ns;
	spin_unlock_irq(&axkvm_backend_timer_lock);

	if (unlikely(axkvm_debug_verbose))
		pr_info_ratelimited("backend timer wake deadline_ns=%llu\n", deadline_ns);

	/*
	 * Drain due LAPIC timers across every per-vCPU table first. This runs in
	 * process context (workqueue), so it may sleep/lock/allocate. It expires,
	 * re-arms, and injects periodic ticks for vCPUs that are starved off-core
	 * under CPU oversubscription (RUNNABLE but never scheduled, and not
	 * HLT-halted). Without this, such a vCPU's tick owner (e.g. rcu_preempt)
	 * starves -> RCU stall -> guest hang. axkvm_vm_backstop_halted_vcpus below
	 * complements it by periodically nudging idle NO_HZ APs that HLT-blocked
	 * with no armed LAPIC timer, so a late cross-call/RCU obligation resumes.
	 */
	axvisor_kvm_x86_bridge_expire_all_due_timers();

	for (i = 1; i <= AXKVM_MAX_BACKEND_VMS; i++) {
		struct axkvm_vm *vm = axkvm_get_backend_vm(i);

		if (!vm)
			continue;
		/*
		 * Do NOT blanket-wake every vCPU here. expire_all_due_timers()
		 * above already delivered each due LAPIC tick to its exact target
		 * vCPU (axvisor_kvm_x86_bridge_inject_interrupt -> wake_vcpu), so a
		 * blanket wake only re-woke vCPUs with no work to do. Under
		 * oversubscription that turned BSP + last AP into a HLT/wake hot
		 * spin (each 500us tick re-woke them immediately) that monopolised
		 * the spare cores and starved the other vCPUs -- including whoever
		 * owns tick_do_timer_cpu -- so jiffies froze globally.
		 *
		 * The one wake we must keep is BSP's legacy/PIT idle wake: the PIT
		 * runs off this backend timer (arm_x86_idle_wakeup_timer) and its
		 * target is always the boot CPU, which is not covered by the LAPIC
		 * per-vCPU timer drain above.
		 */
		if (READ_ONCE(vm->pit_created))
			axkvm_wake_vcpu_in_vm(vm, 0);
		/*
		 * Oversubscription-only, rate-limited (<=1/jiffy) backstop for
		 * idle NO_HZ APs that HLT-blocked with no armed LAPIC timer, so a
		 * late CALL_FUNCTION/RESCHEDULE IPI target or an RCU-quiescent-state
		 * owing CPU always resumes within a jiffy. No-op for nr_vcpus<=online.
		 */
		axkvm_vm_backstop_halted_vcpus(vm);
		axkvm_vm_put(vm);
	}
}

static enum hrtimer_restart axkvm_backend_timer_cb(struct hrtimer *timer)
{
	unsigned long flags;

	spin_lock_irqsave(&axkvm_backend_timer_lock, flags);
	axkvm_backend_timer_active = false;
	spin_unlock_irqrestore(&axkvm_backend_timer_lock, flags);

	if (axkvm_backend_timer_wq)
		queue_work(axkvm_backend_timer_wq, &axkvm_backend_timer_work);
	else
		schedule_work(&axkvm_backend_timer_work);
	return HRTIMER_NORESTART;
}

/*
 * Independent periodic tick: fires unconditionally every AXKVM_BACKEND_PERIODIC_NS
 * and queues the drain-all workfn. Unlike the one-shot timer this cannot be
 * pushed forward by a busy vCPU's re-arm, so it is the guaranteed liveness
 * source for starved vCPUs' LAPIC ticks. Self-forwards from its own expiry to
 * avoid drift.
 */
static enum hrtimer_restart axkvm_backend_periodic_cb(struct hrtimer *timer)
{
	u64 cb = atomic64_inc_return(&axkvm_dbg_periodic_cb_cnt);
	u64 kick_interval_ns = READ_ONCE(axkvm_periodic_kick_ns);

	/*
	 * DIAG (debugcon witness, hardirq/home-CPU): emit a byte to QEMU
	 * debugcon port 0xe9 every ~10000 callbacks (~2.5s). This bypasses
	 * printk/console-lock/ttyS0 entirely, so it isolates "L1 serial stuck"
	 * (debugcon keeps flowing while ttyS0 dies) from "this hrtimer's home
	 * L1 CPU stopped taking timer interrupts" (debugcon also stops). Remove
	 * after diagnosis.
	 */
	if ((cb % 10000) == 0)
		outb('H', 0xe9);

	if (kick_interval_ns) {
		u64 now_ns = ktime_get_mono_fast_ns();
		int i;

		if (kick_interval_ns < AXKVM_PERIODIC_KICK_MIN_NS)
			kick_interval_ns = AXKVM_PERIODIC_KICK_MIN_NS;
		for (i = 1; i <= AXKVM_MAX_BACKEND_VMS; i++) {
			struct axkvm_vm *vm =
				READ_ONCE(axkvm_backend_vm_registry[i]);

			axkvm_vm_periodic_kick_running_vcpus(vm, now_ns,
							     kick_interval_ns);
		}
	}

	if (axkvm_backend_timer_wq)
		queue_work(axkvm_backend_timer_wq, &axkvm_backend_timer_work);
	else
		schedule_work(&axkvm_backend_timer_work);

	/* DIAG heartbeat: one line per ~2.5s (10000 * 250us) proves the hardirq
	 * callback is still firing throughout the hang. Bounded (~96 lines / 240s),
	 * no flood. Remove after diagnosis. */
	if (axkvm_debug_verbose && (cb % 10000) == 0)
		pr_info("dbg_periodic_alive cb=%llu j=%lu kicks=%llu kick_ns=%llu\n",
			cb, jiffies,
			(unsigned long long)atomic64_read(&axkvm_dbg_periodic_kick_cnt),
			(unsigned long long)kick_interval_ns);

	/*
	 * DIAG (bounded, one-shot, hardirq-safe): ~20s after VM creation (deep
	 * into the hang window), dump per-vCPU dbg_wake vs dbg_run_after_wake for
	 * the CALL_FUNCTION CSD A-vs-B question. Runs here (hardirq) precisely
	 * because the workfn/kworker is CFS-starved during the hang and never
	 * prints. Lockless atomic reads only; pr_info is hardirq safe.
	 * Also prints callback-alive counters so "callback stopped" is
	 * distinguishable from "branch never taken". Fires exactly once.
	 */
	if (!axkvm_dbg_ab_dumped) {
		struct axkvm_vm *vm = READ_ONCE(axkvm_dbg_vm);
		unsigned long anchor = READ_ONCE(axkvm_dbg_first_tick_jiffies);
		unsigned long now = jiffies;

			if (vm && anchor &&
			    time_after_eq(now, anchor + 20 * HZ)) {
				int i;

				axkvm_dbg_ab_dumped = true;
				pr_info("dbg_ab_hdr now=%lu anchor=%lu cb=%llu HZ=%d kicks=%llu kick_ns=%llu\n",
					now, anchor, cb, HZ,
					(unsigned long long)atomic64_read(&axkvm_dbg_periodic_kick_cnt),
					(unsigned long long)kick_interval_ns);
				for (i = 0; i < AXKVM_MAX_VCPUS; i++) {
					struct axkvm_vcpu *v = READ_ONCE(vm->vcpus[i]);

					if (!v)
						continue;
					pr_info("dbg_ab vcpu=%d ready=%d boot=%u run_pid=%d wake=%d run_after_wake=%d in_halt=%d mp=%u calls=%llu kicks=%llu last_kick_ns=%llu\n",
						i, READ_ONCE(v->backend_ready),
						(unsigned int)READ_ONCE(v->boot_state),
						READ_ONCE(v->run_pid) ? 1 : 0,
						atomic_read(&v->dbg_wake),
						atomic_read(&v->dbg_run_after_wake),
						atomic_read(&v->in_halt_wait),
						READ_ONCE(v->mp_state.mp_state),
						READ_ONCE(v->backend_run_calls),
						(unsigned long long)atomic64_read(&v->dbg_periodic_kicks),
						(unsigned long long)READ_ONCE(v->last_periodic_kick_ns));
				}
			}
		}

	hrtimer_forward_now(timer, ns_to_ktime(AXKVM_BACKEND_PERIODIC_NS));
	return HRTIMER_RESTART;
}

void axvisor_kvm_x86_bridge_program_timer(u64 deadline_ns)
{
	unsigned long flags;
	bool should_program = false;

	if (!deadline_ns)
		return;

	spin_lock_irqsave(&axkvm_backend_timer_lock, flags);
	if (!axkvm_backend_timer_active ||
	    deadline_ns < axkvm_backend_timer_deadline_ns) {
		axkvm_backend_timer_deadline_ns = deadline_ns;
		axkvm_backend_timer_active = true;
		should_program = true;
	}
	spin_unlock_irqrestore(&axkvm_backend_timer_lock, flags);

	if (should_program)
		hrtimer_start(&axkvm_backend_timer, ns_to_ktime(deadline_ns),
			      HRTIMER_MODE_ABS);
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_program_timer);

/*
 * reprogram_next_kvm_timer() (Rust) computes the globally-earliest live LAPIC
 * deadline after a drain/register/cancel and calls this. It uses only-earlier
 * semantics (same as program_timer): the one-shot per-deadline hrtimer is only a
 * low-latency wake optimization for HLT/PIT/earlier deadlines, NOT the global
 * liveness source. Exact ("!= cur") re-arm is deliberately avoided: a healthy,
 * always-running vCPU re-arms every ~500us, and exact semantics would let it
 * keep pushing the single one-shot hrtimer's expiry into the future, so its
 * callback would never fire and the drain-all workfn would starve. Global
 * liveness is instead guaranteed by the independent periodic hrtimer
 * (axkvm_backend_periodic_timer), which self-forwards and cannot be pushed
 * forward by any vCPU.
 */
void axvisor_kvm_x86_bridge_reprogram_timer(u64 deadline_ns)
{
	if (!deadline_ns) {
		axvisor_kvm_x86_bridge_cancel_timer();
		return;
	}
	axvisor_kvm_x86_bridge_program_timer(deadline_ns);
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_reprogram_timer);

void axvisor_kvm_x86_bridge_cancel_timer(void)
{
	unsigned long flags;

	spin_lock_irqsave(&axkvm_backend_timer_lock, flags);
	axkvm_backend_timer_active = false;
	axkvm_backend_timer_deadline_ns = 0;
	spin_unlock_irqrestore(&axkvm_backend_timer_lock, flags);

	hrtimer_cancel(&axkvm_backend_timer);
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_cancel_timer);

/*
 * The axkvm_vcpu whose guest is currently executing on this physical CPU.
 * Set immediately before entering the backend (preemption already disabled by
 * the backend's run guard) and cleared immediately after. This lets the atomic
 * guest-APIC-write injection path find the *sending* vCPU without any lock or
 * VM-registry lookup (both of which may sleep and are illegal there): the
 * injection runs on this same CPU with preemption disabled, so this_cpu_read()
 * returns the sender, which pins its owning VM for its whole lifetime.
 */
static DEFINE_PER_CPU(struct axkvm_vcpu *, axkvm_running_vcpu);

/*
 * Deliver the wake to one target vCPU: mark an interrupt pending and kick both
 * wait queues so a HLT-blocked / SIPI-waiting thread resumes and takes the
 * injected event. The three ops here are all atomic-context safe: atomic_set()
 * is lock-free and wake_up_all() takes only the wait-queue's own irqsave
 * spinlock (never a sleeping lock). This mirrors KVM's atomic-safe kick, which
 * wakes the target via rcuwait_wake_up()->wake_up_process() with no mutex
 * (kernel/exit.c:314; __kvm_vcpu_kick virt/kvm/kvm_main.c:3813). The caller is
 * responsible for keeping @vcpu (and its owning VM) alive across this call.
 */
static void axkvm_wake_vcpu_target(struct axkvm_vcpu *vcpu)
{
	if (!vcpu)
		return;
	atomic_set(&vcpu->irq_pending_wakeup, 1);
	/* DIAG: count deliveries + arm the run-after-wake edge (bounded). */
	atomic_inc(&vcpu->dbg_wake);
	atomic_set(&vcpu->dbg_wake_pending, 1);
	wake_up_all(&vcpu->halt_wq);
	wake_up_all(&vcpu->mp_state_wq);
	/*
	 * Low-latency spinner-park wake: a freed target AP that just took an IRQ
	 * or soft event should re-evaluate immediately instead of waiting for the
	 * hardirq backstop / timeout. wake_up_all takes only the wq's irqsave
	 * spinlock, so this stays atomic-context safe. No gen bump here (that is
	 * serialised under vm->lock in axkvm_wake_parked_spinners); a spurious
	 * wake just makes parked threads re-check their gate, which is harmless.
	 */
	if (vcpu->vm)
		wake_up_all(&vcpu->vm->spin_park_wq);
}

/*
 * Wake one vCPU by target id, given a VM whose lifetime the caller already
 * guarantees. Runs in a possibly-sleeping context (e.g. the backend timer
 * workqueue), so it may take vm->lock to serialise against vCPU teardown.
 */
static void axkvm_wake_vcpu_in_vm(struct axkvm_vm *vm, u32 vcpu_id)
{
	if (!vm || vcpu_id >= AXKVM_MAX_VCPUS)
		return;

	mutex_lock(&vm->lock);
	axkvm_wake_vcpu_target(vm->vcpus[vcpu_id]);
	mutex_unlock(&vm->lock);
}

/*
 * Atomic-context wake used by the guest-APIC-write / inject_interrupt path AND
 * by the off-core timer-drain path (backend workqueue / hardirq).
 *
 * MUST be callable with preemption (and possibly IRQs) disabled: the guest
 * writes its LAPIC ICR, we VM-exit into the synchronous handler, fan the IPI
 * out and wake each target -- all inside VmxVcpu::run, where taking a sleeping
 * lock triggers "BUG: scheduling while atomic" (observed run LoLJYI:
 * mutex_lock(&vm->lock) here -> scheduling while atomic -> run_vcpu ret=-16 ->
 * the IPI is never delivered). The previous implementation looked the VM up in
 * the registry (axkvm_get_backend_vm -> mutex), locked vm->lock, and kref_put
 * the VM -- three sleeping operations, all illegal here.
 *
 * Target resolution (KVM-faithful, lock-free):
 *
 *  1. If @backend_vm names a live VM (registry slot non-NULL), use it. This is
 *     the authoritative path for the timer-drain callback, which runs in the
 *     backend workqueue / hardirq with NO running guest on the calling CPU --
 *     there the per-CPU sender below is NULL and a handle-less wake would be
 *     silently DROPPED (the "wake-hole": a starved off-core vCPU's periodic
 *     LAPIC tick was latched but its owner never woken -> RCU stall / hang).
 *     Mirrors KVM's kick: the target vCPU is found lock-free (no kvm->lock, no
 *     srcu) and the vcpus[] array is stable for the VM's runtime (cleared only
 *     at teardown, when no drain is in flight and no guest executes).
 *
 *  2. Otherwise fall back to the on-CPU sending vCPU (published per-CPU around
 *     the backend run with preemption disabled), which pins its own VM for its
 *     whole lifetime. This covers legacy callers that pass a stale/zero handle.
 *
 * If neither yields a VM we drop the wake rather than take a sleeping lock in
 * what may be an atomic context.
 */
void axvisor_kvm_x86_bridge_wake_vcpu(u64 backend_vm, u32 vcpu_id)
{
	struct axkvm_vm *vm;

	if (vcpu_id >= AXKVM_MAX_VCPUS)
		return;

	/*
	 * Prefer the caller-supplied handle: it is correct even when no guest
	 * runs on this CPU (timer-drain workqueue / hardirq). Lock-free read;
	 * the VM is live for the whole runtime it is registered.
	 */
	vm = axkvm_lookup_backend_vm_locklessly(backend_vm);
	if (!vm) {
		struct axkvm_vcpu *sender = this_cpu_read(axkvm_running_vcpu);

		if (sender)
			vm = sender->vm;
	}
	if (!vm)
		return;

	/*
	 * No vm->lock: the target vCPU pointer is stable for the VM's runtime
	 * (cleared only at teardown, when no guest runs). wake_up_all/atomic_set
	 * are atomic-safe. This is the KVM-faithful lock-free kick.
	 */
	axkvm_wake_vcpu_target(READ_ONCE(vm->vcpus[vcpu_id]));
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_wake_vcpu);

/* Nice value applied to a vCPU thread while it is being brought up. */
#define AXKVM_BRINGUP_NICE (-20)
/* Hard cap on how long a single AP may keep its bringup boost. */
#define AXKVM_BRINGUP_BOOST_MS 300

/*
 * Effective AP admission budget: how many APs may be simultaneously admitted
 * but not yet ALIVE during SMP bringup. 0 (auto) reserves 4 cores for the
 * L1 kernel/RCU/BSP so admitted APs do not starve them. Clamped to [1, VCPUS].
 */
static unsigned int axkvm_effective_ap_budget(void)
{
	unsigned int budget = READ_ONCE(axkvm_ap_admit_budget);

	/*
	 * budget == 0 means "admit all APs immediately" — the KVM-aligned model.
	 * Real KVM has no per-VM admission gate (virt/kvm/kvm_main.c KVM_RUN just
	 * runs every vCPU thread); oversubscription is absorbed by the host
	 * scheduler plus PLE/directed-yield. The guest kernel uses parallel CPU
	 * bringup (CONFIG_HOTPLUG_PARALLEL): cpuhp_bringup_cpus_parallel() kicks
	 * INIT/SIPI to ALL APs up front (first pass CPUHP_BP_KICK_AP) and only then
	 * drains cpuhp_bp_sync_alive() CPU-by-CPU. A throttle that admits fewer AP
	 * threads than were kicked deadlocks: the BSP waits for a CPU whose thread
	 * we never admitted, while an admitted AP spins in cpuhp_ap_sync_alive()
	 * waiting for the BSP. So admit all, and rely on the software-PLE
	 * confirmed-spinner park (soft_ple_maybe_park in the Rust bridge) to keep
	 * cores rotating under oversubscription. A non-zero ap_admit_budget is kept
	 * only as an opt-in legacy/serial-debug knob.
	 */
	if (!budget)
		return AXKVM_MAX_VCPUS;

	return clamp_t(unsigned int, budget, 1, AXKVM_MAX_VCPUS);
}

/*
 * Lower a vCPU's KVM_RUN thread nice value so the host scheduler gives it
 * bounded latency during SMP bringup. This is the decisive fix for
 * oversubscribed parallel bringup: Linux 6.x kicks every AP at once, then each
 * AP must reach cpuhp_ap_sync_alive() and write SYNC_STATE_ALIVE within the
 * boot CPU's per-AP ~10s window. With more vCPU threads than cores, an unlucky
 * AP is starved past that window and then spins in cpuhp_ap_sync_alive()
 * forever. The boost is bounded three ways so it cannot itself starve the L1
 * kernel: (1) it is only applied to an AP that is admitted-but-not-ALIVE;
 * (2) it is dropped when the AP reaches ALIVE or settles; (3) a watchdog
 * forcibly restores it after AXKVM_BRINGUP_BOOST_MS. Caller must hold no lock
 * that set_user_nice could invert against; run_pid is refcounted here.
 */
static void axkvm_bringup_boost(struct axkvm_vcpu *vcpu)
{
	struct task_struct *task;

	if (!vcpu || vcpu->bringup_boosted)
		return;

	rcu_read_lock();
	task = vcpu->run_pid ? get_pid_task(vcpu->run_pid, PIDTYPE_PID) : NULL;
	rcu_read_unlock();
	if (!task)
		return;

	set_user_nice(task, AXKVM_BRINGUP_NICE);
	put_task_struct(task);
	vcpu->bringup_boosted = true;
	mod_delayed_work(system_unbound_wq, &vcpu->boost_watchdog,
			 msecs_to_jiffies(AXKVM_BRINGUP_BOOST_MS));
}

/*
 * Strong boost for the single AP the BSP is currently waiting on. This is
 * intentionally narrower than axkvm_bringup_boost(): generic directed-yield
 * may pick arbitrary RUNNABLE siblings, but only current_bringup_target should
 * outrank the rest of the VM as SCHED_FIFO. If this AP had previously been
 * classified as a spinner and demoted to SCHED_IDLE, undo that first; otherwise
 * a later nice boost is invisible and the AP remains effectively parked behind
 * every SCHED_NORMAL vCPU.
 */
static void axkvm_bringup_target_boost(struct axkvm_vcpu *vcpu)
{
	struct task_struct *task;

	if (!vcpu || vcpu->bringup_rt_boosted)
		return;

	rcu_read_lock();
	task = vcpu->run_pid ? get_pid_task(vcpu->run_pid, PIDTYPE_PID) : NULL;
	rcu_read_unlock();
	if (!task)
		return;

	/*
	 * SCHED_FIFO boost was tried and reverted (run sEKmzt): an RT vCPU thread
	 * that keeps spinning in the guest cpuhp_ap_sync_alive atomic-poll loop
	 * preempts L1's own SCHED_NORMAL rcu_preempt/ksoftirqd kthreads and
	 * regressed L1 RCU stalls (0 -> 3). Stay in CFS: undo any SCHED_IDLE
	 * demote and apply the strongest CFS nice instead, which cannot starve
	 * L1 host kernel threads.
	 */
	if (vcpu->spin_demoted) {
		sched_set_normal(task, AXKVM_BRINGUP_NICE);
		vcpu->spin_demoted = false;
	} else {
		set_user_nice(task, AXKVM_BRINGUP_NICE);
	}
	put_task_struct(task);

	vcpu->bringup_boosted = true;
	vcpu->bringup_rt_boosted = false;
	mod_delayed_work(system_unbound_wq, &vcpu->boost_watchdog,
			 msecs_to_jiffies(AXKVM_BRINGUP_BOOST_MS));
}

/*
 * Undo axkvm_bringup_boost(): restore the vCPU thread to the default nice value
 * once it has settled or its boost has expired, so it no longer starves the
 * rest of the system. Idempotent. Does not cancel the watchdog (it is harmless
 * once !bringup_boosted); callers on the teardown path cancel it explicitly.
 */
static void axkvm_bringup_restore(struct axkvm_vcpu *vcpu)
{
	struct task_struct *task;

	if (!vcpu || (!vcpu->bringup_boosted && !vcpu->bringup_rt_boosted))
		return;

	rcu_read_lock();
	task = vcpu->run_pid ? get_pid_task(vcpu->run_pid, PIDTYPE_PID) : NULL;
	rcu_read_unlock();
	if (task) {
		sched_set_normal(task, 0);
		put_task_struct(task);
	}
	vcpu->bringup_boosted = false;
	vcpu->bringup_rt_boosted = false;
	vcpu->spin_demoted = false;
}

/*
 * Watchdog: an AP that spins in cpuhp_ap_sync_alive() forever (missed its
 * window) never reaches HLT, so it would keep nice -20 indefinitely and starve
 * L1 kernel threads. Force the boost off after AXKVM_BRINGUP_BOOST_MS. The AP
 * is lost either way (the boot CPU gave up on it); what matters is not letting
 * it hold a real-time-ish priority. Runs in workqueue context, takes no vm
 * lock (set_user_nice is self-serialising; bringup_boosted is a plain bool
 * flipped only here and on the vcpu's own KVM_RUN thread under vm->lock at
 * admit time -- a stale read only costs one extra harmless restore).
 */
static void axkvm_bringup_boost_watchdog(struct work_struct *work)
{
	struct axkvm_vcpu *vcpu =
		container_of(work, struct axkvm_vcpu, boost_watchdog.work);

	axkvm_bringup_restore(vcpu);
}

/*
 * Demote a confirmed-spinning vCPU's KVM_RUN thread to SCHED_IDLE. This is the
 * decisive oversubscription hand-off primitive. A confirmed AP spinner busy-
 * polls cpuhp_ap_sync_alive() (guest RIP unchanged for a long streak) while
 * owning an L1 core alone (nr_running==1): in that layout yield(),
 * cond_resched() and bare schedule() are no-ops (they re-pick the only task on
 * the runqueue) and yield_to() returns -ESRCH, so nothing can hand the core to
 * the starved BSP driving cpuhp_bp_sync_alive. Block-parking the spinner froze
 * the whole L1 (it left CFS's balancing set). SCHED_IDLE is the middle ground:
 * the spinner stays RUNNABLE (never leaves the runqueue) but sits below every
 * SCHED_NORMAL/SCHED_RT task, so the instant the nice-boosted BSP (or any
 * runnable sibling / L1 kernel thread) is placed on this core it preempts the
 * spinner. Mirrors KVM spin mitigation intent without ever blocking the vCPU.
 *
 * Safe with no vm/vcpu lock held; resolves the task via refcounted run_pid.
 * Idempotent via vcpu->spin_demoted. Restore is RIP-driven (see
 * axkvm_spin_restore callers), never timer/work-driven -- the timer/softirq
 * path is itself starved under this wedge.
 */
static void axkvm_spin_demote(struct axkvm_vcpu *vcpu)
{
	struct task_struct *task;
	struct sched_attr attr = {};

	if (!vcpu || vcpu->spin_demoted)
		return;

	rcu_read_lock();
	task = vcpu->run_pid ? get_pid_task(vcpu->run_pid, PIDTYPE_PID) : NULL;
	rcu_read_unlock();
	if (!task)
		return;

	attr.sched_policy = SCHED_IDLE;
	if (sched_setattr_nocheck(task, &attr) == 0)
		vcpu->spin_demoted = true;
	put_task_struct(task);
}

/*
 * Undo axkvm_spin_demote(): restore the vCPU thread to SCHED_NORMAL nice 0.
 * Idempotent. Called when the guest RIP leaves the spin window (real forward
 * progress), on HLT/settle, and on teardown. Must not depend on any timer/work
 * path.
 */
static void axkvm_spin_restore(struct axkvm_vcpu *vcpu)
{
	struct task_struct *task;

	if (!vcpu || !vcpu->spin_demoted)
		return;

	rcu_read_lock();
	task = vcpu->run_pid ? get_pid_task(vcpu->run_pid, PIDTYPE_PID) : NULL;
	rcu_read_unlock();
	if (task) {
		sched_set_normal(task, 0);
		put_task_struct(task);
	}
	vcpu->spin_demoted = false;
}

/*
 * Bounded liveness backstop for a parked spinner: re-evaluate at least this
 * often. This is a thread-context timeout (NOT a hardirq wake) so it cannot
 * starve L1's own timer-softirq / rcu_preempt kthread the way a high-frequency
 * hardirq wake_up_all did. Explicit low-latency wakes still come from the
 * per-vCPU target wake and the admit/settle/CPU_UP broadcasts.
 */
#define AXKVM_SPIN_PARK_TIMEOUT_JIFFIES 1

/*
 * Block a confirmed spinner out of the run queue to free its L1 core for the
 * starved bringup target AP / BSP. Unlike axkvm_spin_demote (which keeps the
 * thread RUNNABLE at SCHED_IDLE), this makes the thread genuinely non-runnable
 * so the freed core goes through newidle-balance and pulls a starved sibling
 * off another rq -- the only mechanism that actually migrates work under this
 * layout. Blocking is SAFE HERE ONLY because the caller has already dropped
 * migrate_disable and unloaded the VMCS (see the Rust make_internal_run_progress
 * call site); the old block-park froze L1 precisely by blocking inside the
	 * migrate_disable window. Liveness is guaranteed by the 1-jiffy timeout (a
 * thread-context re-evaluation, not a hardirq wake) plus explicit low-latency
 * wakes from the per-vCPU target wake and admit/settle/CPU_UP broadcasts.
 *
 * Returns 0 on a normal wake (the core has been yielded), -EINTR if the run
 * must abort (immediate_exit / pending signal).
 */
static int axkvm_spin_park(struct axkvm_vm *vm, struct axkvm_vcpu *vcpu)
{
	unsigned int gen = READ_ONCE(vm->spin_park_gen);

	WRITE_ONCE(vcpu->spin_parked, true);
	/*
	 * Peek irq_pending_wakeup with atomic_read (NOT xchg): the HLT wait path
	 * (axkvm ... KVM_RUN loop) is the sole consumer via atomic_xchg. A pending
	 * IRQ here just means "wake, don't re-park" so the guest takes it on the
	 * next entry -- consuming it would race that consumer.
	 */
	wait_event_interruptible_timeout(
		vm->spin_park_wq,
		READ_ONCE(vm->spin_park_gen) != gen ||
		atomic_read(&vcpu->irq_pending_wakeup) ||
		READ_ONCE(vcpu->run->immediate_exit__unsafe) ||
		signal_pending(current),
		AXKVM_SPIN_PARK_TIMEOUT_JIFFIES);
	WRITE_ONCE(vcpu->spin_parked, false);

	if (READ_ONCE(vcpu->run->immediate_exit__unsafe) || signal_pending(current))
		return -EINTR;
	return 0;
}

/* Wake every parked spinner in this VM so they re-evaluate. Call with vm->lock
 * held (bumps spin_park_gen, then wake_up_all). */
static void axkvm_wake_parked_spinners(struct axkvm_vm *vm)
{
	vm->spin_park_gen++;
	wake_up_all(&vm->spin_park_wq);
}

/*
 * Yield the current physical CPU directly to a specific target vCPU's KVM_RUN
 * thread. Returns true if yield_to() actually boosted the target. Safe to call
 * with no vm/vcpu lock held; resolves the target task through its refcounted
 * run_pid (never a bare task pointer). Used both by the PLE directed-yield scan
 * below and by the CPU_UP path, where the booting BSP must hand its CPU to the
 * AP it just woke so the (serial) SMP bringup handshake can complete under
 * oversubscription.
 */
/*
 * Diagnostic return codes so callers can log WHY a yield attempt did not boost.
 * >0 means boosted; <=0 encodes the failure reason.
 */
#define AXKVM_YIELD_BOOSTED		1
#define AXKVM_YIELD_NO_TARGET		0
#define AXKVM_YIELD_NO_TASK		(-1)
#define AXKVM_YIELD_TO_ZERO		(-2)	/* yield_to() returned 0 */
#define AXKVM_YIELD_TO_NEG		(-3)	/* yield_to() returned <0 */

static int axkvm_yield_to_vcpu_diag(struct axkvm_vcpu *target)
{
	struct task_struct *task;
	int yret;

	if (!target || !target->backend_ready)
		return AXKVM_YIELD_NO_TARGET;

	rcu_read_lock();
	task = target->run_pid ? get_pid_task(target->run_pid, PIDTYPE_PID) : NULL;
	rcu_read_unlock();
	if (!task)
		return AXKVM_YIELD_NO_TASK;

	yret = yield_to(task, true);
	put_task_struct(task);

	if (yret > 0)
		return AXKVM_YIELD_BOOSTED;
	return yret == 0 ? AXKVM_YIELD_TO_ZERO : AXKVM_YIELD_TO_NEG;
}

static bool axkvm_yield_to_vcpu(struct axkvm_vcpu *target)
{
	return axkvm_yield_to_vcpu_diag(target) == AXKVM_YIELD_BOOSTED;
}

/*
 * Bridge entry: record a deferred directed-yield ("IPI boost") request.
 *
 * MUST be callable from the atomic guest-APIC-write / inject_interrupt path
 * (preemption and IRQs disabled). Therefore it does NO locking, NO VM-registry
 * lookup, and NEVER calls the scheduler: it only stores an integer hint on the
 * *sending* vCPU (this CPU's running vCPU). The hint is (target vcpu id + 1);
 * 0 means none. It is drained and converted into a real yield_to() later, at
 * the sender's run-loop safe point (axkvm_vcpu_drain_boost), where scheduling
 * is legal. This split is required: calling yield_to()->schedule() from here
 * triggers "BUG: scheduling while atomic".
 *
 * Rationale (decisive evidence from 20-vCPU@18-core runtime): once SMP is up,
 * the guest issues call-function / reschedule IPIs between vCPUs. The target is
 * frequently HALTED (idle, waiting for the IPI) or RUNNABLE-but-preempted, and
 * the CFS spread leaves it off-core long enough that the guest's CSD-lock /
 * smp_call_function times out and NMIs the "unresponsive" target. At the safe
 * point, the sender hands its physical core to the target (yield_to), the
 * injection-time analogue of KVM waking + boosting an IPI target.
 *
 * Oversubscription gating is applied at drain time, not here.
 */
void axvisor_kvm_x86_bridge_boost_vcpu(u64 backend_vm, u32 vcpu_id)
{
	struct axkvm_vcpu *sender;

	if (vcpu_id >= AXKVM_MAX_VCPUS)
		return;

	/*
	 * Same CPU as the running guest (preemption disabled by the backend run
	 * guard), so this_cpu_read() gives the sending vCPU. If it is NULL we are
	 * not inside a backend run on this CPU; just drop the hint.
	 */
	sender = this_cpu_read(axkvm_running_vcpu);
	if (!sender || sender->id == vcpu_id)
		return;

	/* Latest requested target wins; a repeated hint simply overwrites. */
	atomic_set(&sender->boost_target, (int)vcpu_id + 1);
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_boost_vcpu);

/*
 * Drain a pending IPI-boost hint at the sender's run-loop safe point (called
 * with scheduling legal, no lock held). Converts the recorded target id into a
 * real yield_to(target_task): resolve the target's refcounted run_pid under
 * RCU, take a task reference, then yield_to() outside the RCU section. Only
 * boosts under oversubscription (guest vCPUs > online host CPUs); otherwise the
 * plain wake already suffices and an extra yield would just add churn.
 */
static void axkvm_vcpu_drain_boost(struct axkvm_vcpu *vcpu)
{
	struct axkvm_vm *vm = vcpu->vm;
	struct axkvm_vcpu *target;
	struct task_struct *task = NULL;
	struct pid *pid;
	unsigned int online, nr_vcpus, i;
	int id;

	id = atomic_xchg(&vcpu->boost_target, 0);
	if (!id)
		return;
	id--;
	if (id < 0 || id >= AXKVM_MAX_VCPUS || (unsigned int)id == vcpu->id)
		return;

	online = num_online_cpus();
	if (!online)
		return;

	nr_vcpus = 0;
	for (i = 0; i < AXKVM_MAX_VCPUS; i++)
		if (READ_ONCE(vm->vcpus[i]))
			nr_vcpus++;
	if (nr_vcpus <= online)
		return;

	target = READ_ONCE(vm->vcpus[id]);
	if (!target || target == vcpu)
		return;

	rcu_read_lock();
	pid = target->run_pid;
	if (pid)
		task = get_pid_task(pid, PIDTYPE_PID);
	rcu_read_unlock();
	if (!task)
		return;

	yield_to(task, true);
	put_task_struct(task);
}

/*
 * Software directed-yield: make the target vCPU thread runnable and give it a
 * nice boost so CFS prefers it once the current (spinning) vCPU parks and frees
 * its core. Unlike yield_to(), which fails under oversubscription when the
 * spinner and target each own a core (rq->nr_running==1 => -ESRCH), this only
 * skews scheduling weight, so it stays effective when vCPU threads are spread
 * one-per-core. The boost is bounded/undone exactly like axkvm_bringup_boost
 * (watchdog restore); reuse it so we do not stack un-restored nice offsets.
 * Caller must not hold a lock set_user_nice could invert against.
 */
static void axkvm_wake_boost_vcpu(struct axkvm_vcpu *target)
{
	struct task_struct *task;

	if (!target || !target->backend_ready)
		return;

	rcu_read_lock();
	task = target->run_pid ? get_pid_task(target->run_pid, PIDTYPE_PID) : NULL;
	rcu_read_unlock();
	if (!task)
		return;

	wake_up_process(task);
	put_task_struct(task);

	axkvm_bringup_boost(target);
}

static void axkvm_wake_boost_bringup_target(struct axkvm_vcpu *target)
{
	struct task_struct *task;

	if (!target || !target->backend_ready)
		return;

	rcu_read_lock();
	task = target->run_pid ? get_pid_task(target->run_pid, PIDTYPE_PID) : NULL;
	rcu_read_unlock();
	if (!task)
		return;

	wake_up_process(task);
	put_task_struct(task);

	axkvm_bringup_target_boost(target);
}

/*
 * Admit the next queued AP if there is budget. Called with vm->lock held after
 * a CPU_UP enqueues an AP or after an admitted AP reaches ALIVE/settles. Admits strictly in
 * BSP kick order (FIFO): the boot CPU waits on APs by cpu number, so admitting
 * out of order could start the 10s window of an AP that is still held. Each
 * admitted AP is boosted and its held KVM_RUN thread woken.
 */
static void axkvm_admit_next_ap(struct axkvm_vm *vm)
{
	unsigned int budget = axkvm_effective_ap_budget();

	while (vm->ap_admitted < budget &&
	       vm->ap_boot_queue_head != vm->ap_boot_queue_tail) {
		unsigned int idx = vm->ap_boot_queue_head % AXKVM_MAX_VCPUS;
		unsigned int id = vm->ap_boot_queue[idx];
		struct axkvm_vcpu *ap = id < AXKVM_MAX_VCPUS ? vm->vcpus[id] : NULL;

		vm->ap_boot_queue_head++;
		if (!ap || ap->boot_state != AP_BOOT_KICKED)
			continue;

		ap->boot_state = AP_BOOT_ADMITTED;
		vm->ap_admitted++;
		axkvm_bringup_boost(ap);
		wake_up_all(&ap->admit_wq);
		axkvm_yield_to_vcpu(ap);
	}
	/* (a) A newly admitted AP is bringup progress: wake parked spinners so a
	 * freed core can be pulled to it. vm->lock is held by our caller. */
	axkvm_wake_parked_spinners(vm);
}

/*
 * Record a CPU_UP for an AP: queue it in kick order and try to admit. If it
 * cannot be admitted right now (budget full) its KVM_RUN thread will block in
 * axkvm_ap_wait_admitted() until an earlier AP reaches ALIVE/settles. Called with vm->lock
 * held. No-op (returns as already handled) if the AP is past KICKED.
 */
static void axkvm_ap_enqueue(struct axkvm_vm *vm, struct axkvm_vcpu *ap)
{
	if (!ap || ap->boot_state != AP_BOOT_NONE)
		return;

	ap->boot_state = AP_BOOT_KICKED;
	vm->ap_boot_queue[vm->ap_boot_queue_tail % AXKVM_MAX_VCPUS] = ap->id;
	vm->ap_boot_queue_tail++;
	axkvm_admit_next_ap(vm);
}

/*
 * Mark an AP as settled (first HLT observed => guaranteed fully online). If the
 * AP was not seen at the ALIVE spin point first, this drops its boost, releases
 * its admission budget, and admits the next queued AP. Called with vm->lock held.
 */
static void axkvm_ap_settle(struct axkvm_vm *vm, struct axkvm_vcpu *ap)
{
	if (!ap)
		return;

	/*
	 * Release admission budget and drop the bringup boost only at SETTLED
	 * (first HLT), never at ALIVE. An AP that has reached cpuhp_ap_sync_alive
	 * (ALIVE) is NOT yet fully online: it still has to complete the online
	 * handshake with the boot CPU (SYNC_STATE_SHOULD_ONLINE), finish
	 * per-CPU init, and enter its idle loop. Under oversubscription, dropping
	 * its boost and admitting a competitor at ALIVE starves the AP before it
	 * settles, so a later BSP on_each_cpu()/smp_call_function_many_cond()
	 * waits forever on that AP's CSD ack (observed as a runtime freeze at
	 * ~HugeTLB init with the BSP spinning in smp_call_function_many_cond and
	 * an AP spinning in cpuhp_ap_sync_alive). Holding budget+boost until the
	 * first HLT guarantees the AP a continuous run window to fully online.
	 */
	if (ap->boot_state == AP_BOOT_ADMITTED ||
	    ap->boot_state == AP_BOOT_ALIVE) {
		ap->boot_state = AP_BOOT_SETTLED;
		if (vm->ap_admitted)
			vm->ap_admitted--;
		/*
		 * The CPU_UP path (axkvm_kick_ap_for_bringup) sets
		 * current_bringup_target to this AP but nothing clears it. Clear
		 * it here now the AP is fully settled so axkvm_vm_post_bringup()
		 * can tell "still bringing up an AP" from "steady state". Without
		 * this the target stays latched at the last AP id forever and any
		 * post-bringup-gated fairness path would never arm.
		 */
		if (READ_ONCE(vm->current_bringup_target) == (int)ap->id)
			WRITE_ONCE(vm->current_bringup_target, -1);
		axkvm_bringup_restore(ap);
		axkvm_admit_next_ap(vm);
	}
}

static bool axkvm_ap_mark_alive(struct axkvm_vm *vm, struct axkvm_vcpu *ap)
{
	if (!ap || ap->boot_state != AP_BOOT_ADMITTED)
		return false;

	/*
	 * Record the ALIVE milestone but keep the admission budget and bringup
	 * boost until the AP settles (first HLT). See axkvm_ap_settle() for why
	 * releasing at ALIVE causes a runtime CSD-lock deadlock under
	 * oversubscription.
	 */
	ap->boot_state = AP_BOOT_ALIVE;
	return true;
}

void axvisor_kvm_x86_bridge_note_ap_alive_spin(u64 backend_vm, u32 vcpu_id,
					       u64 rip)
{
	struct axkvm_vcpu *controller_vcpu = NULL;
	struct axkvm_vm *vm;
	int controller;
	bool marked = false;

	if (!READ_ONCE(axkvm_ap_alive_spin_rip) ||
	    rip != READ_ONCE(axkvm_ap_alive_spin_rip) || !vcpu_id)
		return;

	vm = axkvm_get_backend_vm(backend_vm);
	if (!vm)
		return;

	mutex_lock(&vm->lock);
	if (vcpu_id < AXKVM_MAX_VCPUS)
		marked = axkvm_ap_mark_alive(vm, vm->vcpus[vcpu_id]);
	controller = READ_ONCE(vm->boot_controller_id);
	if (marked && controller >= 0 && controller < AXKVM_MAX_VCPUS)
		controller_vcpu = vm->vcpus[controller];
	mutex_unlock(&vm->lock);

	if (marked && controller_vcpu)
		axkvm_yield_to_vcpu(controller_vcpu);

	axkvm_vm_put(vm);
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_note_ap_alive_spin);

/*
 * Block an AP's KVM_RUN thread until it is admitted (moved out of KICKED). The
 * caller must NOT hold vm->lock. Returns 0 when admitted, -EINTR if the run
 * should abort (signal / immediate_exit). APs that were never enqueued (state
 * still NONE, e.g. a stray run before CPU_UP) return immediately.
 */
static int axkvm_ap_wait_admitted(struct axkvm_vcpu *vcpu)
{
	int ret;

	if (READ_ONCE(vcpu->boot_state) != AP_BOOT_KICKED)
		return 0;

	for (;;) {
		ret = wait_event_interruptible(
			vcpu->admit_wq,
			READ_ONCE(vcpu->boot_state) != AP_BOOT_KICKED ||
				READ_ONCE(vcpu->run->immediate_exit__unsafe));
		if (ret < 0)
			return -EINTR;
		if (READ_ONCE(vcpu->run->immediate_exit__unsafe) ||
		    signal_pending(current))
			return -EINTR;
		if (READ_ONCE(vcpu->boot_state) != AP_BOOT_KICKED)
			return 0;
	}
}

/*
 * kvm-clock / pvclock page writers.
 *
 * The guest enables kvm-clock by writing MSR_KVM_SYSTEM_TIME_NEW with the
 * guest-physical address of a per-vCPU struct pvclock_vcpu_time_info (bit 0 =
 * enable). We translate that GPA to the userspace address backing it (the
 * memslot the VMM registered) and write the struct there. The guest reads
 * system time as: system_time + scale(rdtsc() - tsc_timestamp), so once it has
 * this page it no longer depends on a per-tick timer IRQ to advance time --
 * which is exactly what starves under vCPU oversubscription.
 */

/* Compute a fixed-point multiplier: (dividend << 32) / divisor. Mirrors KVM's
 * div_frac()/do_shl32_div32(). */
static u32 axkvm_div_frac(u32 dividend, u32 divisor)
{
	u64 tmp = ((u64)dividend) << 32;

	do_div(tmp, divisor);
	return (u32)tmp;
}

/* Derive pvclock tsc_shift/tsc_to_system_mul from the guest TSC frequency so
 * that scaled TSC ticks map to nanoseconds. Mirrors KVM's kvm_get_time_scale()
 * with scaled_hz = NSEC_PER_SEC and base_hz = tsc_hz. */
static void axkvm_pvclock_time_scale(u64 tsc_khz, s8 *pshift, u32 *pmul)
{
	u64 scaled_hz = NSEC_PER_SEC;
	u64 base_hz = tsc_khz * 1000ULL;
	u64 tps64 = base_hz;
	u64 scaled64 = scaled_hz;
	u32 tps32;
	int shift = 0;

	if (!base_hz) {
		*pshift = 0;
		*pmul = 0;
		return;
	}

	while (tps64 > scaled64 * 2 || (tps64 & 0xffffffff00000000ULL)) {
		tps64 >>= 1;
		shift--;
	}

	tps32 = (u32)tps64;
	while (tps32 <= scaled64 || (scaled64 & 0xffffffff00000000ULL)) {
		if ((scaled64 & 0xffffffff00000000ULL) || (tps32 & 0x80000000))
			scaled64 >>= 1;
		else
			tps32 <<= 1;
		shift++;
	}

	*pshift = shift;
	*pmul = axkvm_div_frac((u32)scaled64, tps32);
}

/*
 * Translate a guest-physical address to the userspace virtual address backing
 * it, and confirm at least len bytes fit inside a single memslot. Returns 0 on
 * success. Caller must hold vm->lock.
 */
static int axkvm_gpa_to_hva(struct axkvm_vm *vm, u64 gpa, size_t len, u64 *hva)
{
	int i;

	for (i = 0; i < AXKVM_MAX_MEMSLOTS; i++) {
		struct axkvm_memslot *slot = &vm->memslots[i];

		if (!slot->valid || !slot->memory_size)
			continue;
		if (gpa < slot->guest_phys_addr)
			continue;
		if (gpa + len > slot->guest_phys_addr + slot->memory_size)
			continue;
		*hva = slot->userspace_addr + (gpa - slot->guest_phys_addr);
		return 0;
	}
	return -EFAULT;
}

/*
 * Compute the backend map flags for a fault-in, folding in read-only intent.
 * The backend maps writable (RWX) by default; AXKVM_MAP_RDONLY drops WRITE so
 * a page whose backing VMA is not writable (or a remapped PFN that
 * follow_pfnmap reported non-writable) installs a read-only EPT leaf.
 */
static u32 axkvm_map_flags(const struct axkvm_memslot *slot, bool writable)
{
	u32 flags = slot->flags;

	if (!writable)
		flags |= AXKVM_MAP_RDONLY;
	return flags;
}

/*
 * Resolve a VM_IO / VM_PFNMAP HVA to a raw PFN, mirroring KVM's
 * hva_to_pfn_remapped (virt/kvm/kvm_main.c). GUP refuses VM_IO|VM_PFNMAP
 * outright (mm/gup.c check_vma_flags), so gvisor's sentry vvar-style mappings
 * identity-mapped into guest-physical can only be resolved this way.
 *
 * MUST be called with mmap_read_lock(current->mm) held and @vma obtained from
 * that same locked lookup. On return the caller must re-check for -EAGAIN:
 * fixup_user_fault() may drop and reacquire the mmap lock (unlocked=true),
 * which invalidates @vma and requires a fresh vma_lookup under a fresh lock.
 *
 * On success *hpa / *writable are populated (copied out BEFORE
 * follow_pfnmap_end, after which the args fields are invalid). The caller
 * installs the EPT leaf AFTER releasing the mmap lock, since
 * follow_pfnmap_start/end hold a page-table spinlock across the args lifetime.
 */
static int axkvm_resolve_remapped_pfn_locked(struct vm_area_struct *vma,
					     unsigned long hva, bool write,
					     u64 *hpa, bool *writable)
{
	struct follow_pfnmap_args args = { .vma = vma, .address = hva };
	bool unlocked = false;
	int ret;

	ret = follow_pfnmap_start(&args);
	if (ret) {
		/*
		 * get_user_pages() does not invoke the fault handler for
		 * VM_IO/VM_PFNMAP, so trigger it explicitly, then retry the
		 * walk. If the mmap lock was dropped, bail with -EAGAIN so the
		 * caller re-looks-up the (now stale) vma.
		 */
		ret = fixup_user_fault(current->mm, hva,
				       write ? FAULT_FLAG_WRITE : 0, &unlocked);
		if (unlocked)
			return -EAGAIN;
		if (ret)
			return ret;

		ret = follow_pfnmap_start(&args);
		if (ret)
			return ret;
	}

	if (write && !args.writable) {
		/*
		 * Write fault against a non-writable remapped PFN. KVM returns
		 * KVM_PFN_ERR_RO_FAULT and emulates; this research shim has no
		 * such emulation path, so abort the run with -EFAULT.
		 */
		ret = -EFAULT;
		goto out;
	}

	*hpa = PFN_PHYS(args.pfn);
	*writable = args.writable;
	ret = 0;
out:
	follow_pfnmap_end(&args);
	return ret;
}

/*
 * Fault in a single guest page on demand and insert it into the backend EPT.
 *
 * Memslots are registered without eager pinning (see
 * axkvm_vm_set_user_memory_region), so a guest access to an as-yet-unmapped
 * GPA in a valid slot causes an EPT violation that the Rust bridge routes here.
 * We locate the owning slot and resolve the backing page one of two ways,
 * mirroring KVM's hva_to_pfn():
 *   - ordinary anonymous/file RAM: GUP-pin (pin_user_pages_fast), map, and
 *     record the struct page in slot->pages[idx] for teardown unpinning;
 *   - VM_IO / VM_PFNMAP backing (no refcounted page, e.g. gvisor's sentry
 *     vvar mapping): follow_pfnmap_start() -> raw PFN, mapped straight into the
 *     EPT with no page to pin (torn down by the slot's range-unmap).
 * slot->mapped[idx] is the authoritative "already installed" bit for both
 * cases; slot->writable[idx] records the EPT leaf writability.
 *
 * Returns 0 on success (guest may re-enter), -ENOENT if the GPA is not backed
 * by any valid slot (caller falls through to MMIO decode), or another negative
 * errno on a real fault-in failure (which must abort the run). Caller holds
 * vm->lock.
 */
static int axkvm_fault_in_gpa(struct axkvm_vm *vm, u64 gpa, bool write)
{
	u64 aligned = gpa & PAGE_MASK;
	struct axkvm_memslot *slot = NULL;
	struct vm_area_struct *vma;
	unsigned long idx, hva, vm_flags = 0;
	struct page *page;
	bool have_vma = false;
	int i, pinned, ret;

	for (i = 0; i < AXKVM_MAX_MEMSLOTS; i++) {
		struct axkvm_memslot *s = &vm->memslots[i];

		if (!s->valid || !s->memory_size)
			continue;
		if (aligned < s->guest_phys_addr)
			continue;
		if (aligned >= s->guest_phys_addr + s->memory_size)
			continue;
		slot = s;
		break;
	}
	if (!slot || !slot->pages) {
		/*
		 * No registered memslot covers this GPA. Mirror KVM's
		 * kvm_faultin_pfn: a slotless GPA is not RAM we can fault in;
		 * it must be handled as MMIO emulation by the caller. Return a
		 * DISTINCT code (-ENOENT) so the caller can tell "no slot ->
		 * fall through to MMIO decode" apart from real fault-in errors
		 * (pin/map/OOM) which must abort the run, not be treated as MMIO.
		 */
		pr_info_ratelimited("fault_in_gpa: no slot for gpa=%#llx aligned=%#llx\n",
				    gpa, aligned);
		return -ENOENT;
	}

	idx = (aligned - slot->guest_phys_addr) >> PAGE_SHIFT;
	if (idx >= slot->nr_pages)
		return -ENOENT;
	if (test_bit(idx, slot->mapped)) {
		/*
		 * Already installed (racing vCPU or a re-fault). If this is a
		 * write to a leaf we only mapped read-only, we cannot satisfy
		 * it here (no RO-fault emulation) -> abort.
		 */
		if (write && !test_bit(idx, slot->writable))
			return -EFAULT;
		return 0;
	}

	hva = slot->userspace_addr + (aligned - slot->guest_phys_addr);

	/*
	 * Fast path: ordinary GUP-able RAM. The GUP write intent MUST match the
	 * EPT writability -- pinning a not-yet-COWed anonymous page without
	 * FOLL_WRITE could resolve the shared zero page, and mapping THAT pfn
	 * writable would let guest writes bypass the host COW fault and land on
	 * the wrong physical page. So fault in with write intent for RAM.
	 * FOLL_LONGTERM is intentionally dropped (its DMA-pin restrictions
	 * reject otherwise-faultable anonymous pages); the FOLL_PIN ref is
	 * released at EPT teardown, sufficient for this research shim.
	 */
	pinned = pin_user_pages_fast(hva, 1, FOLL_WRITE, &page);
	if (pinned == 1) {
		ret = axvisor_kvm_backend_map_page_nolog(vm->backend_vm, aligned,
							 page_to_phys(page),
							 axkvm_map_flags(slot,
									 true));
		if (ret) {
			unpin_user_page(page);
			return ret;
		}
		slot->pages[idx] = page;
		set_bit(idx, slot->writable);
		set_bit(idx, slot->mapped);
		return 0;
	}

	/*
	 * GUP failed. Look up the VMA to decide the fallback path:
	 *   - VM_IO / VM_PFNMAP: remapped raw-PFN path (KVM hva_to_pfn_remapped).
	 *   - read-only VMA: read-only GUP pin + read-only EPT leaf.
	 *   - otherwise: real fault-in error.
	 */
	mmap_read_lock(current->mm);
retry:
	vma = vma_lookup(current->mm, hva);
	if (vma && (vma->vm_flags & (VM_IO | VM_PFNMAP))) {
		u64 hpa;
		bool map_writable;

		ret = axkvm_resolve_remapped_pfn_locked(vma, hva, write,
							&hpa, &map_writable);
		if (ret == -EAGAIN)
			goto retry;
		mmap_read_unlock(current->mm);
		if (ret) {
			pr_info_ratelimited("fault_in_gpa: remapped resolve failed gpa=%#llx hva=%#lx write=%d rc=%d\n",
					    gpa, hva, write, ret);
			return ret;
		}

		ret = axvisor_kvm_backend_map_page_nolog(vm->backend_vm, aligned,
							 hpa,
							 axkvm_map_flags(slot,
									 map_writable));
		if (ret)
			return ret;
		/* Remapped PFN: no struct page to record in pages[]. */
		if (map_writable)
			set_bit(idx, slot->writable);
		set_bit(idx, slot->mapped);
		return 0;
	}

	have_vma = vma != NULL;
	vm_flags = have_vma ? vma->vm_flags : 0;
	mmap_read_unlock(current->mm);

	/*
	 * A read-only VMA (e.g. file-backed guest kernel text) cannot be
	 * write-pinned. A write fault against it cannot be satisfied. A read
	 * fault falls back to a read-only pin + read-only EPT leaf.
	 */
	if (have_vma && !(vm_flags & VM_WRITE)) {
		if (write)
			return -EFAULT;

		pinned = pin_user_pages_fast(hva, 1, 0, &page);
		if (pinned == 1) {
			ret = axvisor_kvm_backend_map_page_nolog(
				vm->backend_vm, aligned, page_to_phys(page),
				axkvm_map_flags(slot, false));
			if (ret) {
				unpin_user_page(page);
				return ret;
			}
			slot->pages[idx] = page;
			set_bit(idx, slot->mapped);
			return 0;
		}
	}

	pr_info_ratelimited("fault_in_gpa: pin failed gpa=%#llx hva=%#lx write=%d rc=%d vma=%s flags=%#lx\n",
			    gpa, hva, write, pinned,
			    have_vma ? "yes" : "NONE", vm_flags);
	return pinned < 0 ? pinned : -EFAULT;
}

/*
 * Capture the VM-wide (host_tsc, kernel_ns) master pair ONCE, mirroring KVM's
 * pvclock_update_vm_gtod_copy() -> kvm_get_time_and_clockread() which snapshots
 * ka->master_cycle_now / ka->master_kernel_ns together. ktime_get_snapshot()
 * reads the clocksource counter (.cycles == raw TSC when the host clocksource
 * is TSC) and the boot-time nanoseconds (.boot) at the SAME instant under the
 * timekeeper seqlock, so the pair is internally consistent. Every vCPU then
 * derives its pvclock page from this single pair, so all pages are mutually
 * monotonic no matter which core samples -- the precondition for asserting
 * PVCLOCK_TSC_STABLE_BIT. Caller holds vm->lock.
 */
static void axkvm_pvclock_capture_master(struct axkvm_vm *vm)
{
	struct system_time_snapshot snap;

	if (vm->pvclock_master_valid)
		return;

	ktime_get_snapshot(&snap);
	vm->pvclock_master_tsc = snap.cycles;
	vm->pvclock_master_kernel_ns = ktime_to_ns(snap.boot);
	/*
	 * Only assert stability if the host clocksource IS the TSC, mirroring
	 * KVM's host_tsc_clocksource gate (kvm_get_time_and_clockread ->
	 * gtod_is_based_on_tsc). Then snap.cycles is a raw TSC value directly
	 * comparable to the guest's rdtsc(), so PVCLOCK_TSC_STABLE_BIT is truthful
	 * and the guest can skip its clocksource watchdog. If the host is on some
	 * other clocksource, .cycles is not a bare TSC; asserting STABLE_BIT would
	 * lie and reintroduce the cross-vCPU skew that stalled RCU.
	 */
	vm->pvclock_master_stable = (snap.cs_id == CSID_X86_TSC);
	vm->pvclock_master_valid = true;
}

/*
 * Refresh a vCPU's pvclock_vcpu_time_info page. Must run in that vCPU's own
 * KVM_RUN thread context (so current->mm maps the guest's memory for
 * copy_to_user). Caller holds vcpu->lock; this takes vm->lock for the memslot
 * lookup. No-op if the guest has not enabled kvm-clock.
 */
static void axkvm_pvclock_refresh(struct axkvm_vcpu *vcpu)
{
	struct axkvm_vm *vm = vcpu->vm;
	struct pvclock_vcpu_time_info pvti = {};
	u64 hva = 0;
	u64 tsc_khz;
	u64 master_tsc, master_kernel_ns;
	bool master_stable;
	int ret;

	if (!vcpu->pvclock_enabled || !vcpu->pvclock_gpa)
		return;

	mutex_lock(&vm->lock);
	ret = axkvm_gpa_to_hva(vm, vcpu->pvclock_gpa, sizeof(pvti), &hva);
	if (!ret)
		axkvm_pvclock_capture_master(vm);
	master_tsc = vm->pvclock_master_tsc;
	master_kernel_ns = vm->pvclock_master_kernel_ns;
	master_stable = vm->pvclock_master_stable;
	mutex_unlock(&vm->lock);
	if (ret)
		return;

	tsc_khz = vcpu->tsc_khz ? vcpu->tsc_khz : AXKVM_DEFAULT_TSC_KHZ;
	axkvm_pvclock_time_scale(tsc_khz, &pvti.tsc_shift,
				 &pvti.tsc_to_system_mul);

	/*
	 * Version protocol: bump to odd before the update, bump to even after.
	 * The guest retries if it sees an odd version or a change across the
	 * read, so the field values stay consistent.
	 */
	vcpu->pvclock_version += 2;
	pvti.version = vcpu->pvclock_version | 1;
	/*
	 * Derive from the VM-wide master pair (NOT a per-core rdtsc), so every
	 * vCPU page uses the same (tsc_timestamp, system_time) anchor. The guest
	 * computes time = system_time + scale(rdtsc() - tsc_timestamp); with the
	 * shared anchor + PVCLOCK_TSC_STABLE_BIT, pvclock_clocksource_read()
	 * clamps to the last value so cross-vCPU reads stay monotonic even if a
	 * lagging core's rdtsc briefly trails master_tsc. This mirrors KVM's
	 * kvm_guest_time_update() using master_cycle_now/master_kernel_ns.
	 */
	pvti.tsc_timestamp = master_tsc;
	pvti.system_time = master_kernel_ns;
	pvti.flags = master_stable ? AXKVM_PVCLOCK_TSC_STABLE_BIT : 0;

	if (copy_to_user((void __user *)(uintptr_t)hva, &pvti, sizeof(pvti)))
		return;

	/* Second (even) version publish so the guest sees a settled page. */
	pvti.version = vcpu->pvclock_version;
	if (copy_to_user((void __user *)(uintptr_t)hva, &pvti.version,
			 sizeof(pvti.version)))
		return;
}

/* Write the VM-scoped pvclock_wall_clock page (boot wall-clock reference). */
static void axkvm_pvclock_write_wall_clock(struct axkvm_vm *vm, u64 gpa)
{
	struct pvclock_wall_clock wc = {};
	struct timespec64 boot;
	u64 hva = 0;
	int ret;

	mutex_lock(&vm->lock);
	ret = axkvm_gpa_to_hva(vm, gpa, sizeof(wc), &hva);
	mutex_unlock(&vm->lock);
	if (ret)
		return;

	ktime_get_real_ts64(&boot);
	/* wall clock = real time at which system_time was zero (boot). */
	boot = timespec64_sub(boot, ns_to_timespec64(ktime_get_boottime_ns()));

	wc.version = 1;
	wc.sec = (u32)boot.tv_sec;
	wc.nsec = (u32)boot.tv_nsec;
	if (copy_to_user((void __user *)(uintptr_t)hva, &wc, sizeof(wc)))
		return;
}

/*
 * Bridge entry: the guest wrote a pvclock MSR (caught as an intercepted
 * MSR_WRITE VM-exit). Called from the x86_vcpu run path via the bridge, in the
 * vCPU's own thread context. Records the GPA and refreshes the page.
 */
void axvisor_kvm_x86_bridge_pvclock_write(u64 backend_vm, u32 vcpu_id, u32 msr,
					  u64 value)
{
	struct axkvm_vm *vm = axkvm_get_backend_vm(backend_vm);
	struct axkvm_vcpu *vcpu;

	if (!vm || vcpu_id >= AXKVM_MAX_VCPUS)
		return;
	vcpu = vm->vcpus[vcpu_id];
	if (!vcpu)
		return;

	if (msr == AXKVM_MSR_KVM_SYSTEM_TIME_NEW) {
		mutex_lock(&vcpu->lock);
		vcpu->pvclock_system_time_msr = value;
		vcpu->pvclock_enabled = value & AXKVM_KVM_SYSTEM_TIME_ENABLE;
		vcpu->pvclock_gpa = value & ~AXKVM_KVM_SYSTEM_TIME_ENABLE &
				    PAGE_MASK;
		if (vcpu->pvclock_enabled)
			axkvm_pvclock_refresh(vcpu);
		mutex_unlock(&vcpu->lock);
	} else if (msr == AXKVM_MSR_KVM_WALL_CLOCK_NEW) {
		u64 gpa = value & PAGE_MASK;

		WRITE_ONCE(vm->pvclock_wall_clock_gpa, gpa);
		if (gpa)
			axkvm_pvclock_write_wall_clock(vm, gpa);
	}
}

/*
 * Bridge entry: refresh the vCPU's pvclock page at KVM_RUN entry so system_time
 * keeps advancing monotonically even if the guest never rewrites the MSR.
 * Called in the vCPU's own thread context.
 */
void axvisor_kvm_x86_bridge_pvclock_refresh(u64 backend_vm, u32 vcpu_id)
{
	struct axkvm_vm *vm = axkvm_get_backend_vm(backend_vm);
	struct axkvm_vcpu *vcpu;

	if (!vm || vcpu_id >= AXKVM_MAX_VCPUS)
		return;
	vcpu = vm->vcpus[vcpu_id];
	if (!vcpu || !vcpu->pvclock_enabled)
		return;

	mutex_lock(&vcpu->lock);
	axkvm_pvclock_refresh(vcpu);
	mutex_unlock(&vcpu->lock);
}

/*
 * Directed yield for PAUSE-loop-exiting (PLE), mirroring KVM's
 * kvm_vcpu_on_spin() -> kvm_vcpu_yield_to() -> yield_to().
 *
 * Called from the bridge run loop when the current vCPU has spun long enough
 * inside a single KVM_RUN (software PLE; hardware PLE is unavailable under this
 * nested setup). It is almost certainly waiting on another vCPU the host
 * scheduler has preempted.
 *
 * Target selection is topology-aware rather than a blind round-robin, because
 * serial SMP bringup under oversubscription needs the *right* peer, not any
 * runnable sibling:
 *
 *   1. If the caller is an AP (not the boot controller) and a boot controller
 *      is recorded, yield to the controller first. An AP spinning in
 *      cpuhp_ap_sync_alive() is waiting for the controller (the CPU_UP sender,
 *      normally vcpu 0) to advance the handshake -- so hand the CPU straight to
 *      it. This is the case that makes 1.8x oversubscription boot: without it a
 *      round-robin scatters CPU across 31 spinning APs and the target AP misses
 *      the controller's 10s cpuhp_bp_sync_alive() release window, wedging
 *      bringup permanently.
 *   2. Otherwise fall back to round-robin over RUNNABLE siblings (covers the
 *      controller boosting APs, and generic guest spinlock contention).
 */
/*
 * Returns true if the caller (a confirmed soft-PLE spinner) should now park
 * itself for one tick to free its core, i.e. there is a starved peer that can
 * take the core: a directed target (boot controller / waited-on AP) or at least
 * one RUNNABLE sibling was seen. Returns false when yield_to() already handed
 * off the core (boosted) or when there is NO peer to run (e.g. the BSP spinning
 * alone during single-threaded early boot) — parking then would only stall the
 * one vCPU making progress.
 */
bool axvisor_kvm_x86_bridge_directed_yield(u64 backend_vm, u32 cur_vcpu_id)
{
	bool should_park = false;
	struct axkvm_vm *vm;
	unsigned int start;
	int controller;
	int i;
	bool boosted = false;
	bool dir_is_bringup_target = false;
	/* Diagnostics: capture why we did/didn't boost this call. */
	int ctrl_diag = AXKVM_YIELD_NO_TARGET;
	int last_diag = AXKVM_YIELD_NO_TARGET;
	unsigned int runnable_seen = 0;
	/*
	 * Software-directed target: the vCPU we want CFS to run on the core we
	 * are about to free. Prefer the boot controller (the AP is waiting on it
	 * for the online handshake), else the last RUNNABLE sibling we scanned.
	 */
	int dir_idx = -1;

	vm = axkvm_get_backend_vm(backend_vm);
	if (!vm)
		return false;

	mutex_lock(&vm->lock);

	/*
	 * Priority 0: the AP the boot controller is *currently* waiting on
	 * (last SIPI'd). Under serial SMP bringup the whole handshake is blocked
	 * on this one AP getting a core; boosting a random sibling wastes the
	 * hand-off. This is the dominant case when the BSP (== controller) spins
	 * in do_boot_cpu(), where the Priority-1 controller check below is a
	 * no-op (controller == cur_vcpu_id).
	 */
	{
		int bringup = READ_ONCE(vm->current_bringup_target);

		if (bringup >= 0 && (unsigned int)bringup != cur_vcpu_id &&
		    bringup < AXKVM_MAX_VCPUS) {
			struct axkvm_vcpu *b = vm->vcpus[bringup];

			if (b && b->backend_ready) {
				int d = axkvm_yield_to_vcpu_diag(b);

				if (d == AXKVM_YIELD_BOOSTED) {
					boosted = true;
					ctrl_diag = d;
				} else {
					dir_idx = bringup;
				}
			}
		}
	}

	/*
	 * Priority 1: a spinning AP hands its CPU to the boot controller it is
	 * waiting on for the online handshake.
	 *
	 * This is correct ONLY while SMP bringup is still in flight: an AP that
	 * spins in cpuhp_ap_sync_alive is genuinely blocked on the controller.
	 * boot_controller_id stays pinned to the BSP forever after the last
	 * CPU_UP, so post-bringup this branch would wrongly force every park
	 * toward the (already on-core, spinning) BSP -- observed as
	 * directed_yield dir_idx=0 boosted=0 while a CSD-wait deadlock starved
	 * the runnable band vcpu10..18. Once bringup is done we fall straight to
	 * the Priority-2 round-robin over RUNNABLE siblings, mirroring KVM's
	 * kvm_vcpu_on_spin(), which has no "controller" concept and simply
	 * round-robins vCPU[N+1]..vCPU[N-1].
	 */
	controller = READ_ONCE(vm->boot_controller_id);
	if (!boosted && READ_ONCE(vm->current_bringup_target) >= 0 &&
	    controller >= 0 &&
	    (unsigned int)controller != cur_vcpu_id &&
	    controller < AXKVM_MAX_VCPUS) {
		struct axkvm_vcpu *c = vm->vcpus[controller];

		if (c && c->backend_ready) {
			ctrl_diag = axkvm_yield_to_vcpu_diag(c);
			if (ctrl_diag == AXKVM_YIELD_BOOSTED)
				boosted = true;
			else
				dir_idx = controller;
		}
	}

	/* Priority 2: round-robin over RUNNABLE siblings. */
	if (!boosted) {
		start = vm->last_boosted_vcpu;
		for (i = 0; i < AXKVM_MAX_VCPUS; i++) {
			unsigned int idx = (start + 1 + i) % AXKVM_MAX_VCPUS;
			struct axkvm_vcpu *t = vm->vcpus[idx];

			if (!t || idx == cur_vcpu_id || !t->backend_ready)
				continue;
			if (READ_ONCE(t->mp_state.mp_state) !=
			    KVM_MP_STATE_RUNNABLE)
				continue;

			runnable_seen++;
			if (dir_idx < 0)
				dir_idx = idx;

			/*
			 * yield_to() itself returns 0 without boosting if the
			 * target is already running or cannot be yielded to, so
			 * no task_curr() pre-check is needed (and task_curr is
			 * not exported to modules).
			 */
			last_diag = axkvm_yield_to_vcpu_diag(t);
			if (last_diag == AXKVM_YIELD_BOOSTED) {
				vm->last_boosted_vcpu = idx;
				boosted = true;
			}
			if (boosted)
				break;
		}
	}

	/*
	 * Grab the software-directed target pointer while still under vm->lock
	 * (the vcpus[] array is stable under it); the actual wake/boost/park runs
	 * after unlock because set_user_nice / schedule_timeout must not run with
	 * the mutex held. The vCPU object outlives this call (freed only on VM
	 * teardown), and the target task is re-resolved via refcounted run_pid.
	 */
	{
		struct axkvm_vcpu *dir = (!boosted && dir_idx >= 0 &&
					  dir_idx < AXKVM_MAX_VCPUS) ?
						 vm->vcpus[dir_idx] : NULL;
		int bringup = READ_ONCE(vm->current_bringup_target);

		dir_is_bringup_target = dir && bringup >= 0 &&
					dir_idx == bringup;

		/*
		 * Advance the round-robin cursor to the directed target even when
		 * yield_to() failed and we fall back to the software park. KVM only
		 * advances last_boosted_vcpu on a successful yield_to()
		 * (kvm_vcpu_on_spin), but KVM has no software-park fallback: under
		 * our all-fail-yield_to regime the Priority-2 scan would otherwise
		 * resolve dir_idx to the SAME first RUNNABLE sibling every call, so
		 * every park boosted one vCPU (observed: dir_idx=1 forever) while the
		 * rest of the starved band never got a core. Rotating here spreads
		 * the park across all runnable siblings.
		 */
		if (!boosted && dir)
			vm->last_boosted_vcpu = dir_idx;

		mutex_unlock(&vm->lock);

		if (!boosted) {
			/*
			 * yield_to() could not hand off the core (oversubscription:
			 * spinner and target each own a core). Skew CFS toward the
			 * directed target by boosting it. We do NOT park the current
			 * vCPU here: the sole caller (soft_ple_maybe_park in the Rust
			 * bridge) parks the confirmed spinner via
			 * axvisor_kvm_x86_bridge_park_now() when we return true, which
			 * is the real core hand-off. Parking here too would double-park
			 * (two ticks).
			 */
			if (dir_is_bringup_target)
				axkvm_wake_boost_bringup_target(dir);
			else if (dir)
				axkvm_wake_boost_vcpu(dir);
		}

		/*
		 * Tell the caller whether parking is worthwhile. Only when a
		 * starved peer can actually use this core:
		 *   - dir != NULL: a directed target (boot controller / waited-on
		 *     AP) exists but yield_to() could not boost it; free the core
		 *     so CFS runs it.
		 *   - runnable_seen > 0: at least one RUNNABLE sibling was scanned;
		 *     freeing the core lets CFS pick one.
		 * When boosted, yield_to() already handed off — no park needed.
		 * When dir == NULL AND runnable_seen == 0 (the BSP spinning alone
		 * in single-threaded early boot, before any AP is RUNNABLE),
		 * parking would only stall the one vCPU making progress, so return
		 * false and let it keep its core.
		 */
		should_park = !boosted && (dir != NULL || runnable_seen > 0);
	}

	axkvm_vm_put(vm);
	return should_park;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_directed_yield);

/*
 * Demote the current vCPU's KVM_RUN thread to SCHED_IDLE because the Rust run
 * loop has confirmed it is spinning in a guest busy-poll under oversubscription
 * (a stable <=2 RIP-window streak, e.g. cpuhp_ap_sync_alive). This is the core
 * hand-off: with the spinner at SCHED_IDLE, the nice-boosted BSP (or any
 * runnable sibling / L1 kernel thread) placed on this core preempts it, so SMP
 * bringup can advance. Uses the lockless registry lookup (VM is live during
 * guest runtime). No-op if the VM cannot be resolved.
 */
void axvisor_kvm_x86_bridge_spin_demote(u64 backend_vm, u32 vcpu_id)
{
	struct axkvm_vm *vm;

	if (vcpu_id >= AXKVM_MAX_VCPUS)
		return;
	vm = axkvm_lookup_backend_vm_locklessly(backend_vm);
	if (!vm) {
		struct axkvm_vcpu *sender = this_cpu_read(axkvm_running_vcpu);

		if (sender)
			vm = sender->vm;
	}
	if (!vm)
		return;
	if (READ_ONCE(vm->current_bringup_target) == (int)vcpu_id)
		return;
	axkvm_spin_demote(READ_ONCE(vm->vcpus[vcpu_id]));
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_spin_demote);

/*
 * Restore the current vCPU's KVM_RUN thread from SCHED_IDLE back to
 * SCHED_NORMAL nice 0. Called by the Rust run loop the moment the guest RIP
 * leaves the confirmed spin window (real forward progress) and on the HLT
 * settle path. Idempotent (no-op if not demoted). Never depends on a
 * timer/work path, which is itself starved under the oversubscription wedge.
 */
void axvisor_kvm_x86_bridge_spin_restore(u64 backend_vm, u32 vcpu_id)
{
	struct axkvm_vm *vm;

	if (vcpu_id >= AXKVM_MAX_VCPUS)
		return;
	vm = axkvm_lookup_backend_vm_locklessly(backend_vm);
	if (!vm) {
		struct axkvm_vcpu *sender = this_cpu_read(axkvm_running_vcpu);

		if (sender)
			vm = sender->vm;
	}
	if (!vm)
		return;
	axkvm_spin_restore(READ_ONCE(vm->vcpus[vcpu_id]));
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_spin_restore);

/*
 * Park a non-critical AP out of the run queue to free its L1 core for the
 * starved bringup target AP / BSP. Called by the Rust run loop in the
 * oversubscription branch AFTER migrate_disable is dropped and the VMCS is
 * unloaded (the only safe point to block).
 *
 * Eligibility is re-checked under vm->lock. There are two legal cases:
 *   - confirmed spinner after bringup: soft-PLE has already demoted this vCPU.
 *
 * During SMP bringup, do not block-park APs out of the runqueue. The current
 * bringup target is explicitly RT-boosted, and the caller will still execute a
 * runnable schedule_now() hand-off. Sleeping non-target APs for a jiffy removes
 * them from CFS's balancing set and was observed to strand APs for seconds.
 *
 * The block itself is done OUTSIDE the lock -- we never schedule while holding
 * vm->lock.
 *
 * Returns 1 if the thread was parked and woken (caller skips schedule_now, the
 * core was already yielded); 0 if not eligible (caller falls back to
 * schedule_now); -EINTR if the run must abort.
 */
int axvisor_kvm_x86_bridge_spin_park(u64 backend_vm, u32 vcpu_id)
{
	struct axkvm_vm *vm;
	struct axkvm_vcpu *vcpu;
	int controller, target;
	bool confirmed_spinner;
	int eligible;

	if (vcpu_id >= AXKVM_MAX_VCPUS)
		return 0;
	vm = axkvm_lookup_backend_vm_locklessly(backend_vm);
	if (!vm) {
		struct axkvm_vcpu *sender = this_cpu_read(axkvm_running_vcpu);

		if (sender)
			vm = sender->vm;
	}
	if (!vm)
		return 0;

	mutex_lock(&vm->lock);
	vcpu = READ_ONCE(vm->vcpus[vcpu_id]);
	controller = READ_ONCE(vm->boot_controller_id);
	target = READ_ONCE(vm->current_bringup_target);
	if (target >= 0) {
		mutex_unlock(&vm->lock);
		return 0;
	}
	confirmed_spinner = vcpu && READ_ONCE(vcpu->spin_demoted);
	eligible = vcpu && vcpu->backend_ready &&
		   (int)vcpu_id != controller &&
		   confirmed_spinner;
	mutex_unlock(&vm->lock);
	if (!eligible)
		return 0;

	{
		int ret = axkvm_spin_park(vm, vcpu);

		if (ret < 0)
			return ret;   /* -EINTR: abort the run */
		return 1;             /* parked and woken: core was yielded */
	}
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_spin_park);

/*
 * DIAG (gvisor signal-interruptibility): return 1 if the calling KVM_RUN thread
 * has a pending signal, else 0. Used by a bounded probe in the Rust run loop to
 * bracket each vmresume and establish whether gvisor's SIGURG bounce signal is
 * (a) delivered to the vCPU thread at all and (b) observable at the run-loop
 * boundary. Remove after the signal-kick path is verified.
 */
int axvisor_kvm_x86_bridge_signal_pending(void)
{
	return signal_pending(current) ? 1 : 0;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_signal_pending);

/*
 * Fault-in entry point for the Rust bridge's NestedPageFault handler. Resolves
 * the VM (lockless registry, falling back to the per-CPU running vCPU when the
 * caller lacks the handle), then faults in the page backing @gpa under vm->lock.
 * Returns 0 (guest may re-enter) or a negative errno (unmapped GPA / OOM).
 */
int axvisor_kvm_x86_bridge_fault_in_gpa(u64 backend_vm, u64 gpa, u32 write)
{
	struct axkvm_vm *vm;
	int ret;

	vm = axkvm_lookup_backend_vm_locklessly(backend_vm);
	if (!vm) {
		struct axkvm_vcpu *sender = this_cpu_read(axkvm_running_vcpu);

		if (sender)
			vm = sender->vm;
	}
	if (!vm)
		return -EINVAL;

	mutex_lock(&vm->lock);
	ret = axkvm_fault_in_gpa(vm, gpa, write != 0);
	mutex_unlock(&vm->lock);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_x86_bridge_fault_in_gpa);
#endif

static void axkvm_vcpu_release_kref(struct kref *kref)
{
	struct axkvm_vcpu *vcpu = container_of(kref, struct axkvm_vcpu, refcount);

	if (vcpu->backend_ready)
		axvisor_kvm_backend_destroy_vcpu(vcpu->backend_vcpu);
	if (vcpu->run_pages)
		free_pages(vcpu->run_pages, get_order(AXKVM_VCPU_MMAP_SIZE));
#ifdef CONFIG_X86_64
	cancel_delayed_work_sync(&vcpu->boost_watchdog);
	axkvm_spin_restore(vcpu);
	if (vcpu->run_pid)
		put_pid(vcpu->run_pid);
#endif
	axkvm_vm_put(vcpu->vm);
	kfree(vcpu);
}

static void axkvm_vcpu_put(struct axkvm_vcpu *vcpu)
{
	kref_put(&vcpu->refcount, axkvm_vcpu_release_kref);
}

static void axkvm_fill_fail_entry(struct axkvm_vcpu *vcpu, u64 reason)
{
	u8 request_interrupt_window = vcpu->run->request_interrupt_window;
	u8 immediate_exit = READ_ONCE(vcpu->run->immediate_exit__unsafe);

	memset(vcpu->run, 0, sizeof(*vcpu->run));
	vcpu->run->request_interrupt_window = request_interrupt_window;
	WRITE_ONCE(vcpu->run->immediate_exit__unsafe, immediate_exit);
	vcpu->run->exit_reason = KVM_EXIT_FAIL_ENTRY;
	vcpu->run->fail_entry.hardware_entry_failure_reason = reason;
	vcpu->run->fail_entry.cpu = vcpu->id;
}

static void axkvm_fill_internal_error(struct axkvm_vcpu *vcpu, long err)
{
	u8 request_interrupt_window = vcpu->run->request_interrupt_window;
	u8 immediate_exit = READ_ONCE(vcpu->run->immediate_exit__unsafe);

	memset(vcpu->run, 0, sizeof(*vcpu->run));
	vcpu->run->request_interrupt_window = request_interrupt_window;
	WRITE_ONCE(vcpu->run->immediate_exit__unsafe, immediate_exit);
	vcpu->run->exit_reason = KVM_EXIT_INTERNAL_ERROR;
	vcpu->run->internal.suberror = 0;
	vcpu->run->internal.ndata = 1;
	vcpu->run->internal.data[0] = err;
}

#ifdef CONFIG_X86_64
static u64 axkvm_ioeventfd_data_mask(u32 width)
{
	switch (width) {
	case 1:
		return U8_MAX;
	case 2:
		return U16_MAX;
	case 4:
		return U32_MAX;
	case 8:
		return U64_MAX;
	default:
		return 0;
	}
}

static bool axkvm_ioeventfd_match(const struct axkvm_eventfd_binding *binding,
				  const struct axkvm_backend_exit *exit)
{
	u64 mask;

	if (!binding->valid)
		return false;
	if (binding->flags & KVM_IOEVENTFD_FLAG_PIO)
		return false;
	if (binding->addr != exit->addr)
		return false;
	if (binding->len && binding->len != exit->width)
		return false;
	if (!(binding->flags & KVM_IOEVENTFD_FLAG_DATAMATCH))
		return true;

	mask = axkvm_ioeventfd_data_mask(exit->width);
	return mask && ((binding->datamatch ^ exit->data) & mask) == 0;
}

static bool axkvm_signal_ioeventfd(struct axkvm_vcpu *vcpu,
				   const struct axkvm_backend_exit *exit)
{
	struct axkvm_vm *vm = vcpu->vm;
	unsigned int i;
	bool signaled = false;

	if (exit->reason != AXKVM_BACKEND_EXIT_MMIO_WRITE)
		return false;
	if (!eventfd_signal_allowed())
		return false;

	mutex_lock(&vm->lock);
	for (i = 0; i < AXKVM_MAX_IOEVENTS; i++) {
		struct axkvm_eventfd_binding *binding = &vm->ioevents[i];

		if (!axkvm_ioeventfd_match(binding, exit))
			continue;

		eventfd_signal(binding->ctx);
		{
			u64 count =
				atomic64_inc_return(&binding->signal_count);

			if (axkvm_trace_count(count))
				pr_info("ioeventfd signal fd=%d addr=%#llx len=%u data=%#llx count=%llu\n",
					binding->fd, binding->addr,
					binding->len, exit->data, count);
		}
		signaled = true;
		break;
	}
	mutex_unlock(&vm->lock);

	return signaled;
}

static int axkvm_handle_cpu_up(struct axkvm_vcpu *source,
			       const struct axkvm_backend_exit *exit)
{
	struct axkvm_vm *vm = source->vm;
	struct axkvm_vcpu *target = NULL;
	u32 target_lapic_id = (u32)exit->addr;
	unsigned int i;

	if (exit->reason != AXKVM_BACKEND_EXIT_CPU_UP)
		return 0;

	mutex_lock(&vm->lock);
	/*
	 * Remember who is driving SMP bringup. An AP later spinning in
	 * cpuhp_ap_sync_alive() is waiting for this controller to release it, so
	 * a spinning AP directed-yields here first (see directed_yield below).
	 */
	WRITE_ONCE(vm->boot_controller_id, (int)source->id);
	for (i = 0; i < AXKVM_MAX_VCPUS; i++) {
		struct axkvm_vcpu *candidate = vm->vcpus[i];

		if (candidate && axkvm_lapic_id(&candidate->lapic) == target_lapic_id) {
			target = candidate;
			break;
		}
	}
	mutex_unlock(&vm->lock);

	if (!target) {
		pr_err("CPU_UP target not found source_vcpu=%u target_lapic=%u entry=%#llx\n",
		       source->id, target_lapic_id, exit->data);
		axkvm_fill_internal_error(source, -ENOENT);
		return -ENOENT;
	}

	/*
	 * Under oversubscription (guest vCPUs > host CPUs) Linux 6.x parallel
	 * bringup kicks every AP at once, then each must report SYNC_STATE_ALIVE
	 * within the boot CPU's per-AP ~10s window. If all 31 APs become runnable
	 * together on 18 cores, the fair-share tail is starved past its window and
	 * spins in cpuhp_ap_sync_alive() forever. Instead of racing them all, queue
	 * this AP in kick order and admit only a bounded batch at a time
	 * (axkvm_ap_enqueue -> axkvm_admit_next_ap). Admitted APs are nice-boosted
	 * so they reach ALIVE fast; the next AP is admitted as each one reaches the
	 * ALIVE wait loop (or first HLT as a fallback). This keeps the number of APs
	 * contending for CPU inside the bringup critical section at ~= host cores,
	 * without lowering the guest's vCPU count.
	 *
	 * Enqueue (which sets boot_state before we set mp_state RUNNABLE) must
	 * happen before waking mp_state_wq: the AP's KVM_RUN thread, once it sees
	 * RUNNABLE, immediately consults boot_state in axkvm_ap_wait_admitted(); if
	 * enqueue had not yet run it would read AP_BOOT_NONE and skip the throttle.
	 */
	mutex_lock(&vm->lock);
	axkvm_ap_enqueue(vm, target);
	/*
	 * Record the AP the BSP is now blocking on so a spinning BSP can
	 * directed-yield precisely to it (Priority 0 in directed_yield) instead
	 * of a random RUNNABLE sibling. Serial bringup means this is the single
	 * AP whose online-report the whole SMP bringup is currently waiting for.
	 */
	WRITE_ONCE(vm->current_bringup_target, (int)target->id);
	/* (a) The bringup target just changed; wake parked spinners so the freed
	 * core can be pulled to the new target. vm->lock held. */
	axkvm_wake_parked_spinners(vm);
	mutex_unlock(&vm->lock);

	mutex_lock(&target->lock);
	WRITE_ONCE(target->mp_state.mp_state, KVM_MP_STATE_RUNNABLE);
	/*
	 * Do not mark backend_state_dirty here. The Rust bridge has already
	 * rebuilt the AP VMCS from SIPI; replaying userspace's initial vCPU
	 * state would overwrite that real-mode trampoline state.
	 */
	mutex_unlock(&target->lock);
	wake_up_all(&target->mp_state_wq);
	axkvm_wake_boost_bringup_target(target);
	if (axkvm_trace_count(source->backend_cpu_ups + 1))
		pr_info("CPU_UP source_vcpu=%u target_vcpu=%u target_lapic=%u entry=%#llx admitted=%u budget=%u\n",
			source->id, target->id, target_lapic_id, exit->data,
			vm->ap_admitted, axkvm_effective_ap_budget());

	return 1;
}
#else
static bool axkvm_signal_ioeventfd(struct axkvm_vcpu *vcpu,
				   const struct axkvm_backend_exit *exit)
{
	return false;
}

static int axkvm_handle_cpu_up(struct axkvm_vcpu *source,
			       const struct axkvm_backend_exit *exit)
{
	return 0;
}
#endif

static void axkvm_translate_backend_exit(struct axkvm_vcpu *vcpu,
					 const struct axkvm_backend_exit *exit)
{
	u8 request_interrupt_window = vcpu->run->request_interrupt_window;
	u8 immediate_exit = READ_ONCE(vcpu->run->immediate_exit__unsafe);

	memset(vcpu->run, 0, sizeof(*vcpu->run));
	vcpu->run->request_interrupt_window = request_interrupt_window;
	WRITE_ONCE(vcpu->run->immediate_exit__unsafe, immediate_exit);
	vcpu->pending_mmio_read = false;
	vcpu->pending_mmio_read_len = 0;
	vcpu->pending_io_read = false;
	vcpu->pending_io_read_len = 0;
	vcpu->pending_io_read_offset = 0;

	switch (exit->reason) {
	case AXKVM_BACKEND_EXIT_MMIO_READ:
		if (!exit->width || exit->width > sizeof(vcpu->run->mmio.data)) {
			axkvm_fill_internal_error(vcpu, -EINVAL);
			break;
		}
		vcpu->run->exit_reason = KVM_EXIT_MMIO;
		vcpu->run->mmio.phys_addr = exit->addr;
		vcpu->run->mmio.len = exit->width;
		vcpu->run->mmio.is_write = 0;
		vcpu->pending_mmio_read = true;
		vcpu->pending_mmio_read_len = exit->width;
		break;
	case AXKVM_BACKEND_EXIT_MMIO_WRITE:
		if (!exit->width || exit->width > sizeof(vcpu->run->mmio.data)) {
			axkvm_fill_internal_error(vcpu, -EINVAL);
			break;
		}
		vcpu->run->exit_reason = KVM_EXIT_MMIO;
		vcpu->run->mmio.phys_addr = exit->addr;
		vcpu->run->mmio.len = exit->width;
		vcpu->run->mmio.is_write = 1;
		memcpy(vcpu->run->mmio.data, &exit->data,
		       min_t(u32, exit->width, sizeof(vcpu->run->mmio.data)));
		break;
	case AXKVM_BACKEND_EXIT_IO_READ:
		if (!exit->width ||
		    sizeof(*vcpu->run) + exit->width > AXKVM_VCPU_MMAP_SIZE) {
			axkvm_fill_internal_error(vcpu, -EINVAL);
			break;
		}
		vcpu->run->exit_reason = KVM_EXIT_IO;
		vcpu->run->io.direction = KVM_EXIT_IO_IN;
		vcpu->run->io.size = exit->width;
		vcpu->run->io.port = exit->addr;
		vcpu->run->io.count = 1;
		vcpu->run->io.data_offset = sizeof(*vcpu->run);
		vcpu->pending_io_read = true;
		vcpu->pending_io_read_len = exit->width;
		vcpu->pending_io_read_offset = sizeof(*vcpu->run);
		break;
	case AXKVM_BACKEND_EXIT_IO_WRITE:
		if (!exit->width ||
		    sizeof(*vcpu->run) + exit->width > AXKVM_VCPU_MMAP_SIZE) {
			axkvm_fill_internal_error(vcpu, -EINVAL);
			break;
		}
		vcpu->run->exit_reason = KVM_EXIT_IO;
		vcpu->run->io.direction = KVM_EXIT_IO_OUT;
		vcpu->run->io.size = exit->width;
		vcpu->run->io.port = exit->addr;
		vcpu->run->io.count = 1;
		vcpu->run->io.data_offset = sizeof(*vcpu->run);
		memcpy((char *)vcpu->run + vcpu->run->io.data_offset, &exit->data,
		       min_t(u32, exit->width,
			     AXKVM_VCPU_MMAP_SIZE - sizeof(*vcpu->run)));
		break;
	case AXKVM_BACKEND_EXIT_HLT:
		vcpu->run->exit_reason = KVM_EXIT_HLT;
		break;
	case AXKVM_BACKEND_EXIT_SHUTDOWN:
		vcpu->run->exit_reason = KVM_EXIT_SHUTDOWN;
		break;
	case AXKVM_BACKEND_EXIT_FAIL_ENTRY:
		axkvm_fill_fail_entry(vcpu, exit->hardware_entry_failure_reason);
		break;
	default:
		axkvm_fill_internal_error(vcpu, -EIO);
		break;
	}
}

#ifdef CONFIG_X86_64
static void axkvm_vcpu_backend_state(struct axkvm_vcpu *vcpu,
				     struct axkvm_backend_vcpu_state *state)
{
	u32 i;

	memset(state, 0, sizeof(*state));
	state->version = AXKVM_BACKEND_STATE_VERSION;
	state->arch = AXKVM_BACKEND_ARCH_X86_64;
	state->rip = vcpu->regs.rip;
	state->rsp = vcpu->regs.rsp;
	state->rflags = vcpu->regs.rflags;
	state->cr0 = vcpu->sregs.cr0;
	state->cr3 = vcpu->sregs.cr3;
	state->cr4 = vcpu->sregs.cr4;
	state->efer = vcpu->sregs.efer;
	state->apic_base = vcpu->sregs.apic_base;
	state->xcr0 = 1;
	for (i = 0; i < vcpu->xcrs.nr_xcrs; i++) {
		if (vcpu->xcrs.xcrs[i].xcr == 0) {
			state->xcr0 = vcpu->xcrs.xcrs[i].value;
			break;
		}
	}
	state->regs = &vcpu->regs;
	state->sregs = &vcpu->sregs;
	state->fpu_valid = vcpu->fpu_valid;
	state->fpu = &vcpu->fpu;
	state->lapic = &vcpu->lapic;
	state->mp_state = &vcpu->mp_state;
	state->debugregs = &vcpu->debugregs;
	state->xsave_valid = vcpu->xsave_valid;
	state->xsave = &vcpu->xsave;
	state->xcrs = &vcpu->xcrs;
	state->events = &vcpu->events;
	state->cpuid_entries = vcpu->cpuid_entries;
	state->cpuid_nent = vcpu->cpuid_nent;
	state->msrs = vcpu->msrs;
	state->nmsrs = vcpu->nmsrs;
	state->tsc_khz = vcpu->tsc_khz;
}
#else
static void axkvm_vcpu_backend_state(struct axkvm_vcpu *vcpu,
				     struct axkvm_backend_vcpu_state *state)
{
	memset(state, 0, sizeof(*state));
	state->version = AXKVM_BACKEND_STATE_VERSION;
	state->arch = AXKVM_BACKEND_ARCH_UNKNOWN;
}
#endif

static int axkvm_vcpu_sync_backend_state(struct axkvm_vcpu *vcpu,
					 bool force)
{
	struct axkvm_backend_vcpu_state state;
	int ret;

	if (!vcpu->backend_ready)
		return 0;

#ifdef CONFIG_X86_64
	mutex_lock(&vcpu->lock);
	if (!force && !vcpu->backend_state_dirty) {
		mutex_unlock(&vcpu->lock);
		return 0;
	}
	axkvm_vcpu_backend_state(vcpu, &state);
	mutex_unlock(&vcpu->lock);
#else
	axkvm_vcpu_backend_state(vcpu, &state);
#endif

	ret = axvisor_kvm_backend_set_vcpu_state(vcpu->backend_vcpu, &state);
	if (ret && ret != -EOPNOTSUPP)
		return ret;

#ifdef CONFIG_X86_64
	mutex_lock(&vcpu->lock);
	vcpu->backend_state_dirty = false;
	mutex_unlock(&vcpu->lock);
#endif
	return 0;
}

static int axkvm_vm_sync_all_vcpu_backend_states_locked(struct axkvm_vm *vm,
							bool force)
{
	unsigned int i;
	int ret;

	for (i = 0; i < AXKVM_MAX_VCPUS; i++) {
		struct axkvm_vcpu *vcpu = vm->vcpus[i];

		if (!vcpu)
			continue;

		ret = axkvm_vcpu_sync_backend_state(vcpu, force);
		if (ret)
			return ret;
	}

	return 0;
}

static int axkvm_vcpu_complete_pending_reads(struct axkvm_vcpu *vcpu)
{
	int ret;

	if (vcpu->pending_mmio_read) {
		u32 len = vcpu->pending_mmio_read_len;

		ret = axvisor_kvm_backend_complete_mmio_read(
			vcpu->backend_vcpu, vcpu->run->mmio.data, len);
		vcpu->pending_mmio_read = false;
		vcpu->pending_mmio_read_len = 0;
		if (ret) {
			pr_err("complete_mmio_read failed vcpu=%u ret=%d len=%u\n",
			       vcpu->id, ret, len);
			axkvm_fill_internal_error(vcpu, ret);
			return ret;
		}
	}

	if (vcpu->pending_io_read) {
		u32 len = vcpu->pending_io_read_len;
		u32 offset = vcpu->pending_io_read_offset;

		ret = axvisor_kvm_backend_complete_io_read(
			vcpu->backend_vcpu, (char *)vcpu->run + offset, len);
		vcpu->pending_io_read = false;
		vcpu->pending_io_read_len = 0;
		vcpu->pending_io_read_offset = 0;
		if (ret) {
			pr_err("complete_io_read failed vcpu=%u ret=%d len=%u offset=%u\n",
			       vcpu->id, ret, len, offset);
			axkvm_fill_internal_error(vcpu, ret);
			return ret;
		}
	}

	return 0;
}

static int axkvm_vcpu_run_backend_unmasked(struct axkvm_vcpu *vcpu)
{
	struct axkvm_backend_vm_state vm_state;
	struct axkvm_backend_exit exit = {};
	struct axkvm_vm *vm = vcpu->vm;
	int ret;

	if (READ_ONCE(vcpu->run->immediate_exit__unsafe) ||
	    signal_pending(current))
		return -EINTR;

	if (!vcpu->backend_ready) {
		axkvm_fill_fail_entry(vcpu, 0);
		return 0;
	}

	ret = axkvm_vcpu_complete_pending_reads(vcpu);
	if (ret)
		return 0;

#ifdef CONFIG_X86_64
	if (READ_ONCE(vcpu->mp_state.mp_state) == KVM_MP_STATE_UNINITIALIZED) {
		pr_info_ratelimited("KVM_RUN wait AP vcpu=%u mp_state=%u lapic_id=%x\n",
				    vcpu->id, vcpu->mp_state.mp_state,
				    axkvm_lapic_id(&vcpu->lapic));
		while (READ_ONCE(vcpu->mp_state.mp_state) ==
		       KVM_MP_STATE_UNINITIALIZED) {
			ret = wait_event_interruptible(
				vcpu->mp_state_wq,
				READ_ONCE(vcpu->mp_state.mp_state) ==
					KVM_MP_STATE_RUNNABLE ||
					READ_ONCE(vcpu->run->immediate_exit__unsafe));
			if (ret < 0) {
				pr_info_ratelimited("KVM_RUN AP wait interrupted vcpu=%u ret=%d mp_state=%u immediate_exit=%u signal_pending=%d\n",
						    vcpu->id, ret,
						    READ_ONCE(vcpu->mp_state.mp_state),
						    READ_ONCE(vcpu->run->immediate_exit__unsafe),
						    signal_pending(current));
				return -EINTR;
			}
			if (READ_ONCE(vcpu->run->immediate_exit__unsafe) ||
			    signal_pending(current)) {
				pr_info_ratelimited("KVM_RUN AP wait exit vcpu=%u mp_state=%u immediate_exit=%u signal_pending=%d\n",
						    vcpu->id,
						    READ_ONCE(vcpu->mp_state.mp_state),
						    READ_ONCE(vcpu->run->immediate_exit__unsafe),
						    signal_pending(current));
				return -EINTR;
			}
		}
		pr_info_ratelimited("KVM_RUN AP runnable vcpu=%u lapic_id=%x\n",
				    vcpu->id, axkvm_lapic_id(&vcpu->lapic));
	}

	/*
	 * SMP bringup admission throttle: an AP that has been kicked (CPU_UP) but
	 * not yet admitted blocks here so it does not race the already-admitted
	 * batch for host CPUs. It is released (in kick order) as earlier APs settle
	 * -- see axkvm_admit_next_ap / axkvm_ap_settle. The BSP and any AP not under
	 * throttle return from this immediately.
	 */
	ret = axkvm_ap_wait_admitted(vcpu);
	if (ret < 0)
		return -EINTR;

	mutex_lock(&vm->lock);
#endif
	if (!vm->backend_booted) {
		axkvm_trace_vcpu_backend_state("KVM_RUN boot path", vcpu);
		axkvm_vm_backend_state(vm, &vm_state);
		ret = axvisor_kvm_backend_set_vm_state(vm->backend_vm,
						       &vm_state);
		if (ret && ret != -EOPNOTSUPP) {
#ifdef CONFIG_X86_64
			mutex_unlock(&vm->lock);
#endif
			pr_err("set_vm_state failed vcpu=%u ret=%d\n",
			       vcpu->id, ret);
			axkvm_fill_internal_error(vcpu, ret);
			return 0;
		}

		ret = axkvm_vm_sync_all_vcpu_backend_states_locked(vm, true);
		if (ret) {
#ifdef CONFIG_X86_64
			mutex_unlock(&vm->lock);
#endif
			pr_err("initial set_vcpu_state failed ret=%d\n", ret);
			axkvm_fill_internal_error(vcpu, ret);
			return 0;
		}

		ret = axvisor_kvm_backend_boot_vm(vm->backend_vm);
		if (ret) {
#ifdef CONFIG_X86_64
			mutex_unlock(&vm->lock);
#endif
			pr_err("boot_vm failed vcpu=%u ret=%d\n", vcpu->id, ret);
			axkvm_fill_internal_error(vcpu, ret);
			return 0;
		}
		vm->backend_booted = true;
	}
#ifdef CONFIG_X86_64
	mutex_unlock(&vm->lock);
#endif

	ret = axkvm_vcpu_sync_backend_state(vcpu, false);
	if (ret) {
		pr_err("set_vcpu_state failed vcpu=%u ret=%d\n", vcpu->id, ret);
		axkvm_fill_internal_error(vcpu, ret);
		return 0;
	}
	axkvm_trace_vcpu_backend_state("KVM_RUN enter backend", vcpu);

	for (;;) {
		if (READ_ONCE(vcpu->run->immediate_exit__unsafe) ||
		    signal_pending(current))
			return -EINTR;

		ret = axkvm_vcpu_drain_pending_irqs(vcpu);
		if (ret) {
			axkvm_fill_internal_error(vcpu, ret);
			return 0;
		}

		/*
		 * Safe point: convert any IPI-boost hint recorded during the
		 * previous backend run (in the atomic injection path) into a real
		 * yield_to(). Scheduling is legal here; a no-op when no hint.
		 */
		axkvm_vcpu_drain_boost(vcpu);

		vcpu->backend_run_calls++;
		if (axkvm_trace_count(vcpu->backend_run_calls))
			pr_info("KVM_RUN backend call vcpu=%u count=%llu\n",
				vcpu->id, vcpu->backend_run_calls);

		/*
		 * DIAG (bounded): if a wake was delivered to us and we are now
		 * about to VM-enter, the host DID schedule us -- count the edge.
		 * dbg_wake without matching dbg_run_after_wake for CPU 15/18 is the
		 * signature of candidate B (host scheduling starvation).
		 */
		if (atomic_xchg(&vcpu->dbg_wake_pending, 0))
			atomic_inc(&vcpu->dbg_run_after_wake);

		/*
		 * Publish the running vCPU for this CPU so the atomic injection
		 * path can find the sender. The backend disables preemption
		 * around the guest run, so this stays consistent on-CPU.
		 */
		this_cpu_write(axkvm_running_vcpu, vcpu);
		ret = axvisor_kvm_backend_run_vcpu(vcpu->backend_vcpu, &exit);
		this_cpu_write(axkvm_running_vcpu, NULL);
		if (ret) {
			if (ret == -EOPNOTSUPP) {
				axkvm_fill_fail_entry(vcpu, 0);
			} else {
				pr_err("run_vcpu failed vcpu=%u ret=%d\n",
				       vcpu->id, ret);
				axkvm_fill_internal_error(vcpu, ret);
			}
			return 0;
		}

		ret = axkvm_handle_cpu_up(vcpu, &exit);
		if (ret < 0)
			return 0;
		if (ret > 0) {
			vcpu->backend_cpu_ups++;
			/*
			 * CPU_UP is an in-kernel LAPIC/SIPI side effect. The BSP
			 * continues its own KVM_RUN; Linux may schedule the AP
			 * thread at this weak checkpoint.
			 */
			if (axkvm_trace_count(vcpu->backend_cpu_ups))
				pr_info("CPU_UP source_vcpu=%u schedule checkpoint after AP wake count=%llu backend_calls=%llu\n",
					vcpu->id, vcpu->backend_cpu_ups,
					vcpu->backend_run_calls);
			axkvm_backend_schedule_point();
			continue;
		}

		if (exit.reason == AXKVM_BACKEND_EXIT_HLT) {
			/*
			 * Guest idle HLT is a wait-for-interrupt checkpoint, not
			 * a userspace exit during normal boot. irqfd injection
			 * sets irq_pending_wakeup and wakes halt_wq, matching the
			 * KVM model where HLT resumes when an interrupt is pending.
			 */
#ifdef CONFIG_X86_64
			/*
			 * Reaching HLT means this vCPU has finished bringup and
			 * entered the idle loop (an AP busy-polls with cpu_relax in
			 * cpuhp_ap_sync_alive, so it does not HLT until online). This
			 * first HLT is the conservative "settled" signal: the AP is
			 * guaranteed past SYNC_STATE_ALIVE and fully online. Release its
			 * admission budget (which drops the boost and admits the next
			 * queued AP). settle is a no-op for the BSP / already-settled APs.
			 */
			mutex_lock(&vm->lock);
			axkvm_ap_settle(vm, vcpu);
			mutex_unlock(&vm->lock);
#endif
			/*
			 * KVM-style halt (kvm_vcpu_halt -> kvm_vcpu_block): a short
			 * poll for an imminent wake to avoid the block/unblock cost,
			 * then block releasing the host core until an interrupt wake
			 * arrives. The old 1-jiffy timeout made every idle vCPU wake
			 * ~HZ times/sec even with no event -- under oversubscription
			 * that runqueue churn never freed the core, so a spinning
			 * cross-call initiator (smp_call_function_many_cond) and the
			 * halted RCU GP kthread's CPU both starved. A blocking halt
			 * lets CFS schedule the starved band. Every real wake path
			 * (LAPIC timer callback, IPI/IOAPIC/PIC inject) now calls
			 * wake_vcpu(), so the wait cannot lose an interrupt; the
			 * generous safety-net timeout only backstops a missed wake.
			 */
			{
				int poll = AXKVM_HALT_POLL_ITERS;

				/*
				 * Mirror KVM's halt-poll gate kvm_vcpu_can_poll()
				 * (single_task_running() && !need_resched()):
				 * busy-poll for an imminent wake ONLY while this
				 * host CPU is uncontended and no reschedule is
				 * pending. Under CPU oversubscription a vCPU
				 * thread shares its core with runnable peers
				 * (single_task_running()==false) and/or the tick
				 * sets NEED_RESCHED; continuing to cpu_relax()
				 * there pins the core, starving sibling vCPU
				 * threads and the timer workqueue that drives
				 * per-vCPU LAPIC ticks. Bailing to the blocking
				 * wait immediately frees the core, exactly as
				 * KVM does. A lone vCPU (baseline 1/2/4/8/16,
				 * one-thread-per-core, no contention) keeps the
				 * full low-latency poll and does not regress.
				 */
				while (poll-- > 0 &&
				       single_task_running() &&
				       !need_resched() &&
				       !atomic_read(&vcpu->irq_pending_wakeup) &&
				       !READ_ONCE(vcpu->run->immediate_exit__unsafe) &&
				       !signal_pending(current))
					cpu_relax();
			}
			/*
			 * Mark this vCPU as parked in the HLT wait so the 250us
			 * backend workfn's oversubscription backstop
			 * (axkvm_vm_backstop_halted_vcpus) can give it a periodic,
			 * rate-limited kick -- an idle NO_HZ AP has no armed LAPIC
			 * timer and would otherwise never be re-woken to drain a
			 * late CSD/RCU obligation. Cleared right after the wait.
			 */
			atomic_set(&vcpu->in_halt_wait, 1);
			wait_event_interruptible_timeout(
				vcpu->halt_wq,
				atomic_xchg(&vcpu->irq_pending_wakeup, 0) ||
					READ_ONCE(vcpu->run->immediate_exit__unsafe) ||
					signal_pending(current),
				AXKVM_HALT_BLOCK_TIMEOUT_JIFFIES);
			atomic_set(&vcpu->in_halt_wait, 0);
			continue;
		}

		if (!axkvm_signal_ioeventfd(vcpu, &exit))
			break;

		/*
		 * ioeventfd hands the device request to userspace. Return to the
		 * scheduler so the VMM device thread can consume the eventfd and
		 * later inject completion through irqfd.
		 */
		axkvm_backend_schedule_point();
		return -EINTR;
	}

	axkvm_translate_backend_exit(vcpu, &exit);
	if (vcpu->run->exit_reason == KVM_EXIT_INTERNAL_ERROR)
		pr_err("translated internal error vcpu=%u backend_reason=%u width=%u addr=%#llx data=%#llx\n",
		       vcpu->id, exit.reason, exit.width, exit.addr, exit.data);
	return 0;
}

static int axkvm_vcpu_set_signal_mask(struct axkvm_vcpu *vcpu,
				      void __user *argp)
{
	struct kvm_signal_mask mask_hdr;
	sigset_t sigset;

	if (!argp) {
		vcpu->signal_mask_valid = false;
		pr_info("KVM_SET_SIGNAL_MASK vcpu=%u cleared\n", vcpu->id);
		return 0;
	}

	if (copy_from_user(&mask_hdr, argp, sizeof(mask_hdr)))
		return -EFAULT;
	if (mask_hdr.len != sizeof(sigset_t))
		return -EINVAL;
	if (copy_from_user(&sigset,
			   (char __user *)argp + sizeof(mask_hdr),
			   sizeof(sigset)))
		return -EFAULT;

	sigdelsetmask(&sigset, sigmask(SIGKILL) | sigmask(SIGSTOP));
	vcpu->signal_mask = sigset;
	vcpu->signal_mask_valid = true;
	pr_info("KVM_SET_SIGNAL_MASK vcpu=%u len=%u applied\n",
		vcpu->id, mask_hdr.len);
	return 0;
}

static bool axkvm_vcpu_sigset_activate(struct axkvm_vcpu *vcpu)
{
	sigset_t sigset;

	if (!READ_ONCE(vcpu->signal_mask_valid))
		return false;

	sigset = vcpu->signal_mask;
	sigprocmask(SIG_SETMASK, &sigset, &current->real_blocked);
	return true;
}

static void axkvm_vcpu_sigset_deactivate(bool active)
{
	if (!active)
		return;

	sigprocmask(SIG_SETMASK, &current->real_blocked, NULL);
	sigemptyset(&current->real_blocked);
}

static int axkvm_vcpu_run_backend(struct axkvm_vcpu *vcpu)
{
	bool sigset_active;
	int ret;

#ifdef CONFIG_X86_64
	/*
	 * Record the calling KVM_RUN thread so a sibling vCPU that takes a
	 * PAUSE (PLE) exit can directed-yield the physical CPU to this thread.
	 * Store a refcounted struct pid (not a raw task pointer) so the target
	 * lookup stays safe even if the thread later exits or closes its fd.
	 * Mirrors KVM's per-vCPU pid tracking for kvm_vcpu_yield_to().
	 */
	{
		struct pid *old = vcpu->run_pid;

		vcpu->run_pid = get_task_pid(current, PIDTYPE_PID);
		if (old)
			put_pid(old);
	}

	/*
	 * Refresh this vCPU's kvm-clock page in its own thread context (so
	 * current->mm backs the guest memory) before entering the guest, so
	 * system_time keeps advancing even if the guest never rewrites the MSR.
	 * No-op until the guest has enabled kvm-clock.
	 */
	if (READ_ONCE(vcpu->pvclock_enabled)) {
		mutex_lock(&vcpu->lock);
		axkvm_pvclock_refresh(vcpu);
		mutex_unlock(&vcpu->lock);
	}
#endif

	sigset_active = axkvm_vcpu_sigset_activate(vcpu);
	ret = axkvm_vcpu_run_backend_unmasked(vcpu);
	axkvm_vcpu_sigset_deactivate(sigset_active);
	return ret;
}

#ifdef CONFIG_X86_64
static int axkvm_copy_cpuid_to_user(void __user *argp,
				    const struct kvm_cpuid_entry2 *entries,
				    u32 nent)
{
	struct kvm_cpuid2 hdr;
	u32 user_nent;

	if (copy_from_user(&hdr, argp, sizeof(hdr)))
		return -EFAULT;

	user_nent = hdr.nent;
	hdr.nent = nent;
	if (copy_to_user(argp, &hdr, sizeof(hdr)))
		return -EFAULT;
	if (user_nent < nent)
		return -E2BIG;
	if (copy_to_user((char __user *)argp + sizeof(hdr), entries,
			 array_size(sizeof(*entries), nent)))
		return -EFAULT;

	return 0;
}

static int axkvm_get_supported_cpuid(void __user *argp)
{
	struct kvm_cpuid_entry2 entries[] = {
		{
			.function = 0x0,
			.eax = 0x16,
			.ebx = 0x756e6547,
			.ecx = 0x6c65746e,
			.edx = 0x49656e69,
		},
		{
			.function = 0x1,
			.eax = 0x000306a9,
			.ebx = (8 << 8),
			/*
			 * BIT(24) = TSC_DEADLINE is intentionally NOT advertised:
			 * the guest LAPIC TSC-deadline timer (armed via WRMSR
			 * IA32_TSC_DEADLINE 0x6E0) is not yet virtualized, so the
			 * guest must fall back to the APIC_TMICT one-shot path that
			 * we do implement (register_kvm_timer_on_table).
			 */
			.ecx = BIT(0) | BIT(9) | BIT(19) | BIT(20) |
			       BIT(23) | BIT(25) | BIT(26) |
			       BIT(27) | BIT(28) | BIT(29) | BIT(30),
			.edx = BIT(0) | BIT(3) | BIT(4) | BIT(5) | BIT(6) |
			       BIT(8) | BIT(9) | BIT(11) | BIT(13) | BIT(15) |
			       BIT(23) | BIT(24) | BIT(25) | BIT(26),
		},
		{
			.function = 0x2,
			.eax = 0x76036301,
			.ebx = 0x00f0b5ff,
			.ecx = 0,
			.edx = 0x00c10000,
		},
		{
			.function = 0x4,
			.index = 0,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
			.eax = 0x1c004121,
			.ebx = 0x01c0003f,
			.ecx = 0x0000003f,
		},
		{
			.function = 0x4,
			.index = 1,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
			.eax = 0x1c004122,
			.ebx = 0x01c0003f,
			.ecx = 0x0000003f,
		},
		{
			.function = 0x4,
			.index = 2,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
			.eax = 0x1c004143,
			.ebx = 0x03c0003f,
			.ecx = 0x000003ff,
		},
		{
			.function = 0x4,
			.index = 3,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
			.eax = 0x1c03c163,
			.ebx = 0x03c0003f,
			.ecx = 0x00003fff,
		},
		{
			.function = 0x4,
			.index = 4,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
		},
		{
			.function = 0x6,
		},
		{
			.function = 0x7,
			.index = 0,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
			.ebx = BIT(6) | BIT(13),
		},
		{
			.function = 0xa,
		},
		{
			.function = 0xb,
			.index = 0,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
			.eax = 0,
			.ebx = 1,
			.ecx = 1 << 8,
		},
		{
			.function = 0xb,
			.index = 1,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
			.eax = 7,
			.ebx = 1,
			.ecx = (1 << 0) | (2 << 8),
		},
		{
			.function = 0xb,
			.index = 2,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
			.ecx = 2,
		},
		{
			.function = 0xd,
			.index = 0,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
			.eax = BIT(0) | BIT(1),
			.ebx = 0x240,
			.ecx = 0x240,
		},
		{
			.function = 0xd,
			.index = 1,
			.flags = AXKVM_KVM_CPUID_FLAG_SIGNIFICANT_INDEX,
			.eax = BIT(0),
		},
		{
			.function = 0x15,
			.eax = 1,
			.ebx = 120,
			.ecx = 25000000,
		},
		{
			.function = 0x16,
			.eax = AXKVM_DEFAULT_TSC_KHZ / 1000,
			.ebx = AXKVM_DEFAULT_TSC_KHZ / 1000,
			.ecx = 100,
		},
		{
			.function = 0x80000000,
			.eax = 0x80000008,
		},
		{
			.function = 0x80000001,
			.edx = BIT(11) | BIT(20) | BIT(27) | BIT(29),
			.ecx = BIT(0),
		},
		{
			.function = 0x80000002,
			.eax = 0x65746e49,
			.ebx = 0x2952286c,
			.ecx = 0x6f655820,
			.edx = 0x2952286e,
		},
		{
			.function = 0x80000003,
			.eax = 0x55504320,
			.ebx = 0x20202020,
			.ecx = 0x20202020,
			.edx = 0x20202020,
		},
		{
			.function = 0x80000004,
			.eax = 0x20402020,
			.ebx = 0x30302e32,
			.ecx = 0x007a4847,
			.edx = 0,
		},
		{
			.function = 0x80000005,
			.ecx = 0x01ff01ff,
			.edx = 0x40020140,
		},
		{
			.function = 0x80000006,
			.ecx = 0x01006040,
		},
		{
			.function = 0x80000008,
			.eax = 0x0000302e,
		},
		/*
		 * kvm-clock (pvclock) advertisement intentionally removed.
		 *
		 * Our per-vCPU pvclock page is refreshed only inside each vCPU's
		 * own KVM_RUN thread, so a halted vCPU's page goes stale, and
		 * tsc_timestamp=rdtsc() is sampled on whatever (unsynchronized)
		 * host core the vCPU thread runs on while system_time comes from
		 * ktime_get_boottime_ns(). The result is a non-monotonic,
		 * cross-vCPU-inconsistent clocksource: modern guests remotely read
		 * another CPU's pvclock during the clocksource watchdog, see
		 * kvm-clock disagree with the TSC, mark TSC unstable, and then get
		 * wedged spinning in pvclock_clocksource_read while RCU starves
		 * (observed at 4 vCPUs, no oversubscription). The LAPIC periodic
		 * timer bug that originally motivated kvm-clock is fixed, so the
		 * guest boots fine on its native TSC/PIT/HPET clocksource. Do not
		 * re-expose the KVM CPUID signature until pvclock is made robust
		 * (coherent host time base, seqlock barriers, all-vCPU refresh,
		 * stale-while-halted handling).
		 */
	};

	return axkvm_copy_cpuid_to_user(argp, entries, ARRAY_SIZE(entries));
}

static int axkvm_get_msr_index_list(void __user *argp)
{
	struct kvm_msr_list hdr;
	u32 user_nmsrs;
	u32 nmsrs = ARRAY_SIZE(axkvm_default_msr_indices);

	if (copy_from_user(&hdr, argp, sizeof(hdr)))
		return -EFAULT;

	user_nmsrs = hdr.nmsrs;
	hdr.nmsrs = nmsrs;
	if (copy_to_user(argp, &hdr, sizeof(hdr)))
		return -EFAULT;
	if (user_nmsrs < nmsrs)
		return -E2BIG;
	if (copy_to_user((char __user *)argp + sizeof(hdr),
			 axkvm_default_msr_indices,
			 array_size(sizeof(axkvm_default_msr_indices[0]), nmsrs)))
		return -EFAULT;

	return 0;
}

static int axkvm_vm_create_irqchip(struct axkvm_vm *vm)
{
	mutex_lock(&vm->lock);
	vm->irqchip_created = true;
	mutex_unlock(&vm->lock);
	return 0;
}

static int axkvm_vm_create_pit2(struct axkvm_vm *vm, void __user *argp)
{
	struct kvm_pit_config cfg;

	if (copy_from_user(&cfg, argp, sizeof(cfg)))
		return -EFAULT;
	if (cfg.flags & ~KVM_PIT_SPEAKER_DUMMY)
		return -EINVAL;

	mutex_lock(&vm->lock);
	vm->pit_created = true;
	vm->pit_flags = cfg.flags;
	memset(&vm->pit_state, 0, sizeof(vm->pit_state));
	vm->pit_state.flags = cfg.flags;
	mutex_unlock(&vm->lock);

	return 0;
}

static int axkvm_vm_set_identity_map_addr(struct axkvm_vm *vm,
					  void __user *argp)
{
	u64 addr;

	if (copy_from_user(&addr, argp, sizeof(addr)))
		return -EFAULT;

	mutex_lock(&vm->lock);
	vm->identity_map_addr = addr;
	mutex_unlock(&vm->lock);
	return 0;
}

static int axkvm_vm_get_clock(struct axkvm_vm *vm, void __user *argp)
{
	struct kvm_clock_data clock;

	mutex_lock(&vm->lock);
	clock = vm->clock;
	mutex_unlock(&vm->lock);

	return copy_to_user(argp, &clock, sizeof(clock)) ? -EFAULT : 0;
}

static int axkvm_vm_set_clock(struct axkvm_vm *vm, void __user *argp)
{
	struct kvm_clock_data clock;

	if (copy_from_user(&clock, argp, sizeof(clock)))
		return -EFAULT;

	mutex_lock(&vm->lock);
	vm->clock = clock;
	mutex_unlock(&vm->lock);
	return 0;
}

static int axkvm_vm_get_irqchip(struct axkvm_vm *vm, void __user *argp)
{
	struct kvm_irqchip irqchip;

	if (copy_from_user(&irqchip, argp, sizeof(irqchip)))
		return -EFAULT;
	if (irqchip.chip_id >= KVM_NR_IRQCHIPS)
		return -EINVAL;

	mutex_lock(&vm->lock);
	irqchip = vm->irqchips[irqchip.chip_id];
	mutex_unlock(&vm->lock);

	return copy_to_user(argp, &irqchip, sizeof(irqchip)) ? -EFAULT : 0;
}

static int axkvm_vm_set_irqchip(struct axkvm_vm *vm, void __user *argp)
{
	struct kvm_irqchip irqchip;

	if (copy_from_user(&irqchip, argp, sizeof(irqchip)))
		return -EFAULT;
	if (irqchip.chip_id >= KVM_NR_IRQCHIPS)
		return -EINVAL;

	mutex_lock(&vm->lock);
	vm->irqchips[irqchip.chip_id] = irqchip;
	mutex_unlock(&vm->lock);
	return 0;
}

static int axkvm_vm_get_pit2(struct axkvm_vm *vm, void __user *argp)
{
	struct kvm_pit_state2 pit;

	mutex_lock(&vm->lock);
	pit = vm->pit_state;
	mutex_unlock(&vm->lock);

	return copy_to_user(argp, &pit, sizeof(pit)) ? -EFAULT : 0;
}

static int axkvm_vm_set_pit2(struct axkvm_vm *vm, void __user *argp)
{
	struct kvm_pit_state2 pit;

	if (copy_from_user(&pit, argp, sizeof(pit)))
		return -EFAULT;

	mutex_lock(&vm->lock);
	vm->pit_state = pit;
	mutex_unlock(&vm->lock);
	return 0;
}

static int axkvm_vm_set_tss_addr(struct axkvm_vm *vm, unsigned long addr)
{
	mutex_lock(&vm->lock);
	vm->tss_addr = addr;
	mutex_unlock(&vm->lock);
	return 0;
}

static int axkvm_vm_ioeventfd(struct axkvm_vm *vm, void __user *argp)
{
	struct kvm_ioeventfd ioevent;
	struct eventfd_ctx *ctx = NULL;
	unsigned int i;
	int free_slot = -1;
	int ret = -ENOENT;
	bool matched = false;

	if (copy_from_user(&ioevent, argp, sizeof(ioevent)))
		return -EFAULT;
	if (ioevent.flags & ~KVM_IOEVENTFD_VALID_FLAG_MASK)
		return -EINVAL;
	if (ioevent.len != 0 && ioevent.len != 1 && ioevent.len != 2 &&
	    ioevent.len != 4 && ioevent.len != 8)
		return -EINVAL;

	if (!(ioevent.flags & KVM_IOEVENTFD_FLAG_DEASSIGN)) {
		ctx = eventfd_ctx_fdget(ioevent.fd);
		if (IS_ERR(ctx))
			return PTR_ERR(ctx);
	}

	mutex_lock(&vm->lock);
	for (i = 0; i < AXKVM_MAX_IOEVENTS; i++) {
		struct axkvm_eventfd_binding *binding = &vm->ioevents[i];

		if (!binding->valid) {
			if (free_slot < 0)
				free_slot = i;
			continue;
		}

		if (binding->addr == ioevent.addr &&
		    binding->len == ioevent.len &&
		    binding->datamatch == ioevent.datamatch &&
		    (binding->flags & ~KVM_IOEVENTFD_FLAG_DEASSIGN) ==
			    (ioevent.flags & ~KVM_IOEVENTFD_FLAG_DEASSIGN)) {
			if (ioevent.flags & KVM_IOEVENTFD_FLAG_DEASSIGN) {
				axkvm_eventfd_binding_release(binding);
				ret = 0;
				} else {
					ret = -EEXIST;
				}
				matched = true;
				break;
			}
		}

		if (!matched && !(ioevent.flags & KVM_IOEVENTFD_FLAG_DEASSIGN)) {
			if (free_slot < 0) {
				ret = -ENOSPC;
			} else {
			struct axkvm_eventfd_binding *binding =
				&vm->ioevents[free_slot];

			binding->valid = true;
			binding->ctx = ctx;
			binding->addr = ioevent.addr;
			binding->datamatch = ioevent.datamatch;
			binding->len = ioevent.len;
			binding->flags = ioevent.flags;
			binding->fd = ioevent.fd;
			atomic64_set(&binding->signal_count, 0);
			atomic64_set(&binding->wake_count, 0);
			atomic64_set(&binding->inject_count, 0);
			pr_info("ioeventfd assign fd=%d addr=%#llx len=%u flags=%#x datamatch=%#llx\n",
				binding->fd, binding->addr, binding->len,
				binding->flags, binding->datamatch);
			ctx = NULL;
			ret = 0;
		}
	}
	mutex_unlock(&vm->lock);

	if (ctx)
		eventfd_ctx_put(ctx);
	return ret;
}

static int axkvm_vm_irqfd(struct axkvm_vm *vm, void __user *argp)
{
	struct kvm_irqfd irqfd;
	struct eventfd_ctx *ctx = NULL;
	struct file *file = NULL;
	__poll_t events = 0;
	unsigned int i;
	int free_slot = -1;
	int ret = -ENOENT;
	bool matched = false;

	if (copy_from_user(&irqfd, argp, sizeof(irqfd)))
		return -EFAULT;
	if (irqfd.flags & ~(KVM_IRQFD_FLAG_DEASSIGN | KVM_IRQFD_FLAG_RESAMPLE))
		return -EINVAL;
	if (irqfd.flags & KVM_IRQFD_FLAG_RESAMPLE)
		return -EOPNOTSUPP;

	file = eventfd_fget(irqfd.fd);
	if (IS_ERR(file))
		return PTR_ERR(file);

	ctx = eventfd_ctx_fileget(file);
	if (IS_ERR(ctx)) {
		ret = PTR_ERR(ctx);
		ctx = NULL;
		goto out_file;
	}

	mutex_lock(&vm->lock);
	for (i = 0; i < AXKVM_MAX_IRQFDS; i++) {
		struct axkvm_eventfd_binding *binding = &vm->irqfds[i];

		if (!binding->valid) {
			if (free_slot < 0)
				free_slot = i;
			continue;
		}

		if (binding->ctx == ctx && binding->gsi == irqfd.gsi) {
			if (irqfd.flags & KVM_IRQFD_FLAG_DEASSIGN) {
				axkvm_eventfd_binding_release(binding);
				ret = 0;
				} else {
					ret = -EBUSY;
				}
				matched = true;
				break;
			}
		}

		if (!matched && !(irqfd.flags & KVM_IRQFD_FLAG_DEASSIGN)) {
			if (free_slot < 0) {
				ret = -ENOSPC;
			} else {
			struct axkvm_eventfd_binding *binding =
				&vm->irqfds[free_slot];

			binding->ctx = ctx;
			binding->vm = vm;
			binding->irqfd = true;
			binding->fd = irqfd.fd;
			binding->gsi = irqfd.gsi;
			binding->flags = irqfd.flags;
			binding->resamplefd = irqfd.resamplefd;
			atomic64_set(&binding->signal_count, 0);
			atomic64_set(&binding->wake_count, 0);
			atomic64_set(&binding->inject_count, 0);
			INIT_WORK(&binding->irqfd_inject_work,
				  axkvm_irqfd_inject_work);
			init_waitqueue_func_entry(&binding->irqfd_wait,
						  axkvm_irqfd_wakeup);
			init_poll_funcptr(&binding->irqfd_pt,
					  axkvm_irqfd_poll_func);
			binding->valid = true;
			events = vfs_poll(file, &binding->irqfd_pt);
			pr_info("irqfd assign fd=%d gsi=%u flags=%#x initial_events=%#x\n",
				binding->fd, binding->gsi, binding->flags,
				(unsigned int)events);
			if (events & EPOLLIN)
				schedule_work(&binding->irqfd_inject_work);
			ctx = NULL;
			ret = 0;
		}
	}
	mutex_unlock(&vm->lock);

	if (ctx)
		eventfd_ctx_put(ctx);
out_file:
	fput(file);
	return ret;
}

static int axkvm_vm_set_gsi_routing(struct axkvm_vm *vm, void __user *argp)
{
	struct kvm_irq_routing hdr;
	struct kvm_irq_routing *routing;
	struct axkvm_irq_route *new_routes;
	size_t size;
	u32 i;
	u32 logged = 0;
	int ret = 0;

	if (copy_from_user(&hdr, argp, sizeof(hdr)))
		return -EFAULT;
	if (hdr.flags)
		return -EINVAL;
	if (hdr.nr > AXKVM_MAX_IRQ_ROUTES)
		return -EINVAL;

	new_routes = kvcalloc(AXKVM_MAX_IRQ_ROUTES, sizeof(*new_routes),
			      GFP_KERNEL);
	if (!new_routes)
		return -ENOMEM;

	size = sizeof(*routing) +
	       (size_t)hdr.nr * sizeof(struct kvm_irq_routing_entry);
	routing = memdup_user(argp, size);
	if (IS_ERR(routing)) {
		ret = PTR_ERR(routing);
		goto out_routes;
	}

	if (routing->nr != hdr.nr || routing->flags != hdr.flags) {
		ret = -EINVAL;
		goto out;
	}

	for (i = 0; i < routing->nr; i++) {
		const struct kvm_irq_routing_entry *entry = &routing->entries[i];
		struct axkvm_irq_route route;

		if (entry->gsi >= AXKVM_MAX_IRQ_ROUTES) {
			ret = -EINVAL;
			goto out;
		}
		if (entry->flags || entry->type != KVM_IRQ_ROUTING_IRQCHIP) {
			ret = -EOPNOTSUPP;
			goto out;
		}

		route.valid = true;
		route.type = entry->type;
		route.irqchip = entry->u.irqchip.irqchip;
		route.pin = entry->u.irqchip.pin;

		switch (route.irqchip) {
		case KVM_IRQCHIP_IOAPIC:
			if (route.pin >= KVM_IOAPIC_NUM_PINS) {
				ret = -EINVAL;
				goto out;
			}
			new_routes[entry->gsi] = route;
			break;
		case KVM_IRQCHIP_PIC_MASTER:
		case KVM_IRQCHIP_PIC_SLAVE:
			if (route.pin >= 8) {
				ret = -EINVAL;
				goto out;
			}
			if (!new_routes[entry->gsi].valid)
				new_routes[entry->gsi] = route;
			break;
		default:
			ret = -EINVAL;
			goto out;
		}

		if (logged < 16) {
			pr_info("gsi_route entry gsi=%u type=%u irqchip=%u pin=%u\n",
				entry->gsi, entry->type, route.irqchip,
				route.pin);
			logged++;
		}
	}

	mutex_lock(&vm->lock);
	memcpy(vm->irq_routes, new_routes,
	       AXKVM_MAX_IRQ_ROUTES * sizeof(*new_routes));
	mutex_unlock(&vm->lock);

	pr_info("set_gsi_routing nr=%u applied=%u\n", routing->nr, routing->nr);

out:
	kfree(routing);
out_routes:
	kvfree(new_routes);
	return ret;
}

static int axkvm_vcpu_copy_cpuid_from_user(struct axkvm_vcpu *vcpu,
					   void __user *argp)
{
	struct kvm_cpuid2 hdr;

	if (copy_from_user(&hdr, argp, sizeof(hdr)))
		return -EFAULT;
	if (hdr.nent > AXKVM_MAX_CPUID_ENTRIES)
		return -E2BIG;
	if (copy_from_user(vcpu->cpuid_entries,
			   (char __user *)argp + sizeof(hdr),
			   array_size(sizeof(vcpu->cpuid_entries[0]), hdr.nent)))
		return -EFAULT;

	vcpu->cpuid_nent = hdr.nent;

	/*
	 * Firecracker's CPUID is copied verbatim above (no sanitize). Clear
	 * TSC_DEADLINE (CPUID.01H:ECX bit 24) so the guest does not select the
	 * LAPIC TSC-deadline timer, which arms via WRMSR IA32_TSC_DEADLINE
	 * (0x6E0) - a path we do not virtualize yet (the write is swallowed by
	 * the generic MSR handler, start_timer never runs, KVM_TIMERS stays
	 * empty, LAPIC tick never fires). Clearing the bit forces the guest
	 * back to the APIC_TMICT one-shot path we do implement.
	 */
	{
		u32 i;

		for (i = 0; i < vcpu->cpuid_nent; i++) {
			if (vcpu->cpuid_entries[i].function == 0x1) {
				vcpu->cpuid_entries[i].ecx &= ~BIT(24);
				break;
			}
		}
	}

	return 0;
}

static int axkvm_vcpu_copy_cpuid_to_user(struct axkvm_vcpu *vcpu,
					 void __user *argp)
{
	return axkvm_copy_cpuid_to_user(argp, vcpu->cpuid_entries,
				       vcpu->cpuid_nent);
}

static int axkvm_vcpu_find_msr(struct axkvm_vcpu *vcpu, u32 index)
{
	u32 i;

	for (i = 0; i < vcpu->nmsrs; i++) {
		if (vcpu->msrs[i].index == index)
			return i;
	}
	return -ENOENT;
}

static int axkvm_vcpu_get_msrs(struct axkvm_vcpu *vcpu, void __user *argp)
{
	struct kvm_msrs hdr;
	u32 i;

	if (copy_from_user(&hdr, argp, sizeof(hdr)))
		return -EFAULT;
	if (hdr.nmsrs > AXKVM_MAX_MSR_ENTRIES)
		return -E2BIG;

	for (i = 0; i < hdr.nmsrs; i++) {
		struct kvm_msr_entry entry;
		int idx;

		if (copy_from_user(&entry,
				   (char __user *)argp + sizeof(hdr) +
					   i * sizeof(entry),
				   sizeof(entry)))
			return -EFAULT;

		idx = axkvm_vcpu_find_msr(vcpu, entry.index);
		entry.data = idx >= 0 ? vcpu->msrs[idx].data : 0;

		if (copy_to_user((char __user *)argp + sizeof(hdr) +
					 i * sizeof(entry),
				 &entry, sizeof(entry)))
			return -EFAULT;
	}

	return hdr.nmsrs;
}

static int axkvm_vcpu_set_msrs(struct axkvm_vcpu *vcpu, void __user *argp)
{
	struct kvm_msrs hdr;
	u32 i;

	if (copy_from_user(&hdr, argp, sizeof(hdr)))
		return -EFAULT;
	if (hdr.nmsrs > AXKVM_MAX_MSR_ENTRIES)
		return -E2BIG;

	for (i = 0; i < hdr.nmsrs; i++) {
		struct kvm_msr_entry entry;
		int idx;

		if (copy_from_user(&entry,
				   (char __user *)argp + sizeof(hdr) +
					   i * sizeof(entry),
				   sizeof(entry)))
			return -EFAULT;

		idx = axkvm_vcpu_find_msr(vcpu, entry.index);
		if (idx < 0) {
			if (vcpu->nmsrs >= AXKVM_MAX_MSR_ENTRIES)
				return i;
			idx = vcpu->nmsrs++;
		}
		vcpu->msrs[idx] = entry;
	}

	return hdr.nmsrs;
}

static long axkvm_vcpu_ioctl_x86(struct axkvm_vcpu *vcpu, unsigned int ioctl,
				 unsigned long arg)
{
	void __user *argp = (void __user *)arg;
	long ret = 0;
	bool state_changed = false;

	mutex_lock(&vcpu->lock);
	switch (ioctl) {
	case KVM_GET_REGS:
		ret = copy_to_user(argp, &vcpu->regs, sizeof(vcpu->regs)) ?
			      -EFAULT : 0;
		break;
	case KVM_SET_REGS:
		ret = copy_from_user(&vcpu->regs, argp, sizeof(vcpu->regs)) ?
			      -EFAULT : 0;
		state_changed = !ret;
		break;
	case KVM_GET_SREGS:
		ret = copy_to_user(argp, &vcpu->sregs, sizeof(vcpu->sregs)) ?
			      -EFAULT : 0;
		break;
	case KVM_SET_SREGS:
		ret = copy_from_user(&vcpu->sregs, argp, sizeof(vcpu->sregs)) ?
			      -EFAULT : 0;
		if (!ret)
			axkvm_normalize_x86_sregs(&vcpu->sregs);
		if (!ret)
			pr_info("KVM_SET_SREGS vcpu=%u cr0=%llx cr3=%llx cr4=%llx efer=%llx apic_base=%llx cs=%x:%llx unusable=%u\n",
				vcpu->id, vcpu->sregs.cr0, vcpu->sregs.cr3,
				vcpu->sregs.cr4, vcpu->sregs.efer,
				vcpu->sregs.apic_base, vcpu->sregs.cs.selector,
				vcpu->sregs.cs.base, vcpu->sregs.cs.unusable);
		state_changed = !ret;
		break;
	case KVM_GET_FPU:
		ret = copy_to_user(argp, &vcpu->fpu, sizeof(vcpu->fpu)) ?
			      -EFAULT : 0;
		break;
	case KVM_SET_FPU:
		ret = copy_from_user(&vcpu->fpu, argp, sizeof(vcpu->fpu)) ?
			      -EFAULT : 0;
		if (!ret)
			vcpu->fpu_valid = true;
		state_changed = !ret;
		break;
	case KVM_GET_LAPIC:
		ret = copy_to_user(argp, &vcpu->lapic, sizeof(vcpu->lapic)) ?
			      -EFAULT : 0;
		break;
	case KVM_SET_LAPIC:
		ret = copy_from_user(&vcpu->lapic, argp, sizeof(vcpu->lapic)) ?
			      -EFAULT : 0;
		if (!ret)
			pr_info("KVM_SET_LAPIC vcpu=%u lapic_id=%x\n",
				vcpu->id, axkvm_lapic_id(&vcpu->lapic));
		state_changed = !ret;
		break;
	case KVM_SET_CPUID2:
		ret = axkvm_vcpu_copy_cpuid_from_user(vcpu, argp);
		state_changed = !ret;
		break;
	case KVM_GET_CPUID2:
		ret = axkvm_vcpu_copy_cpuid_to_user(vcpu, argp);
		break;
	case KVM_GET_MSRS:
		ret = axkvm_vcpu_get_msrs(vcpu, argp);
		break;
	case KVM_SET_MSRS:
		ret = axkvm_vcpu_set_msrs(vcpu, argp);
		if (ret >= 0)
			pr_info("KVM_SET_MSRS vcpu=%u accepted=%ld nmsrs=%u\n",
				vcpu->id, ret, vcpu->nmsrs);
		state_changed = ret >= 0;
		break;
	case KVM_GET_MP_STATE:
		ret = copy_to_user(argp, &vcpu->mp_state, sizeof(vcpu->mp_state)) ?
			      -EFAULT : 0;
		break;
	case KVM_SET_MP_STATE:
		ret = copy_from_user(&vcpu->mp_state, argp,
				     sizeof(vcpu->mp_state)) ? -EFAULT : 0;
		if (!ret) {
			pr_info("KVM_SET_MP_STATE vcpu=%u mp_state=%u\n",
				vcpu->id, vcpu->mp_state.mp_state);
			if (vcpu->mp_state.mp_state == KVM_MP_STATE_RUNNABLE)
				wake_up_all(&vcpu->mp_state_wq);
		}
		state_changed = !ret;
		break;
	case KVM_GET_DEBUGREGS:
		ret = copy_to_user(argp, &vcpu->debugregs,
				   sizeof(vcpu->debugregs)) ? -EFAULT : 0;
		break;
	case KVM_SET_DEBUGREGS:
		ret = copy_from_user(&vcpu->debugregs, argp,
				     sizeof(vcpu->debugregs)) ? -EFAULT : 0;
		state_changed = !ret;
		break;
	case KVM_GET_XSAVE:
		ret = copy_to_user(argp, &vcpu->xsave, sizeof(vcpu->xsave)) ?
			      -EFAULT : 0;
		break;
	case KVM_SET_XSAVE:
		ret = copy_from_user(&vcpu->xsave, argp, sizeof(vcpu->xsave)) ?
			      -EFAULT : 0;
		if (!ret)
			vcpu->xsave_valid = true;
		state_changed = !ret;
		break;
	case KVM_GET_XCRS:
		ret = copy_to_user(argp, &vcpu->xcrs, sizeof(vcpu->xcrs)) ?
			      -EFAULT : 0;
		break;
	case KVM_SET_XCRS: {
		struct kvm_xcrs xcrs;

		ret = copy_from_user(&xcrs, argp, sizeof(xcrs)) ? -EFAULT : 0;
		if (!ret && xcrs.nr_xcrs > ARRAY_SIZE(xcrs.xcrs)) {
			ret = -EINVAL;
		} else if (!ret) {
			u32 i;

			for (i = 0; i < xcrs.nr_xcrs; i++) {
				if (xcrs.xcrs[i].xcr == 0 &&
				    !axkvm_x86_xcr0_valid(xcrs.xcrs[i].value)) {
					ret = -EINVAL;
					break;
				}
			}
		}
		if (!ret)
			vcpu->xcrs = xcrs;
		state_changed = !ret;
		break;
	}
	case KVM_GET_VCPU_EVENTS:
		ret = copy_to_user(argp, &vcpu->events, sizeof(vcpu->events)) ?
			      -EFAULT : 0;
		break;
	case KVM_SET_VCPU_EVENTS:
		ret = copy_from_user(&vcpu->events, argp, sizeof(vcpu->events)) ?
			      -EFAULT : 0;
		state_changed = !ret;
		break;
	case KVM_GET_TSC_KHZ:
		ret = vcpu->tsc_khz;
		break;
	case KVM_SET_TSC_KHZ:
		vcpu->tsc_khz = arg;
		ret = 0;
		state_changed = true;
		break;
	default:
		ret = -ENOTTY;
		break;
	}
	if (state_changed)
		vcpu->backend_state_dirty = true;
	mutex_unlock(&vcpu->lock);

	return ret;
}
#else
static long axkvm_vcpu_ioctl_x86(struct axkvm_vcpu *vcpu, unsigned int ioctl,
				 unsigned long arg)
{
	return -ENOTTY;
}
#endif

static int axkvm_vm_set_user_memory_region(struct axkvm_vm *vm,
					   void __user *argp)
{
	struct kvm_userspace_memory_region mem;
	struct axkvm_memslot new_slot = {};
	struct axkvm_memslot *slot;
	unsigned long nr_pages;
	struct page **pages = NULL;
	unsigned long *mapped = NULL;
	unsigned long *writable = NULL;

	if (copy_from_user(&mem, argp, sizeof(mem)))
		return -EFAULT;

	if (mem.slot >= AXKVM_MAX_MEMSLOTS)
		return -EINVAL;
	if (!PAGE_ALIGNED(mem.guest_phys_addr) ||
	    !PAGE_ALIGNED(mem.userspace_addr) ||
	    !PAGE_ALIGNED(mem.memory_size))
		return -EINVAL;
	if (mem.flags & ~(u32)KVM_MEM_LOG_DIRTY_PAGES)
		return -EINVAL;
	if (mem.flags & KVM_MEM_LOG_DIRTY_PAGES)
		return -EOPNOTSUPP;

	nr_pages = mem.memory_size >> PAGE_SHIFT;
	if (mem.memory_size && !nr_pages)
		return -EINVAL;

	if (nr_pages) {
		if (nr_pages > INT_MAX)
			return -E2BIG;

		/*
		 * Lazy on-demand mapping: do NOT eager pin/map the slot here.
		 * gvisor registers huge sparse slots (e.g. 8 GiB) backed by an
		 * unpopulated anonymous mmap; eager FOLL_LONGTERM pinning of
		 * unpopulated pages returns EFAULT and defeats overcommit.
		 * Instead just validate that the HVA range is addressable and
		 * allocate a sparse pages[] array; individual pages are pinned
		 * and mapped into the EPT on demand via axkvm_fault_in_gpa when
		 * the guest first touches them (EPT violation).
		 */
		if (!access_ok((void __user *)(unsigned long)mem.userspace_addr,
			       mem.memory_size))
			return -EFAULT;

		pages = kvcalloc(nr_pages, sizeof(*pages), GFP_KERNEL);
		if (!pages)
			return -ENOMEM;

		mapped = bitmap_zalloc(nr_pages, GFP_KERNEL);
		writable = bitmap_zalloc(nr_pages, GFP_KERNEL);
		if (!mapped || !writable) {
			bitmap_free(mapped);
			bitmap_free(writable);
			kvfree(pages);
			return -ENOMEM;
		}

		new_slot.valid = true;
		new_slot.slot = mem.slot;
		new_slot.flags = mem.flags;
		new_slot.guest_phys_addr = mem.guest_phys_addr;
		new_slot.memory_size = mem.memory_size;
		new_slot.userspace_addr = mem.userspace_addr;
		new_slot.pages = pages;
		new_slot.mapped = mapped;
		new_slot.writable = writable;
		new_slot.nr_pages = nr_pages;
	}

	mutex_lock(&vm->lock);
	slot = &vm->memslots[mem.slot];
	if (!mem.memory_size) {
		axkvm_backend_unmap_memslot(vm, slot);
		axkvm_memslot_release(slot);
		mutex_unlock(&vm->lock);
		return 0;
	}

	/*
	 * Replace the previous slot (if any). Its lazily-faulted pages are
	 * unpinned and its EPT range torn down; the new slot starts empty and
	 * is populated on demand.
	 */
	axkvm_backend_unmap_memslot(vm, slot);
	axkvm_memslot_release(slot);
	*slot = new_slot;
	mutex_unlock(&vm->lock);

	if (axkvm_debug_verbose)
		pr_info("registered lazy memslot slot=%u gpa=%#llx hva=%#llx size=%#llx pages=%lu\n",
			 mem.slot, mem.guest_phys_addr, mem.userspace_addr,
			 mem.memory_size, nr_pages);
	return 0;
}

static int axkvm_vcpu_mmap(struct file *file, struct vm_area_struct *vma)
{
	struct axkvm_vcpu *vcpu = file->private_data;
	unsigned long size = vma->vm_end - vma->vm_start;
	unsigned long pfn;

	if (size > AXKVM_VCPU_MMAP_SIZE)
		return -EINVAL;
	if (vma->vm_pgoff)
		return -EINVAL;

	pfn = virt_to_phys((void *)vcpu->run_pages) >> PAGE_SHIFT;
	return remap_pfn_range(vma, vma->vm_start, pfn, size,
			       vma->vm_page_prot);
}

static long axkvm_vcpu_ioctl(struct file *file, unsigned int ioctl,
			     unsigned long arg)
{
	struct axkvm_vcpu *vcpu = file->private_data;

	if (_IOC_TYPE(ioctl) != KVMIO)
		return -ENOTTY;

	switch (ioctl) {
	case KVM_RUN:
		if (arg)
			return -EINVAL;
		return axkvm_vcpu_run_backend(vcpu);
	case KVM_SET_SIGNAL_MASK:
		return axkvm_vcpu_set_signal_mask(vcpu, (void __user *)arg);
#ifdef KVM_KVMCLOCK_CTRL
	case KVM_KVMCLOCK_CTRL:
		if (arg)
			return -EINVAL;
		return 0;
#endif
	default:
		return axkvm_vcpu_ioctl_x86(vcpu, ioctl, arg);
	}
}

static int axkvm_vcpu_release(struct inode *inode, struct file *file)
{
	struct axkvm_vcpu *vcpu = file->private_data;
	struct axkvm_vm *vm = vcpu->vm;

	mutex_lock(&vm->lock);
	if (vcpu->id < AXKVM_MAX_VCPUS && vm->vcpus[vcpu->id] == vcpu)
		vm->vcpus[vcpu->id] = NULL;
	/*
	 * Prod any parked spinner so it re-checks its gate (immediate_exit/signal
	 * already return -EINTR from axkvm_spin_park). The hardirq backstop + the
	 * short timeout guarantee liveness even without this, but waking here
	 * bounds teardown latency. vm->lock held.
	 */
	axkvm_wake_parked_spinners(vm);
	mutex_unlock(&vm->lock);

	axkvm_vcpu_put(vcpu);
	return 0;
}

static const struct file_operations axkvm_vcpu_fops = {
	.owner = THIS_MODULE,
	.release = axkvm_vcpu_release,
	.unlocked_ioctl = axkvm_vcpu_ioctl,
	.mmap = axkvm_vcpu_mmap,
	.llseek = noop_llseek,
};

static int axkvm_vm_create_vcpu(struct axkvm_vm *vm, unsigned long id)
{
	struct axkvm_vcpu *vcpu;
	char name[32];
	int fd;
	int ret;

	if (id >= AXKVM_MAX_VCPUS)
		return -EINVAL;

	vcpu = kzalloc(sizeof(*vcpu), GFP_KERNEL);
	if (!vcpu)
		return -ENOMEM;

	vcpu->run_pages = __get_free_pages(GFP_KERNEL | __GFP_ZERO,
					    get_order(AXKVM_VCPU_MMAP_SIZE));
	if (!vcpu->run_pages) {
		kfree(vcpu);
		return -ENOMEM;
	}

	kref_init(&vcpu->refcount);
	vcpu->vm = axkvm_vm_get(vm);
	vcpu->id = id;
	vcpu->run = (struct kvm_run *)vcpu->run_pages;
	if (vm->backend_ready) {
		ret = axvisor_kvm_backend_create_vcpu(vm->backend_vm, id,
						      &vcpu->backend_vcpu);
		if (ret && ret != -EOPNOTSUPP) {
			axkvm_vcpu_put(vcpu);
			return ret;
		}
		vcpu->backend_ready = ret == 0;
	}
#ifdef CONFIG_X86_64
	mutex_init(&vcpu->lock);
	init_waitqueue_head(&vcpu->mp_state_wq);
	init_waitqueue_head(&vcpu->halt_wq);
	init_waitqueue_head(&vcpu->admit_wq);
	INIT_DELAYED_WORK(&vcpu->boost_watchdog, axkvm_bringup_boost_watchdog);
	vcpu->boot_state = AP_BOOT_NONE;
	atomic_set(&vcpu->irq_pending_wakeup, 0);
	atomic_set(&vcpu->in_halt_wait, 0);
	vcpu->last_halt_backstop_jiffies = 0;
	atomic_set(&vcpu->boost_target, 0);
	axkvm_init_x86_vcpu_state(vcpu);
	pr_info("KVM_CREATE_VCPU id=%lu default_mp_state=%u default_apic_base=%llx\n",
		id, vcpu->mp_state.mp_state, vcpu->sregs.apic_base);
#endif

	mutex_lock(&vm->lock);
	if (vm->vcpus[id]) {
		mutex_unlock(&vm->lock);
		axkvm_vcpu_put(vcpu);
		return -EEXIST;
	}
	vm->vcpus[id] = vcpu;
	mutex_unlock(&vm->lock);

	snprintf(name, sizeof(name), "axvisor-kvm-vcpu:%lu", id);
	fd = anon_inode_getfd(name, &axkvm_vcpu_fops, vcpu,
			      O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		mutex_lock(&vm->lock);
		if (vm->vcpus[id] == vcpu)
			vm->vcpus[id] = NULL;
		mutex_unlock(&vm->lock);
		axkvm_vcpu_put(vcpu);
		return fd;
	}

	return fd;
}

static long axkvm_vm_ioctl(struct file *file, unsigned int ioctl,
			   unsigned long arg)
{
	struct axkvm_vm *vm = file->private_data;

	if (_IOC_TYPE(ioctl) != KVMIO)
		return -ENOTTY;

	/* DIAG-N1B: unconditional vm-level ioctl trace (remove after n=1 root-cause) */
	pr_info("diag_n1b vm_ioctl nr=%u ioctl=%#x\n", _IOC_NR(ioctl), ioctl);

	switch (ioctl) {
	case KVM_CHECK_EXTENSION:
		return axkvm_check_extension(arg);
	case KVM_CREATE_VCPU:
		return axkvm_vm_create_vcpu(vm, arg);
	case KVM_SET_USER_MEMORY_REGION:
		return axkvm_vm_set_user_memory_region(vm, (void __user *)arg);
#ifdef CONFIG_X86_64
	case KVM_SET_TSS_ADDR:
		return axkvm_vm_set_tss_addr(vm, arg);
	case KVM_SET_IDENTITY_MAP_ADDR:
		return axkvm_vm_set_identity_map_addr(vm, (void __user *)arg);
	case KVM_CREATE_IRQCHIP:
		if (arg)
			return -EINVAL;
		return axkvm_vm_create_irqchip(vm);
	case KVM_CREATE_PIT2:
		return axkvm_vm_create_pit2(vm, (void __user *)arg);
	case KVM_GET_CLOCK:
		return axkvm_vm_get_clock(vm, (void __user *)arg);
	case KVM_SET_CLOCK:
		return axkvm_vm_set_clock(vm, (void __user *)arg);
	case KVM_GET_IRQCHIP:
		return axkvm_vm_get_irqchip(vm, (void __user *)arg);
	case KVM_SET_IRQCHIP:
		return axkvm_vm_set_irqchip(vm, (void __user *)arg);
	case KVM_GET_PIT2:
		return axkvm_vm_get_pit2(vm, (void __user *)arg);
	case KVM_SET_PIT2:
		return axkvm_vm_set_pit2(vm, (void __user *)arg);
	case KVM_IOEVENTFD:
		return axkvm_vm_ioeventfd(vm, (void __user *)arg);
	case KVM_IRQFD:
		return axkvm_vm_irqfd(vm, (void __user *)arg);
	case KVM_SET_GSI_ROUTING:
		return axkvm_vm_set_gsi_routing(vm, (void __user *)arg);
#endif
	default:
		return -ENOTTY;
	}
}

static int axkvm_vm_release(struct inode *inode, struct file *file)
{
	struct axkvm_vm *vm = file->private_data;

#ifdef CONFIG_X86_64
	axkvm_unregister_backend_vm(vm);
#endif
	axkvm_vm_put(vm);
	return 0;
}

static const struct file_operations axkvm_vm_fops = {
	.owner = THIS_MODULE,
	.release = axkvm_vm_release,
	.unlocked_ioctl = axkvm_vm_ioctl,
	.llseek = noop_llseek,
};

static int axkvm_dev_create_vm(unsigned long type)
{
	struct axkvm_vm *vm;
	struct file *file;
	int fd;
	int ret;

	if (type)
		return -EINVAL;

	fd = get_unused_fd_flags(O_CLOEXEC);
	if (fd < 0)
		return fd;

	vm = kzalloc(sizeof(*vm), GFP_KERNEL);
	if (!vm) {
		put_unused_fd(fd);
		return -ENOMEM;
	}

	kref_init(&vm->refcount);
	mutex_init(&vm->lock);
#ifdef CONFIG_X86_64
	vm->boot_controller_id = -1;
	vm->current_bringup_target = -1;
	init_waitqueue_head(&vm->spin_park_wq);
	vm->spin_park_gen = 0;
	axkvm_vm_init_default_irq_routes(vm);
#endif
	ret = axvisor_kvm_backend_create_vm(&vm->backend_vm);
	if (ret && ret != -EOPNOTSUPP) {
		axkvm_vm_put(vm);
		put_unused_fd(fd);
		return ret;
	}
	vm->backend_ready = ret == 0;

#ifdef CONFIG_X86_64
	ret = axkvm_register_backend_vm(vm);
	if (ret) {
		axkvm_vm_put(vm);
		put_unused_fd(fd);
		return ret;
	}
	/* DIAG: publish lockless handle for the hardirq A-vs-B dump, and anchor
	 * the dump window to VM-publish time (NOT module load) so the ~20s dump
	 * lands deep in the guest hang rather than before the guest exists. */
	WRITE_ONCE(axkvm_dbg_first_tick_jiffies, jiffies);
	WRITE_ONCE(axkvm_dbg_vm, vm);
#endif

	file = anon_inode_getfile("axvisor-kvm-vm", &axkvm_vm_fops, vm,
				  O_RDWR);
	if (IS_ERR(file)) {
#ifdef CONFIG_X86_64
		axkvm_unregister_backend_vm(vm);
#endif
		axkvm_vm_put(vm);
		put_unused_fd(fd);
		return PTR_ERR(file);
	}

	fd_install(fd, file);
	return fd;
}

static long axkvm_dev_ioctl(struct file *file, unsigned int ioctl,
			    unsigned long arg)
{
	if (_IOC_TYPE(ioctl) != KVMIO)
		return -ENOTTY;

	/* DIAG-N1B: unconditional dev-level ioctl trace (remove after n=1 root-cause) */
	pr_info("diag_n1b dev_ioctl nr=%u ioctl=%#x\n", _IOC_NR(ioctl), ioctl);

	switch (ioctl) {
	case KVM_GET_API_VERSION:
		if (arg)
			return -EINVAL;
		return KVM_API_VERSION;
	case KVM_CHECK_EXTENSION:
		return axkvm_check_extension(arg);
	case KVM_GET_VCPU_MMAP_SIZE:
		if (arg)
			return -EINVAL;
		return AXKVM_VCPU_MMAP_SIZE;
	case KVM_CREATE_VM:
		return axkvm_dev_create_vm(arg);
#ifdef CONFIG_X86_64
	case KVM_GET_SUPPORTED_CPUID:
		return axkvm_get_supported_cpuid((void __user *)arg);
	case KVM_GET_MSR_INDEX_LIST:
		return axkvm_get_msr_index_list((void __user *)arg);
#endif
	default:
		return -ENOTTY;
	}
}

static const struct file_operations axkvm_dev_fops = {
	.owner = THIS_MODULE,
	.unlocked_ioctl = axkvm_dev_ioctl,
	.llseek = noop_llseek,
};

static struct miscdevice axkvm_miscdev = {
	.minor = KVM_MINOR,
	.name = "kvm",
	.fops = &axkvm_dev_fops,
};

#ifdef CONFIG_X86_64
/*
 * DIAG (any-CPU liveness witness): a floating SCHED_FIFO kthread that emits a
 * byte to QEMU debugcon (port 0xe9) roughly every 200ms. Being unpinned and
 * RT-priority, it runs on ANY L1 CPU that has spare cycles, and it bypasses
 * printk/console-lock/ttyS0. Combined with the hardirq home-CPU witness in the
 * periodic hrtimer callback, this distinguishes:
 *   - debugcon 'W' keeps flowing after ttyS0 dies  -> L1 has free cycles;
 *     the ttyS0/console path is stuck (not a hard CPU wedge).
 *   - debugcon 'W' also stops                       -> ALL L1 CPUs are
 *     monopolised (no core ever schedules this RT thread) = genuine wedge.
 * Remove after diagnosis.
 */
static struct task_struct *axkvm_dbg_witness_task;

static int axkvm_dbg_witness_fn(void *unused)
{
	while (!kthread_should_stop()) {
		outb('W', 0xe9);
		msleep(200);
	}
	return 0;
}

static void axkvm_dbg_witness_start(void)
{
	struct task_struct *t;

	t = kthread_run(axkvm_dbg_witness_fn, NULL, "axkvm_dbg_witness");
	if (IS_ERR(t)) {
		axkvm_dbg_witness_task = NULL;
		pr_warn("axkvm dbg witness kthread failed: %ld\n", PTR_ERR(t));
		return;
	}
	sched_set_fifo(t);
	axkvm_dbg_witness_task = t;
}

static void axkvm_dbg_witness_stop(void)
{
	if (axkvm_dbg_witness_task) {
		kthread_stop(axkvm_dbg_witness_task);
		axkvm_dbg_witness_task = NULL;
	}
}
#endif

static int __init axkvm_init(void)
{
	int ret;

#ifdef CONFIG_X86_64
	hrtimer_setup(&axkvm_backend_timer, axkvm_backend_timer_cb,
		      CLOCK_MONOTONIC, HRTIMER_MODE_ABS);
	hrtimer_setup(&axkvm_backend_periodic_timer, axkvm_backend_periodic_cb,
		      CLOCK_MONOTONIC, HRTIMER_MODE_REL_HARD);
	axkvm_backend_timer_wq = alloc_workqueue("axkvm_timer",
						 WQ_UNBOUND | WQ_HIGHPRI |
							 WQ_MEM_RECLAIM,
						 1);
	if (!axkvm_backend_timer_wq)
		pr_warn("axkvm_timer workqueue alloc failed; using system_wq (timer delivery may lag under oversubscription)\n");
	hrtimer_start(&axkvm_backend_periodic_timer,
		      ns_to_ktime(AXKVM_BACKEND_PERIODIC_NS),
		      HRTIMER_MODE_REL_HARD);
	axkvm_dbg_witness_start();
#endif

	ret = axvisor_kvm_builtin_backend_init();
	if (ret) {
		pr_err("failed to initialize built-in AxVisor backend: %d\n", ret);
		return ret;
	}

	axkvm_miscdev.name = axkvm_dev_name;
	axkvm_miscdev.minor = !strcmp(axkvm_dev_name, "kvm") ? KVM_MINOR :
			      MISC_DYNAMIC_MINOR;

	ret = misc_register(&axkvm_miscdev);
	if (ret) {
		pr_err("failed to register /dev/%s: %d%s\n",
		       axkvm_miscdev.name, ret,
		       axkvm_miscdev.minor == KVM_MINOR ?
			       "; unload native kvm if it owns KVM_MINOR" : "");
		axvisor_kvm_builtin_backend_exit();
		return ret;
	}

	pr_info("registered /dev/%s ABI provider\n", axkvm_miscdev.name);
	return 0;
}

static void __exit axkvm_exit(void)
{
	misc_deregister(&axkvm_miscdev);
#ifdef CONFIG_X86_64
	axkvm_dbg_witness_stop();
	axvisor_kvm_x86_bridge_cancel_timer();
	hrtimer_cancel(&axkvm_backend_periodic_timer);
	cancel_work_sync(&axkvm_backend_timer_work);
	if (axkvm_backend_timer_wq) {
		destroy_workqueue(axkvm_backend_timer_wq);
		axkvm_backend_timer_wq = NULL;
	}
#endif
	axvisor_kvm_builtin_backend_exit();
	pr_info("unregistered /dev/%s ABI provider\n", axkvm_miscdev.name);
}

module_init(axkvm_init);
module_exit(axkvm_exit);

MODULE_AUTHOR("AxVisor Team");
MODULE_DESCRIPTION("AxVisor KVM ABI compatibility provider");
MODULE_LICENSE("GPL");
