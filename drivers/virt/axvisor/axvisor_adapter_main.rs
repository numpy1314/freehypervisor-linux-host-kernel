// SPDX-License-Identifier: GPL-2.0

//! AxVisor Linux host adapter, module-first version.

#![allow(dead_code, improper_ctypes, missing_docs, unused_unsafe)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kernel::{
    alloc::KBox,
    alloc::kvec::KVVec,
    bindings,
    cpu::CpuId,
    cpumask::CpumaskVar,
    error::{code::*, from_err_ptr, Error, Result},
    ffi::{c_char, c_int, c_uint},
    init::InPlaceInit,
    prelude::*,
    sync::{Arc, CondVar, Mutex, new_condvar, new_mutex},
};
use kernel::sync::atomic::Release as KRelease;

mod arch;
mod core_link;
mod vendor;

module! {
    type: AxvisorAdapterModule,
    name: "axvisor_adapter",
    authors: ["OpenAI", "bullet1517"],
    description: "AxVisor Linux host adapter",
    license: "GPL",
}

unsafe extern "C" {
    fn axvisor_adapter_kthread_create(
        threadfn: unsafe extern "C" fn(*mut c_void) -> c_int,
        data: *mut c_void,
        name: *const c_char,
    ) -> *mut bindings::task_struct;

    fn axvisor_adapter_set_cpus_allowed_ptr(
        task: *mut bindings::task_struct,
        mask: *const bindings::cpumask,
    ) -> c_int;

    fn axvisor_adapter_kthread_bind(task: *mut bindings::task_struct, cpu: c_uint);

    fn axvisor_adapter_wake_up_process(task: *mut bindings::task_struct);

    fn axvisor_adapter_kthread_stop(task: *mut bindings::task_struct) -> c_int;

    fn axvisor_adapter_yield();

    fn axvisor_adapter_host_cpu_num() -> usize;
    fn axvisor_adapter_current_cpu_id() -> usize;

    fn axvisor_adapter_console_write(buf: *const u8, len: usize);
    fn axvisor_adapter_guest_console_write(buf: *const u8, len: usize);
    fn axvisor_adapter_guest_console_read(buf: *mut u8, len: usize) -> usize;
    fn axvisor_adapter_console_input_install() -> bool;
    fn axvisor_adapter_console_input_remove();
    fn axvisor_adapter_host_fdt_prepare() -> c_int;
    fn axvisor_adapter_host_fdt_release();
    fn axvisor_adapter_release_dynamic_mappings();
    fn axvisor_adapter_release_passthrough_irqs();
    fn axvisor_adapter_release_host_filesystems() -> c_int;
    fn axvisor_adapter_request_x86_qemu_blk_intx() -> c_int;
    fn axvisor_adapter_x86_passthrough_irq_unmask(irq_id: c_uint) -> bool;
    fn axvisor_adapter_x86_passthrough_irq_handle_vector(vector: c_uint) -> bool;
    fn axvisor_adapter_x86_passthrough_irq_poll(irq_id: c_uint) -> bool;

    fn axvisor_adapter_current_time_nanos() -> u64;

    fn axvisor_adapter_host_exit(exit_code: c_int) -> !;

    fn axvisor_adapter_alloc_frame() -> u64;

    fn axvisor_adapter_dealloc_frame(paddr: u64) -> bool;

    fn axvisor_adapter_phys_to_virt(paddr: u64) -> u64;

    fn axvisor_adapter_virt_to_phys(vaddr: u64) -> u64;

    fn axvisor_adapter_register_guest_ram(paddr: u64, size: u64) -> bool;

    fn axvisor_adapter_mmio_read32(paddr: u64) -> u32;

    fn axvisor_adapter_mmio_write32(paddr: u64, value: u32);

    fn axvisor_adapter_riscv_plic_complete_passthrough_irq(irq_id: c_uint);

}

const THREAD_NAME: &kernel::str::CStr = c"axvisor-host-task";
const MAX_IRQ_VECTORS: usize = 256;
const MAX_PASSTHROUGH_DEVICES: usize = 16;
#[cfg(target_arch = "x86_64")]
const X86_QEMU_BLK_VM_ID: usize = 1;
#[cfg(target_arch = "x86_64")]
const X86_QEMU_BLK_GUEST_GSI: usize = 19;
#[cfg(target_arch = "x86_64")]
const X86_QEMU_BLK_SYNTHETIC_BASE_HPA: u64 = 0x5850_4943_0000_0300;
const HOST_EMERG_LINE_BUFFER_CAPACITY: usize = 4096;
const HOST_EMERG_FORCE_FLUSH_THRESHOLD: usize = 512;
const GUEST_CONSOLE_INPUT_BUFFER_CAPACITY: usize = 4096;
const GUEST_CONSOLE_OUTPUT_BUFFER_CAPACITY: usize = 65536;
static IRQ_HANDLERS: [AtomicUsize; MAX_IRQ_VECTORS] = [const { AtomicUsize::new(0) }; MAX_IRQ_VECTORS];
static IRQ_BRIDGE_HANDLER_CTX: [AtomicUsize; MAX_IRQ_VECTORS] =
    [const { AtomicUsize::new(0) }; MAX_IRQ_VECTORS];
static IRQ_BRIDGE_HANDLER_FN: [AtomicUsize; MAX_IRQ_VECTORS] =
    [const { AtomicUsize::new(0) }; MAX_IRQ_VECTORS];
static IRQ_BRIDGE_EXTERNAL_HANDLER_CTX: AtomicUsize = AtomicUsize::new(0);
static IRQ_BRIDGE_EXTERNAL_HANDLER_FN: AtomicUsize = AtomicUsize::new(0);
static HOST_RELEASE_FILESYSTEMS_CALLS: AtomicUsize = AtomicUsize::new(0);
static KERNEL_TASK_RUNTIME_INSTALLED: AtomicUsize = AtomicUsize::new(0);
static WAIT_QUEUE_IDS: AtomicUsize = AtomicUsize::new(1);
static TIMER_BRIDGE_LAST_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
static TIMER_BRIDGE_LAST_DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);
static TIMER_EVENT_HOOK_CALLS: AtomicU64 = AtomicU64::new(0);
static TIMER_EVENT_PROCESSOR_INSTALLED: AtomicUsize = AtomicUsize::new(0);
static IRQ_EVENT_PROCESSOR_INSTALLED: AtomicUsize = AtomicUsize::new(0);
static RUNTIME_START_PROCESSOR_INSTALLED: AtomicUsize = AtomicUsize::new(0);
static AXVISOR_RUNTIME_HOOKS_INSTALLED: AtomicUsize = AtomicUsize::new(0);
static RUNTIME_RUN_CALLS: AtomicU64 = AtomicU64::new(0);
static IRQ_EVENT_CALLS: AtomicU64 = AtomicU64::new(0);
static HOST_EXIT_REQUESTED: AtomicUsize = AtomicUsize::new(0);
static HOST_EXIT_CODE: kernel::sync::atomic::Atomic<c_int> = kernel::sync::atomic::Atomic::new(0);
static RUNTIME_LAST_HOST_CPU_NUM: AtomicUsize = AtomicUsize::new(0);
static RUNTIME_LAST_CPU_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static RUNTIME_LAST_RUNTIME_INSTALLED: AtomicUsize = AtomicUsize::new(0);
static RUNTIME_LAST_RUN_CALL_INDEX: AtomicU64 = AtomicU64::new(0);
static RUNTIME_LAST_PROCESSOR_PRESENT: AtomicUsize = AtomicUsize::new(0);
static RUNTIME_LAST_PROCESSOR_INVOKED: AtomicUsize = AtomicUsize::new(0);
static RUNTIME_LAST_FALLBACK_USED: AtomicUsize = AtomicUsize::new(0);
static RUNTIME_LAST_START_PREPARED: AtomicUsize = AtomicUsize::new(0);
static RUNTIME_LAST_START_ENTERED: AtomicUsize = AtomicUsize::new(0);
static RUNTIME_LAST_START_RETURNED: AtomicUsize = AtomicUsize::new(0);
static GLUE_LAST_TIMER_CPU_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static GLUE_LAST_TIMER_DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);
static GLUE_LAST_TIMER_FIRE_COUNT: AtomicU64 = AtomicU64::new(0);
static GLUE_LAST_TIMER_CONSUMED: AtomicUsize = AtomicUsize::new(0);
static GLUE_LAST_IRQ_VECTOR: AtomicUsize = AtomicUsize::new(usize::MAX);
static GLUE_LAST_IRQ_EXTERNAL_MATCHED: AtomicUsize = AtomicUsize::new(0);
static GLUE_LAST_IRQ_CALL_INDEX: AtomicU64 = AtomicU64::new(0);
static GLUE_LAST_IRQ_CONSUMED: AtomicUsize = AtomicUsize::new(0);
static GLUE_LAST_EXTERNAL_EVENT_CPU_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static GLUE_LAST_EXTERNAL_EVENT_CALL_INDEX: AtomicU64 = AtomicU64::new(0);
static GLUE_LAST_EXTERNAL_EVENT_VECTOR: AtomicUsize = AtomicUsize::new(usize::MAX);
static GLUE_LAST_EXTERNAL_EVENT_IRQ_ID: AtomicUsize = AtomicUsize::new(0);
static GLUE_LAST_EXTERNAL_EVENT_CONSUMED: AtomicUsize = AtomicUsize::new(0);
static CONSOLE_SHELL_READY: AtomicUsize = AtomicUsize::new(0);
static IRQ_LAST_EXTERNAL_PATH_MATCHED: AtomicUsize = AtomicUsize::new(0);
static IRQ_LAST_EXTERNAL_PATH_CONSUMED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" {
    static __start_axvisor_percpu: u8;
    static __stop_axvisor_percpu: u8;
}

static AXVISOR_PERCPU_BASE: AtomicUsize = AtomicUsize::new(0);
static AXVISOR_PERCPU_SLOT_SIZE: AtomicUsize = AtomicUsize::new(0);
static AXVISOR_PERCPU_CPU_COUNT: AtomicUsize = AtomicUsize::new(0);

fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

fn axvisor_percpu_template_range() -> (usize, usize) {
    let start = (&raw const __start_axvisor_percpu) as usize;
    let end = (&raw const __stop_axvisor_percpu) as usize;
    (start, end)
}

fn init_axvisor_percpu_backing() -> Result<()> {
    if AXVISOR_PERCPU_BASE.load(Ordering::Acquire) != 0 {
        return Ok(());
    }

    let (template_start, template_end) = axvisor_percpu_template_range();
    if template_end < template_start {
        pr_err!(
            "axvisor_adapter: invalid axvisor_percpu range start=0x{:x} end=0x{:x}\n",
            template_start,
            template_end
        );
        return Err(EINVAL);
    }

    let template_size = template_end - template_start;
    let slot_size = align_up(template_size.max(1), 64);
    let cpu_count = LinuxHostAdapter::get_host_cpu_num().max(1);
    let total_size = slot_size.checked_mul(cpu_count).ok_or(ENOMEM)?;
    let mut backing = KVVec::with_capacity(total_size, GFP_KERNEL)?;
    backing.resize(total_size, 0, GFP_KERNEL)?;

    for cpu_idx in 0..cpu_count {
        let dst = unsafe { backing.as_mut_ptr().add(cpu_idx * slot_size) };
        if template_size != 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(template_start as *const u8, dst, template_size);
            }
        }
    }

    let base = backing.as_ptr() as usize;
    core::mem::forget(backing);
    AXVISOR_PERCPU_SLOT_SIZE.store(slot_size, Ordering::Release);
    AXVISOR_PERCPU_CPU_COUNT.store(cpu_count, Ordering::Release);
    AXVISOR_PERCPU_BASE.store(base, Ordering::Release);
    pr_info!(
        "axvisor_adapter: percpu backing base=0x{:x} cpu_count={} template_size={} slot_size={} total_size={}\n",
        base,
        cpu_count,
        template_size,
        slot_size,
        total_size
    );
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn _percpu_base_ptr(cpu_idx: usize) -> *mut u8 {
    let base = AXVISOR_PERCPU_BASE.load(Ordering::Acquire);
    let slot_size = AXVISOR_PERCPU_SLOT_SIZE.load(Ordering::Acquire);
    let cpu_count = AXVISOR_PERCPU_CPU_COUNT.load(Ordering::Acquire);
    if base == 0 || slot_size == 0 || cpu_idx >= cpu_count {
        pr_err!(
            "axvisor_adapter: invalid percpu base request cpu_idx={} base=0x{:x} slot_size={} cpu_count={}\n",
            cpu_idx,
            base,
            slot_size,
            cpu_count
        );
        return core::ptr::null_mut();
    }
    (base + cpu_idx * slot_size) as *mut u8
}
static IRQ_LAST_LOCAL_PATH_HIT: AtomicUsize = AtomicUsize::new(0);
static IRQ_LAST_FINAL_RESULT: AtomicUsize = AtomicUsize::new(0);
static IRQ_EXTERNAL_REGISTRATION_CALLS: AtomicU64 = AtomicU64::new(0);
static IRQ_EXTERNAL_PENDING_PUSHES: AtomicU64 = AtomicU64::new(0);
static IRQ_EXTERNAL_DRAIN_CALLS: AtomicU64 = AtomicU64::new(0);
static IRQ_EXTERNAL_LAST_DRAINED_COUNT: AtomicUsize = AtomicUsize::new(0);
static IRQ_EXTERNAL_LAST_PENDING_DEPTH: AtomicUsize = AtomicUsize::new(0);
static IRQ_EXTERNAL_LAST_DRAIN_EVENT_CPU_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static IRQ_EXTERNAL_LAST_DRAIN_EVENT_CALL_INDEX: AtomicU64 = AtomicU64::new(0);
static IRQ_LAST_REGISTER_VECTOR: AtomicUsize = AtomicUsize::new(usize::MAX);
static IRQ_LAST_REGISTER_EXTERNAL: AtomicUsize = AtomicUsize::new(0);
static IRQ_LAST_REGISTER_ARCH_OK: AtomicUsize = AtomicUsize::new(0);
static IRQ_LAST_REGISTER_LOCAL_INSTALLED: AtomicUsize = AtomicUsize::new(0);
static IRQ_LAST_REGISTER_RESULT: AtomicUsize = AtomicUsize::new(0);
static IRQ_LOCAL_HANDLER_INSTALLED_COUNT: AtomicUsize = AtomicUsize::new(0);
static IRQ_EXTERNAL_HANDLER_SLOT_CLAIMED: AtomicUsize = AtomicUsize::new(0);
static IRQ_HANDLE_LOG_COUNT: AtomicU64 = AtomicU64::new(0);
static PASSTHROUGH_DEVICE_COUNT: AtomicUsize = AtomicUsize::new(0);
static PASSTHROUGH_DEVICE_BASE_HPA: [AtomicU64; MAX_PASSTHROUGH_DEVICES] =
    [const { AtomicU64::new(0) }; MAX_PASSTHROUGH_DEVICES];
static PASSTHROUGH_DEVICE_LENGTH: [AtomicU64; MAX_PASSTHROUGH_DEVICES] =
    [const { AtomicU64::new(0) }; MAX_PASSTHROUGH_DEVICES];
static PASSTHROUGH_DEVICE_IRQ_ID: [AtomicUsize; MAX_PASSTHROUGH_DEVICES] =
    [const { AtomicUsize::new(0) }; MAX_PASSTHROUGH_DEVICES];
static PASSTHROUGH_DEVICE_VM_ID: [AtomicUsize; MAX_PASSTHROUGH_DEVICES] =
    [const { AtomicUsize::new(usize::MAX) }; MAX_PASSTHROUGH_DEVICES];
const CONSOLE_INPUT_BUFFER_CAPACITY: usize = 512;

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static TASK_REGISTRY: Mutex<TaskRegistry> = TaskRegistry {
        entries: KVVec::new(),
    };
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static WAIT_QUEUE_REGISTRY: Mutex<WaitQueueRegistry> = WaitQueueRegistry {
        entries: KVVec::new(),
    };
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static CONSOLE_INPUT_BUFFER: Mutex<KVVec<u8>> = KVVec::new();
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static CONSOLE_INPUT_STATE: Mutex<Option<Arc<ConsoleInputState>>> = None;
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static HOST_EMERG_LINE_BUFFER: Mutex<KVVec<u8>> = KVVec::new();
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static GUEST_CONSOLE_INPUT_BUFFER: Mutex<KVVec<u8>> = KVVec::new();
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static GUEST_CONSOLE_OUTPUT_BUFFER: Mutex<KVVec<u8>> = KVVec::new();
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static EXTERNAL_IRQ_PENDING: Mutex<KVVec<ExternalIrqEvent>> = KVVec::new();
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static EXTERNAL_IRQ_REGISTRATION: Mutex<Option<ExternalIrqRegistration>> = None;
}

struct WaitQueueState {
    next_ticket: usize,
    woken_ticket: usize,
}

struct WaitQueueRegistryEntry {
    id: usize,
    queue: Arc<LinuxWaitQueueRecord>,
}

struct WaitQueueRegistry {
    entries: KVVec<WaitQueueRegistryEntry>,
}

struct TaskRegistryEntry {
    pid: kernel::task::Pid,
    handle: Arc<TaskHandleRecord>,
}

struct TaskRegistry {
    entries: KVVec<TaskRegistryEntry>,
}

struct ExternalIrqRegistration {
    vector: usize,
    pending_pushes: u64,
    drain_calls: u64,
    last_drained_count: usize,
    last_event_cpu_id: usize,
    last_event_call_index: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct ExternalIrqEvent {
    vector: usize,
    irq_id: usize,
    vm_id: usize,
    cpu_id: usize,
    call_index: u64,
}

#[pin_data]
struct LinuxWaitQueueEntry {
    #[pin]
    state: Mutex<WaitQueueState>,
    #[pin]
    cv: CondVar,
}

struct LinuxWaitQueueRecord {
    queue: Pin<KBox<LinuxWaitQueueEntry>>,
    destroyed: AtomicUsize,
}

impl LinuxWaitQueueRecord {
    fn new() -> Result<Arc<Self>> {
        Ok(Arc::new(
            Self {
                queue: KBox::pin_init(LinuxWaitQueueEntry::new(), GFP_KERNEL)?,
                destroyed: AtomicUsize::new(0),
            },
            GFP_KERNEL,
        )?)
    }

    fn mark_destroyed(&self) {
        self.destroyed.store(1, Ordering::Release);
        let queue = self.queue.as_ref().get_ref();
        let mut guard = queue.state.lock();
        guard.woken_ticket = guard.next_ticket;
        queue.cv.notify_all();
    }

    fn is_destroyed(&self) -> bool {
        self.destroyed.load(Ordering::Acquire) != 0
    }
}

trait KernelTaskRuntime: Sync {
    // 1. KernelTaskRuntime::spawn_task
    fn spawn_task(
        &self,
        entry: KBox<dyn TaskEntry>,
        cpu_affinity: AxvisorCpuSet,
    ) -> Result<TaskHandle>;
}

#[derive(Clone, Copy)]
struct AxvisorCpuSet {
    mask: usize,
}

impl AxvisorCpuSet {
    const fn from_mask(mask: usize) -> Self {
        Self { mask }
    }
}

struct LinuxKernelTaskRuntime;
struct LinuxHostAdapter;
struct LinuxConsoleAdapter;
struct LinuxGuestConsoleAdapter;
struct LinuxTimeAdapter;
struct LinuxSyncAdapter;
struct LinuxIrqAdapter;
struct LinuxMemoryAdapter;
struct LinuxRuntimeAdapter;
struct AxvisorCoreGlue;

#[pin_data]
struct ConsoleInputState {
    #[pin]
    pending: Mutex<bool>,
    #[pin]
    cv: CondVar,
}

#[derive(Clone, Copy)]
pub(crate) struct AxvisorTimerEventContext {
    cpu_id: usize,
    deadline_nanos: u64,
    fire_count: u64,
}

type LinuxTimerEventProcessor = fn(AxvisorTimerEventContext);
type LinuxTimerCoreEntryInvoker = fn(AxvisorTimerEventContext) -> bool;

#[derive(Clone, Copy)]
struct AxvisorIrqEventContext {
    vector: usize,
    dispatch_external_matched: bool,
    call_index: u64,
}

type LinuxIrqEventProcessor = fn(AxvisorIrqEventContext) -> bool;
type LinuxExternalIrqCoreEntryInvoker = fn(ExternalIrqEvent) -> bool;

#[derive(Clone, Copy)]
pub(crate) struct AxvisorRuntimeStartContext {
    host_cpu_num: usize,
    current_cpu_id: usize,
    kernel_task_runtime_installed: bool,
    run_call_index: u64,
}

type LinuxRuntimeStartProcessor = fn(AxvisorRuntimeStartContext);
type LinuxCoreEntryInvoker = fn(AxvisorRuntimeStartContext);

#[derive(Clone, Copy)]
struct AxvisorRuntimeHooks {
    // Future target: AxVisor timer event processing entry.
    timer_event_processor: LinuxTimerEventProcessor,
    // Future target: AxVisor-side IRQ handoff / inject decision entry.
    irq_event_processor: LinuxIrqEventProcessor,
    // Future target: `axvisor_core::boot::run()`.
    runtime_start_processor: LinuxRuntimeStartProcessor,
}

trait TaskEntry: Send {
    fn call(self: KBox<Self>);
}

impl<F> TaskEntry for F
where
    F: FnOnce() + Send + 'static,
{
    fn call(self: KBox<Self>) {
        let func = KBox::into_inner(self);
        func();
    }
}

struct AxvisorTaskStart {
    entry: Option<KBox<dyn TaskEntry>>,
    state: Arc<TaskState>,
    registered: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    cpu_affinity_mask: usize,
}

struct TaskStateInner {
    finished: bool,
    exit_code: c_int,
}

#[pin_data]
struct TaskState {
    #[pin]
    inner: Mutex<TaskStateInner>,
    #[pin]
    cv: CondVar,
}

impl TaskState {
    fn new() -> Result<Arc<Self>> {
        Arc::pin_init(
            pin_init::pin_init!(Self {
                inner <- new_mutex!(TaskStateInner {
                    finished: false,
                    exit_code: 0,
                }),
                cv <- new_condvar!(),
            }),
            GFP_KERNEL,
        )
    }

    fn new_finished(exit_code: c_int) -> Result<Arc<Self>> {
        Arc::pin_init(
            pin_init::pin_init!(Self {
                inner <- new_mutex!(TaskStateInner {
                    finished: true,
                    exit_code,
                }),
                cv <- new_condvar!(),
            }),
            GFP_KERNEL,
        )
    }

    fn finish(&self, exit_code: c_int) {
        let mut guard = self.inner.lock();
        guard.finished = true;
        guard.exit_code = exit_code;
        self.cv.notify_all();
    }

    fn wait(&self) -> c_int {
        let mut guard = self.inner.lock();
        while !guard.finished {
            self.cv.wait(&mut guard);
        }
        guard.exit_code
    }
}

struct TaskHandle {
    state: Arc<TaskState>,
    record: Arc<TaskHandleRecord>,
}

struct TaskHandleRecord {
    pid: kernel::task::Pid,
    state: Arc<TaskState>,
}

impl TaskHandleRecord {
    fn new(pid: kernel::task::Pid, state: Arc<TaskState>) -> Result<Arc<Self>> {
        Ok(Arc::new(Self { pid, state }, GFP_KERNEL)?)
    }
}

impl TaskHandle {
    fn pid(&self) -> kernel::task::Pid {
        self.record.pid
    }

    fn join(self) -> Result<c_int> {
        unregister_task_handle(self.record.pid);
        Ok(self.state.wait())
    }

    fn current() -> Result<Self> {
        let state = TaskState::new_finished(0)?;
        let pid = current!().pid();
        Ok(Self {
            state: state.clone(),
            record: TaskHandleRecord::new(pid, state)?,
        })
    }

    fn yield_now() {
        unsafe { axvisor_adapter_yield() };
    }
}

impl LinuxHostAdapter {
    // 4. HostIf::get_host_cpu_num
    fn get_host_cpu_num() -> usize {
        unsafe { axvisor_adapter_host_cpu_num() }
    }

    fn current_cpu_id() -> usize {
        unsafe { axvisor_adapter_current_cpu_id() }
    }

    // 5. HostIf::init_percpu
    fn init_percpu() {
        let cpu_id = Self::current_cpu_id();
        let _ = arch::init_percpu(cpu_id);
    }

    fn release_host_filesystems() -> c_int {
        let calls = HOST_RELEASE_FILESYSTEMS_CALLS.fetch_add(1, Ordering::AcqRel) + 1;
        pr_info!(
            "axvisor_adapter: release_host_filesystems call={} before guest passthrough ownership\n",
            calls
        );
        unsafe { axvisor_adapter_release_host_filesystems() }
    }

    // 6. HostIf::exit
    fn exit(exit_code: i32) -> ! {
        HOST_EXIT_CODE.store(exit_code as c_int, KRelease);
        HOST_EXIT_REQUESTED.store(1, Ordering::Release);
        pr_emerg!(
            "axvisor_adapter: host exit requested exit_code={} host_cpu_num={} current_cpu_id={} runtime_installed={} run_call_index={} start_prepared={} start_entered={} start_returned={} processor_present={} processor_invoked={} fallback_used={}\n",
            exit_code,
            RUNTIME_LAST_HOST_CPU_NUM.load(Ordering::Relaxed),
            RUNTIME_LAST_CPU_ID.load(Ordering::Relaxed),
            RUNTIME_LAST_RUNTIME_INSTALLED.load(Ordering::Relaxed),
            RUNTIME_LAST_RUN_CALL_INDEX.load(Ordering::Relaxed),
            RUNTIME_LAST_START_PREPARED.load(Ordering::Relaxed),
            RUNTIME_LAST_START_ENTERED.load(Ordering::Relaxed),
            RUNTIME_LAST_START_RETURNED.load(Ordering::Relaxed),
            RUNTIME_LAST_PROCESSOR_PRESENT.load(Ordering::Relaxed),
            RUNTIME_LAST_PROCESSOR_INVOKED.load(Ordering::Relaxed),
            RUNTIME_LAST_FALLBACK_USED.load(Ordering::Relaxed),
        );
        unsafe { axvisor_adapter_host_exit(exit_code as c_int) }
    }
}

impl LinuxConsoleAdapter {
    fn console_input_state() -> Option<Arc<ConsoleInputState>> {
        CONSOLE_INPUT_STATE.lock().as_ref().cloned()
    }

    // 7. ConsoleIf::write_bytes
    fn write_bytes(bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        unsafe { axvisor_adapter_console_write(bytes.as_ptr(), bytes.len()) };
    }

    // 8. ConsoleIf::read_bytes
    fn read_bytes(bytes: &mut [u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        pr_info!(
            "axvisor_adapter: console read_bytes enter want={}\n",
            bytes.len()
        );

        loop {
            let mut buffer = CONSOLE_INPUT_BUFFER.lock();
            let pending_before = buffer.len();
            let target = core::cmp::min(bytes.len(), buffer.len());
            let mut read = 0;
            for slot in bytes.iter_mut().take(target) {
                match buffer.remove(0) {
                    Ok(byte) => {
                        *slot = byte;
                        read += 1;
                    }
                    Err(_) => break,
                }
            }
            if read != 0 {
                let pending_after = buffer.len();
                drop(buffer);
                if pending_after == 0 {
                    if let Some(input_state) = Self::console_input_state() {
                        let mut pending = input_state.pending.lock();
                        *pending = false;
                    }
                }
                pr_info!(
                    "axvisor_adapter: console read_bytes read={} pending_before={} pending_after={}\n",
                    read,
                    pending_before,
                    pending_after
                );
                return read;
            }

            drop(buffer);
            let Some(input_state) = Self::console_input_state() else {
                pr_info!("axvisor_adapter: console read_bytes no input_state\n");
                return 0;
            };
            let mut pending = input_state.pending.lock();
            while !*pending {
                pr_info!(
                    "axvisor_adapter: console read_bytes sleeping pending={} buffered={}\n",
                    *pending,
                    pending_before
                );
                input_state.cv.wait(&mut pending);
                pr_info!(
                    "axvisor_adapter: console read_bytes woke pending={}\n",
                    *pending
                );
            }
        }
    }

    fn enqueue_bytes(bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        let mut buffer = CONSOLE_INPUT_BUFFER.lock();
        if buffer.reserve(bytes.len(), GFP_KERNEL).is_err() {
            pr_warn!(
                "axvisor_adapter: console enqueue_bytes reserve failed requested={} buffered={}\n",
                bytes.len(),
                buffer.len()
            );
            return 0;
        }
        let mut written = 0;

        for &byte in bytes {
            if buffer.push(byte, GFP_KERNEL).is_err() {
                break;
            }
            written += 1;
        }

        pr_info!(
            "axvisor_adapter: console enqueue_bytes requested={} written={} pending={}\n",
            bytes.len(),
            written,
            buffer.len()
        );
        drop(buffer);

        if written != 0 {
            if let Some(input_state) = Self::console_input_state() {
                let mut pending = input_state.pending.lock();
                *pending = true;
                pr_info!(
                    "axvisor_adapter: console enqueue_bytes notify pending={} written={}\n",
                    *pending,
                    written
                );
                input_state.cv.notify_all();
            }
        }

        written
    }

}

impl LinuxGuestConsoleAdapter {
    fn write_bytes(bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        unsafe { axvisor_adapter_guest_console_write(bytes.as_ptr(), bytes.len()) };
    }

    fn write_bytes_to_rust_buffer(bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let mut buffer = GUEST_CONSOLE_OUTPUT_BUFFER.lock();
        let needed = bytes.len();
        if buffer.reserve(needed, GFP_ATOMIC).is_err() {
            buffer.clear();
            let _ = buffer.reserve(needed, GFP_ATOMIC);
        }

        for &byte in bytes {
            while buffer.len() >= GUEST_CONSOLE_OUTPUT_BUFFER_CAPACITY {
                let _ = buffer.remove(0);
            }
            let _ = buffer.push(byte, GFP_ATOMIC);
        }

        let buffered = buffer.len();
        drop(buffer);
        pr_debug!(
            "axvisor_adapter: guest console output write len={} buffered={}\n",
            bytes.len(),
            buffered
        );
    }

    fn read_input_bytes(bytes: &mut [u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        unsafe { axvisor_adapter_guest_console_read(bytes.as_mut_ptr(), bytes.len()) }
    }

    fn read_input_bytes_from_rust_buffer(bytes: &mut [u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        let mut buffer = GUEST_CONSOLE_INPUT_BUFFER.lock();
        let target = core::cmp::min(bytes.len(), buffer.len());
        let mut read = 0;

        for slot in bytes.iter_mut().take(target) {
            match buffer.remove(0) {
                Ok(byte) => {
                    *slot = byte;
                    read += 1;
                }
                Err(_) => break,
            }
        }

        read
    }

    fn enqueue_input_bytes(bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        let mut buffer = GUEST_CONSOLE_INPUT_BUFFER.lock();
        if buffer.reserve(bytes.len(), GFP_KERNEL).is_err() {
            pr_warn!(
                "axvisor_adapter: guest console input reserve failed requested={} buffered={}\n",
                bytes.len(),
                buffer.len()
            );
            return 0;
        }

        let mut written = 0;
        for &byte in bytes {
            if buffer.len() >= GUEST_CONSOLE_INPUT_BUFFER_CAPACITY {
                break;
            }
            if buffer.push(byte, GFP_KERNEL).is_err() {
                break;
            }
            written += 1;
        }

        pr_info!(
            "axvisor_adapter: guest console input enqueue requested={} written={} buffered={}\n",
            bytes.len(),
            written,
            buffer.len()
        );

        written
    }

    fn drain_output_bytes(bytes: &mut [u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        let mut buffer = GUEST_CONSOLE_OUTPUT_BUFFER.lock();
        let target = core::cmp::min(bytes.len(), buffer.len());
        let mut read = 0;

        for slot in bytes.iter_mut().take(target) {
            match buffer.remove(0) {
                Ok(byte) => {
                    *slot = byte;
                    read += 1;
                }
                Err(_) => break,
            }
        }

        if read != 0 {
            pr_info!(
                "axvisor_adapter: guest console output drain requested={} read={} remaining={}\n",
                bytes.len(),
                read,
                buffer.len()
            );
        }

        read
    }
}

impl LinuxTimeAdapter {
    // 9. TimeIf::current_time_nanos
    fn current_time_nanos() -> u64 {
        unsafe { axvisor_adapter_current_time_nanos() }
    }

    // 10. TimeIf::set_oneshot_timer
    fn set_oneshot_timer(deadline_nanos: u64) {
        arch::set_oneshot_timer(deadline_nanos);
    }

    fn install_timer_event_processor(processor: LinuxTimerEventProcessor) -> bool {
        if TIMER_EVENT_PROCESSOR_INSTALLED
            .compare_exchange(0, processor as usize, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return true;
        }

        false
    }

    fn timer_event_processor() -> Option<LinuxTimerEventProcessor> {
        let raw = TIMER_EVENT_PROCESSOR_INSTALLED.load(Ordering::Acquire);
        if raw == 0 {
            return None;
        }

        // SAFETY: `install_timer_event_processor` stores only valid function pointers.
        Some(unsafe { core::mem::transmute(raw) })
    }
}

impl LinuxRuntimeAdapter {
    fn install_irq_event_processor(processor: LinuxIrqEventProcessor) -> bool {
        IRQ_EVENT_PROCESSOR_INSTALLED
            .compare_exchange(0, processor as usize, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn irq_event_processor() -> Option<LinuxIrqEventProcessor> {
        let raw = IRQ_EVENT_PROCESSOR_INSTALLED.load(Ordering::Acquire);
        if raw == 0 {
            return None;
        }

        // SAFETY: `install_irq_event_processor` stores only valid function pointers.
        Some(unsafe { core::mem::transmute(raw) })
    }

    fn install_runtime_start_processor(processor: LinuxRuntimeStartProcessor) -> bool {
        RUNTIME_START_PROCESSOR_INSTALLED
            .compare_exchange(0, processor as usize, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn runtime_start_processor() -> Option<LinuxRuntimeStartProcessor> {
        let raw = RUNTIME_START_PROCESSOR_INSTALLED.load(Ordering::Acquire);
        if raw == 0 {
            return None;
        }

        // SAFETY: `install_runtime_start_processor` stores only valid function pointers.
        Some(unsafe { core::mem::transmute(raw) })
    }

    fn install_axvisor_runtime_hooks(hooks: AxvisorRuntimeHooks) -> bool {
        if AXVISOR_RUNTIME_HOOKS_INSTALLED
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }

        LinuxTimeAdapter::install_timer_event_processor(hooks.timer_event_processor)
            && Self::install_irq_event_processor(hooks.irq_event_processor)
            && Self::install_runtime_start_processor(hooks.runtime_start_processor)
    }
}

fn linux_timer_event_hook(_event: AxvisorTimerEventContext) {
    TIMER_EVENT_HOOK_CALLS.fetch_add(1, Ordering::AcqRel);
}

fn linux_timer_event_context(cpu_id: usize, deadline_nanos: u64) -> AxvisorTimerEventContext {
    AxvisorTimerEventContext {
        cpu_id,
        deadline_nanos,
        fire_count: arch::timer_fire_count(),
    }
}

fn linux_timer_bridge(cpu_id: usize, deadline_nanos: u64) {
    let event = linux_timer_event_context(cpu_id, deadline_nanos);
    TIMER_BRIDGE_LAST_CPU.store(cpu_id, Ordering::Release);
    TIMER_BRIDGE_LAST_DEADLINE.store(deadline_nanos, Ordering::Release);
    linux_timer_event_hook(event);
    if let Some(processor) = LinuxTimeAdapter::timer_event_processor() {
        processor(event);
    }
}

impl AxvisorCoreGlue {
    fn runtime_hooks() -> AxvisorRuntimeHooks {
        AxvisorRuntimeHooks {
            timer_event_processor: Self::timer_event_processor,
            irq_event_processor: Self::irq_event_processor,
            runtime_start_processor: Self::runtime_start_processor,
        }
    }

    fn install() -> bool {
        LinuxRuntimeAdapter::install_axvisor_runtime_hooks(Self::runtime_hooks())
    }

    fn record_timer_event(event: AxvisorTimerEventContext) {
        GLUE_LAST_TIMER_CPU_ID.store(event.cpu_id, Ordering::Release);
        GLUE_LAST_TIMER_DEADLINE.store(event.deadline_nanos, Ordering::Release);
        GLUE_LAST_TIMER_FIRE_COUNT.store(event.fire_count, Ordering::Release);
        GLUE_LAST_TIMER_CONSUMED.store(0, Ordering::Release);
    }

    fn timer_core_entry_invoker() -> LinuxTimerCoreEntryInvoker {
        // Future target: return a dispatcher that invokes `axvisor_core::vmm::timer::check_events()`.
        core_link::timer::timer_check_events
    }

    fn invoke_timer_event_core(event: AxvisorTimerEventContext) -> bool {
        let invoker = Self::timer_core_entry_invoker();
        pr_info!(
            "axvisor_adapter: timer event -> core cpu_id={} deadline_nanos={} fire_count={}\n",
            event.cpu_id,
            event.deadline_nanos,
            event.fire_count
        );
        invoker(event)
    }

    fn timer_event_processor(event: AxvisorTimerEventContext) {
        Self::record_timer_event(event);
        GLUE_LAST_TIMER_CONSUMED.store(
            usize::from(Self::invoke_timer_event_core(event)),
            Ordering::Release,
        );
    }

    fn record_irq_event(event: AxvisorIrqEventContext) {
        GLUE_LAST_IRQ_VECTOR.store(event.vector, Ordering::Release);
        GLUE_LAST_IRQ_EXTERNAL_MATCHED.store(
            usize::from(event.dispatch_external_matched),
            Ordering::Release,
        );
        GLUE_LAST_IRQ_CALL_INDEX.store(event.call_index, Ordering::Release);
        GLUE_LAST_IRQ_CONSUMED.store(0, Ordering::Release);
    }

    fn record_external_irq_event(event: ExternalIrqEvent) {
        GLUE_LAST_EXTERNAL_EVENT_VECTOR.store(event.vector, Ordering::Release);
        GLUE_LAST_EXTERNAL_EVENT_IRQ_ID.store(event.irq_id, Ordering::Release);
        GLUE_LAST_EXTERNAL_EVENT_CPU_ID.store(event.cpu_id, Ordering::Release);
        GLUE_LAST_EXTERNAL_EVENT_CALL_INDEX.store(event.call_index, Ordering::Release);
        GLUE_LAST_EXTERNAL_EVENT_CONSUMED.store(0, Ordering::Release);
    }

    fn future_irq_event_hook(_event: AxvisorIrqEventContext) -> bool {
        // External IRQs are queued on the host IRQ path and consumed here by
        // the AxVisor-side injection path.
        let drained = LinuxIrqAdapter::drain_external_pending();
        if drained.is_empty() {
            return false;
        }
        Self::process_external_irq_events(drained.as_slice())
    }

    fn external_irq_core_entry_invoker() -> LinuxExternalIrqCoreEntryInvoker {
        // Future target: return a dispatcher that performs real guest interrupt injection.
        core_link::irq::inject_external_interrupt
    }

    fn invoke_external_irq_core(event: ExternalIrqEvent) -> bool {
        let invoker = Self::external_irq_core_entry_invoker();
        pr_info!(
            "axvisor_adapter: external irq -> core vector={} irq_id={} vm_id={} cpu_id={} call_index={}\n",
            event.vector,
            event.irq_id,
            event.vm_id,
            event.cpu_id,
            event.call_index
        );
        invoker(event)
    }

    fn process_external_irq_events(events: &[ExternalIrqEvent]) -> bool {
        let mut consumed_any = false;
        for &event in events {
            Self::record_external_irq_event(event);
            let consumed = Self::invoke_external_irq_core(event);
            GLUE_LAST_EXTERNAL_EVENT_CONSUMED.store(usize::from(consumed), Ordering::Release);
            consumed_any |= consumed;
        }
        consumed_any
    }

    fn irq_event_processor(event: AxvisorIrqEventContext) -> bool {
        Self::record_irq_event(event);
        let consumed = Self::future_irq_event_hook(event);
        GLUE_LAST_IRQ_CONSUMED.store(usize::from(consumed), Ordering::Release);
        consumed
    }

    fn record_runtime_start(ctx: AxvisorRuntimeStartContext) {
        RUNTIME_LAST_HOST_CPU_NUM.store(ctx.host_cpu_num, Ordering::Release);
        RUNTIME_LAST_CPU_ID.store(ctx.current_cpu_id, Ordering::Release);
        RUNTIME_LAST_RUNTIME_INSTALLED.store(
            usize::from(ctx.kernel_task_runtime_installed),
            Ordering::Release,
        );
        RUNTIME_LAST_RUN_CALL_INDEX.store(ctx.run_call_index, Ordering::Release);
        RUNTIME_LAST_START_PREPARED.store(1, Ordering::Release);
        RUNTIME_LAST_START_ENTERED.store(0, Ordering::Release);
        RUNTIME_LAST_START_RETURNED.store(0, Ordering::Release);
    }

    fn prepare_runtime_start(_ctx: AxvisorRuntimeStartContext) {
        // Future target: perform any last host-side setup before entering AxVisor core.
    }

    fn runtime_core_entry_invoker() -> LinuxCoreEntryInvoker {
        // Future target: return a dispatcher that invokes `axvisor_core::boot::run()`.
        core_link::boot::boot_run
    }

    fn invoke_runtime_start_core(ctx: AxvisorRuntimeStartContext) {
        RUNTIME_LAST_START_ENTERED.store(1, Ordering::Release);
        let invoker = Self::runtime_core_entry_invoker();
        pr_info!(
            "axvisor_adapter: runtime start -> core host_cpu_num={} current_cpu_id={} runtime_installed={} run_call_index={}\n",
            ctx.host_cpu_num,
            ctx.current_cpu_id,
            ctx.kernel_task_runtime_installed,
            ctx.run_call_index
        );
        invoker(ctx);
    }

    fn finalize_runtime_start(_ctx: AxvisorRuntimeStartContext) {
        // Future target: post-run cleanup / shutdown handoff if AxVisor core returns.
    }

    fn future_runtime_start_hook(ctx: AxvisorRuntimeStartContext) {
        Self::prepare_runtime_start(ctx);
        Self::invoke_runtime_start_core(ctx);
        Self::finalize_runtime_start(ctx);
    }

    fn runtime_start_processor(ctx: AxvisorRuntimeStartContext) {
        Self::record_runtime_start(ctx);
        Self::future_runtime_start_hook(ctx);
        RUNTIME_LAST_START_RETURNED.store(1, Ordering::Release);
    }
}

type LinuxIrqHandler = fn(usize);

impl LinuxWaitQueueEntry {
    fn new() -> impl pin_init::PinInit<Self> {
        pin_init::pin_init!(Self {
            state <- new_mutex!(WaitQueueState {
                next_ticket: 0,
                woken_ticket: 0,
            }),
            cv <- new_condvar!(),
        })
    }
}

impl LinuxSyncAdapter {
    // 11. SyncIf::create_wait_queue
    fn create_wait_queue() -> Result<usize> {
        let mut id = WAIT_QUEUE_IDS.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            id = WAIT_QUEUE_IDS.fetch_add(1, Ordering::Relaxed);
        }
        let queue = LinuxWaitQueueRecord::new()?;
        WAIT_QUEUE_REGISTRY.lock().entries.push(
            WaitQueueRegistryEntry {
                id,
                queue,
            },
            GFP_KERNEL,
        )?;
        Ok(id)
    }

    // 12. SyncIf::destroy_wait_queue
    fn destroy_wait_queue(queue: usize) {
        if let Some(record) = remove_wait_queue(queue) {
            record.mark_destroyed();
        }
    }

    // 13. SyncIf::wait_queue_wait
    fn wait_queue_wait(queue: usize) {
        let Some(queue) = lookup_wait_queue(queue) else {
            return;
        };
        if queue.is_destroyed() {
            return;
        }
        let queue_ref = queue.queue.as_ref().get_ref();
        let mut guard = queue_ref.state.lock();
        let ticket = guard.next_ticket;
        guard.next_ticket += 1;
        while !queue.is_destroyed() && guard.woken_ticket <= ticket {
            queue_ref.cv.wait(&mut guard);
        }
    }

    // 14. SyncIf::wait_queue_wait_until
    fn wait_queue_wait_until(
        queue: usize,
        condition: &mut dyn FnMut() -> bool,
    ) {
        let Some(queue) = lookup_wait_queue(queue) else {
            return;
        };
        loop {
            if queue.is_destroyed() {
                pr_debug!(
                    "axvisor_adapter: wait_queue_wait_until destroyed queue={}\n",
                    Arc::as_ptr(&queue) as usize,
                );
                return;
            }
            if condition() {
                pr_debug!(
                    "axvisor_adapter: wait_queue_wait_until condition-satisfied queue={}\n",
                    Arc::as_ptr(&queue) as usize,
                );
                return;
            }

            let queue_ref = queue.queue.as_ref().get_ref();
            let mut guard = queue_ref.state.lock();
            if queue.is_destroyed() {
                pr_debug!(
                    "axvisor_adapter: wait_queue_wait_until destroyed-after-lock queue={} next_ticket={} woken_ticket={}\n",
                    Arc::as_ptr(&queue) as usize,
                    guard.next_ticket,
                    guard.woken_ticket,
                );
                return;
            }
            if condition() {
                pr_debug!(
                    "axvisor_adapter: wait_queue_wait_until condition-met queue={} next_ticket={} woken_ticket={}\n",
                    Arc::as_ptr(&queue) as usize,
                    guard.next_ticket,
                    guard.woken_ticket,
                );
                return;
            }
            let ticket = guard.next_ticket;
            guard.next_ticket += 1;
            pr_debug!(
                "axvisor_adapter: wait_queue_wait_until sleeping queue={} ticket={} next_ticket={} woken_ticket={}\n",
                Arc::as_ptr(&queue) as usize,
                ticket,
                guard.next_ticket,
                guard.woken_ticket,
            );
            // Bounded wait: `condition` here is often time-based (the x86 idle
            // HLT path blocks on `current_time_nanos() >= deadline_ns`). The
            // wake is delivered by the host hrtimer callback via
            // notify_all_registered_vcpus(), but that notify can be lost if it
            // fires on the wake edge before this waiter takes its ticket, or if
            // the callback targets a different queue. A plain untimed cv.wait()
            // then sleeps forever even though wall-clock time has already passed
            // the deadline (observed: idle vCPU hangs in pv_native_safe_halt,
            // jiffies freeze). Wait with a short timeout so the loop re-checks
            // the time-based condition on its own, making the deadline the
            // correctness backstop the caller relies on. The explicit notify
            // still provides the low-latency wake in the common case.
            while !queue.is_destroyed() && !condition() && guard.woken_ticket <= ticket {
                let _ = queue_ref.cv.wait_interruptible_timeout(
                    &mut guard,
                    kernel::time::msecs_to_jiffies(2),
                );
            }
            if !queue.is_destroyed() && !condition() {
                pr_debug!(
                    "axvisor_adapter: wait_queue_wait_until spurious-wake queue={} ticket={} next_ticket={} woken_ticket={}\n",
                    Arc::as_ptr(&queue) as usize,
                    ticket,
                    guard.next_ticket,
                    guard.woken_ticket,
                );
                continue;
            }
        }
    }

    // 15. SyncIf::wait_queue_wake_one
    fn wait_queue_wake_one(queue: usize) {
        let Some(queue) = lookup_wait_queue(queue) else {
            return;
        };
        if queue.is_destroyed() {
            return;
        }
        let queue = queue.queue.as_ref().get_ref();
        let mut guard = queue.state.lock();
        pr_debug!(
            "axvisor_adapter: wait_queue_wake_one queue={} next_ticket={} woken_ticket={}\n",
            queue as *const _ as usize,
            guard.next_ticket,
            guard.woken_ticket,
        );
        if guard.woken_ticket >= guard.next_ticket {
            return;
        }
        guard.woken_ticket += 1;
        queue.cv.notify_one();
    }

    // 16. SyncIf::wait_queue_wake_all
    fn wait_queue_wake_all(queue: usize) {
        let Some(queue) = lookup_wait_queue(queue) else {
            return;
        };
        if queue.is_destroyed() {
            return;
        }
        let queue = queue.queue.as_ref().get_ref();
        let mut guard = queue.state.lock();
        guard.woken_ticket = guard.next_ticket;
        queue.cv.notify_all();
    }
}

impl LinuxIrqAdapter {
    fn core_external_irq_entry_registered() -> bool {
        #[cfg(target_arch = "riscv64")]
        {
            vendor::axvisor_core::arch::riscv64::external_irq_entry_registered()
        }
        #[cfg(not(target_arch = "riscv64"))]
        {
            false
        }
    }

    fn irq_event_context(vector: usize, dispatch_external_matched: bool) -> AxvisorIrqEventContext {
        AxvisorIrqEventContext {
            vector,
            dispatch_external_matched,
            call_index: IRQ_EVENT_CALLS.fetch_add(1, Ordering::AcqRel) + 1,
        }
    }

    fn handle_external_irq_path(vector: usize) -> bool {
        let external_matched = arch::dispatch_external_irq(vector);
        IRQ_LAST_EXTERNAL_PATH_MATCHED.store(usize::from(external_matched), Ordering::Release);
        let event = Self::irq_event_context(vector, external_matched);
        let log_idx = event.call_index as usize;
        if log_idx <= 32 || log_idx.is_power_of_two() {
            pr_emerg!(
                "axvisor_adapter: external_irq path enter vector={} matched={} pending_depth={} registration_calls={} slot_claimed={} core_entry_registered={} call_index={}\n",
                vector,
                external_matched,
                IRQ_EXTERNAL_LAST_PENDING_DEPTH.load(Ordering::Acquire),
                IRQ_EXTERNAL_REGISTRATION_CALLS.load(Ordering::Acquire),
                IRQ_EXTERNAL_HANDLER_SLOT_CLAIMED.load(Ordering::Acquire),
                usize::from(Self::core_external_irq_entry_registered()),
                event.call_index
            );
            pr_info!(
                "axvisor_adapter: external_irq path enter vector={} matched={} pending_depth={} registration_calls={} slot_claimed={} core_entry_registered={} call_index={}\n",
                vector,
                external_matched,
                IRQ_EXTERNAL_LAST_PENDING_DEPTH.load(Ordering::Acquire),
                IRQ_EXTERNAL_REGISTRATION_CALLS.load(Ordering::Acquire),
                IRQ_EXTERNAL_HANDLER_SLOT_CLAIMED.load(Ordering::Acquire),
                usize::from(Self::core_external_irq_entry_registered()),
                event.call_index
            );
        }
        if external_matched {
            let cpu_id = LinuxHostAdapter::current_cpu_id();

            /*
             * Linux owns the physical PLIC claim/complete path. The
             * passthrough request_irq handler injects guest vPLIC state; the
             * S-ext VM-exit path only reports "handled" so the vCPU loop
             * latches live virtual interrupt state before re-entering.
             */
            if log_idx <= 16 || log_idx.is_power_of_two() {
                pr_emerg!(
                    "axvisor_adapter: external_irq latch_only vector={} cpu_id={} call_index={} linux_irq_core=1\n",
                    vector,
                    cpu_id,
                    event.call_index
                );
                pr_info!(
                    "axvisor_adapter: external_irq latch_only vector={} cpu_id={} call_index={} linux_irq_core=1\n",
                    vector,
                    cpu_id,
                    event.call_index
                );
            }
            IRQ_EXTERNAL_LAST_DRAINED_COUNT.store(0, Ordering::Release);
            IRQ_LAST_EXTERNAL_PATH_CONSUMED.store(1, Ordering::Release);
            return true;
        }
        if let Some(processor) = LinuxRuntimeAdapter::irq_event_processor() {
            let consumed = processor(event);
            IRQ_LAST_EXTERNAL_PATH_CONSUMED.store(usize::from(consumed), Ordering::Release);
            if consumed {
                return true;
            }
        }
        IRQ_EXTERNAL_LAST_DRAINED_COUNT.store(0, Ordering::Release);
        IRQ_LAST_EXTERNAL_PATH_CONSUMED.store(0, Ordering::Release);
        false
    }

    fn handle_local_irq_path(vector: usize) -> bool {
        if vector >= MAX_IRQ_VECTORS {
            IRQ_LAST_LOCAL_PATH_HIT.store(0, Ordering::Release);
            return false;
        }

        let raw = IRQ_HANDLERS[vector].load(Ordering::Acquire);
        if raw == 0 {
            IRQ_LAST_LOCAL_PATH_HIT.store(0, Ordering::Release);
            return false;
        }

        // SAFETY: `raw` is written only by `register_irq_handler`, which stores a valid function
        // pointer cast to `usize`.
        let handler: LinuxIrqHandler = unsafe { core::mem::transmute(raw) };
        handler(vector);
        IRQ_LAST_LOCAL_PATH_HIT.store(1, Ordering::Release);
        true
    }

    fn is_external_irq_vector(vector: usize) -> bool {
        arch::is_supervisor_external_vector(vector)
    }

    fn push_external_pending(event: ExternalIrqEvent) -> Result {
        let mut pending = EXTERNAL_IRQ_PENDING.lock();
        pending.push(event, GFP_ATOMIC)?;
        IRQ_EXTERNAL_PENDING_PUSHES.fetch_add(1, Ordering::AcqRel);
        IRQ_EXTERNAL_LAST_PENDING_DEPTH.store(pending.len(), Ordering::Release);
        if let Some(record) = EXTERNAL_IRQ_REGISTRATION.lock().as_mut() {
            if record.vector == event.vector {
                record.pending_pushes += 1;
                record.last_event_cpu_id = event.cpu_id;
                record.last_event_call_index = event.call_index;
            }
        }
        Ok(())
    }

    fn drain_external_pending() -> KVVec<ExternalIrqEvent> {
        IRQ_EXTERNAL_DRAIN_CALLS.fetch_add(1, Ordering::AcqRel);
        let mut pending = EXTERNAL_IRQ_PENDING.lock();
        let drained_count = pending.len();
        let mut drained = KVVec::new();
        for event in pending.iter().copied() {
            let _ = drained.push(event, GFP_ATOMIC);
        }
        pending.clear();
        IRQ_EXTERNAL_LAST_PENDING_DEPTH.store(0, Ordering::Release);
        if let Some(last) = drained.last() {
            IRQ_EXTERNAL_LAST_DRAIN_EVENT_CPU_ID.store(last.cpu_id, Ordering::Release);
            IRQ_EXTERNAL_LAST_DRAIN_EVENT_CALL_INDEX.store(last.call_index, Ordering::Release);
        } else {
            IRQ_EXTERNAL_LAST_DRAIN_EVENT_CPU_ID.store(usize::MAX, Ordering::Release);
            IRQ_EXTERNAL_LAST_DRAIN_EVENT_CALL_INDEX.store(0, Ordering::Release);
        }
        if let Some(record) = EXTERNAL_IRQ_REGISTRATION.lock().as_mut() {
            record.drain_calls += 1;
            record.last_drained_count = drained_count;
        }
        drained
    }

    fn install_external_registration(vector: usize) -> bool {
        let mut registration = EXTERNAL_IRQ_REGISTRATION.lock();
        if registration.is_some() {
            pr_info!(
                "axvisor_adapter: external_irq registration already installed vector={} existing_vector={}\n",
                vector,
                registration.as_ref().map(|r| r.vector).unwrap_or(usize::MAX)
            );
            return false;
        }

        IRQ_EXTERNAL_REGISTRATION_CALLS.fetch_add(1, Ordering::AcqRel);
        *registration = Some(ExternalIrqRegistration {
            vector,
            pending_pushes: 0,
            drain_calls: 0,
            last_drained_count: 0,
            last_event_cpu_id: usize::MAX,
            last_event_call_index: 0,
        });
        pr_info!(
            "axvisor_adapter: external_irq registration installed vector={} registration_calls={}\n",
            vector,
            IRQ_EXTERNAL_REGISTRATION_CALLS.load(Ordering::Acquire)
        );
        true
    }

    fn record_registration_attempt(
        vector: usize,
        is_external: bool,
        arch_ok: bool,
        local_installed: bool,
        result: bool,
    ) {
        IRQ_LAST_REGISTER_VECTOR.store(vector, Ordering::Release);
        IRQ_LAST_REGISTER_EXTERNAL.store(usize::from(is_external), Ordering::Release);
        IRQ_LAST_REGISTER_ARCH_OK.store(usize::from(arch_ok), Ordering::Release);
        IRQ_LAST_REGISTER_LOCAL_INSTALLED.store(
            usize::from(local_installed),
            Ordering::Release,
        );
        IRQ_LAST_REGISTER_RESULT.store(usize::from(result), Ordering::Release);
        if result {
            if is_external {
                IRQ_EXTERNAL_HANDLER_SLOT_CLAIMED.store(1, Ordering::Release);
            } else if local_installed {
                IRQ_LOCAL_HANDLER_INSTALLED_COUNT.fetch_add(1, Ordering::AcqRel);
            }
        }
        if is_external {
            pr_info!(
                "axvisor_adapter: irq_register external vector={} arch_ok={} result={} slot_claimed={} registration_calls={}\n",
                vector,
                arch_ok,
                result,
                IRQ_EXTERNAL_HANDLER_SLOT_CLAIMED.load(Ordering::Acquire),
                IRQ_EXTERNAL_REGISTRATION_CALLS.load(Ordering::Acquire)
            );
        }
    }

    // 21. IrqIf::handle_irq
    fn handle_irq(vector: usize) -> bool {
        let log_idx = IRQ_HANDLE_LOG_COUNT.fetch_add(1, Ordering::AcqRel) + 1;
        if log_idx <= 16 || log_idx.is_power_of_two() {
            pr_info!(
                "axvisor_adapter: irq_handle enter vector={} is_external={} call_index={}\n",
                vector,
                Self::is_external_irq_vector(vector),
                log_idx
            );
        }
        if Self::handle_external_irq_path(vector) {
            IRQ_LAST_FINAL_RESULT.store(1, Ordering::Release);
            return true;
        }

        let handled = Self::handle_local_irq_path(vector);
        IRQ_LAST_FINAL_RESULT.store(usize::from(handled), Ordering::Release);
        handled
    }

    // 22. IrqIf::register_irq_handler
    fn register_irq_handler(vector: usize, handler: LinuxIrqHandler) -> bool {
        let is_external = Self::is_external_irq_vector(vector);
        if !is_external && vector >= MAX_IRQ_VECTORS {
            Self::record_registration_attempt(vector, false, false, false, false);
            return false;
        }

        if !arch::register_irq_vector(vector) {
            Self::record_registration_attempt(vector, is_external, false, false, false);
            return false;
        }

        if is_external {
            let _ = handler;
            if !Self::install_external_registration(vector) {
                Self::record_registration_attempt(vector, true, true, false, false);
                return false;
            }
            Self::record_registration_attempt(vector, true, true, false, true);
            return true;
        }

        let raw = handler as usize;
        let installed = IRQ_HANDLERS[vector]
            .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        Self::record_registration_attempt(vector, false, true, installed, installed);
        installed
    }
}

impl LinuxMemoryAdapter {
    // 23. MemoryIf::alloc_frame
    fn alloc_frame() -> Option<u64> {
        let paddr = unsafe { axvisor_adapter_alloc_frame() };
        (paddr != 0).then_some(paddr)
    }

    // 25. MemoryIf::dealloc_frame
    fn dealloc_frame(paddr: u64) -> bool {
        unsafe { axvisor_adapter_dealloc_frame(paddr) }
    }

    // 27. MemoryIf::phys_to_virt
    fn phys_to_virt(paddr: u64) -> u64 {
        unsafe { axvisor_adapter_phys_to_virt(paddr) }
    }

    // 28. MemoryIf::virt_to_phys
    fn virt_to_phys(vaddr: u64) -> u64 {
        unsafe { axvisor_adapter_virt_to_phys(vaddr) }
    }
}

struct LinuxPassthroughRegistry;

impl LinuxPassthroughRegistry {
    fn register_device(vm_id: usize, base_hpa: u64, length: u64, irq_id: usize) -> bool {
        if base_hpa == 0 || length == 0 {
            return false;
        }

        for idx in 0..MAX_PASSTHROUGH_DEVICES {
            if PASSTHROUGH_DEVICE_BASE_HPA[idx].load(Ordering::Acquire) == base_hpa {
                PASSTHROUGH_DEVICE_LENGTH[idx].store(length, Ordering::Release);
                PASSTHROUGH_DEVICE_IRQ_ID[idx].store(irq_id, Ordering::Release);
                PASSTHROUGH_DEVICE_VM_ID[idx].store(vm_id, Ordering::Release);
                pr_info!(
                    "axvisor_adapter: passthrough device update idx={} vm_id={} base_hpa=0x{:x} length=0x{:x} irq_id={}\n",
                    idx,
                    vm_id,
                    base_hpa,
                    length,
                    irq_id
                );
                return true;
            }
        }

        let idx = PASSTHROUGH_DEVICE_COUNT.fetch_add(1, Ordering::AcqRel);
        if idx >= MAX_PASSTHROUGH_DEVICES {
            PASSTHROUGH_DEVICE_COUNT.store(MAX_PASSTHROUGH_DEVICES, Ordering::Release);
            pr_warn!(
                "axvisor_adapter: passthrough registry full, drop base_hpa=0x{:x} length=0x{:x} irq_id={}\n",
                base_hpa,
                length,
                irq_id
            );
            return false;
        }

        PASSTHROUGH_DEVICE_LENGTH[idx].store(length, Ordering::Release);
        PASSTHROUGH_DEVICE_IRQ_ID[idx].store(irq_id, Ordering::Release);
        PASSTHROUGH_DEVICE_VM_ID[idx].store(vm_id, Ordering::Release);
        PASSTHROUGH_DEVICE_BASE_HPA[idx].store(base_hpa, Ordering::Release);
        pr_info!(
            "axvisor_adapter: passthrough device register idx={} vm_id={} base_hpa=0x{:x} length=0x{:x} irq_id={}\n",
            idx,
            vm_id,
            base_hpa,
            length,
            irq_id
        );
        true
    }

    fn len() -> usize {
        core::cmp::min(
            PASSTHROUGH_DEVICE_COUNT.load(Ordering::Acquire),
            MAX_PASSTHROUGH_DEVICES,
        )
    }

    fn base_hpa(idx: usize) -> u64 {
        if idx >= Self::len() {
            0
        } else {
            PASSTHROUGH_DEVICE_BASE_HPA[idx].load(Ordering::Acquire)
        }
    }

    fn irq_id(idx: usize) -> usize {
        if idx >= Self::len() {
            0
        } else {
            PASSTHROUGH_DEVICE_IRQ_ID[idx].load(Ordering::Acquire)
        }
    }

    fn vm_id(idx: usize) -> usize {
        if idx >= Self::len() {
            usize::MAX
        } else {
            PASSTHROUGH_DEVICE_VM_ID[idx].load(Ordering::Acquire)
        }
    }

    fn length(idx: usize) -> u64 {
        if idx >= Self::len() {
            0
        } else {
            PASSTHROUGH_DEVICE_LENGTH[idx].load(Ordering::Acquire)
        }
    }

    fn contains_irq(irq_id: usize) -> bool {
        if irq_id == 0 {
            return false;
        }

        for idx in 0..Self::len() {
            if PASSTHROUGH_DEVICE_IRQ_ID[idx].load(Ordering::Acquire) == irq_id {
                return true;
            }
        }
        false
    }

    fn vm_id_for_irq(irq_id: usize) -> Option<usize> {
        if irq_id == 0 {
            return None;
        }

        for idx in 0..Self::len() {
            if PASSTHROUGH_DEVICE_IRQ_ID[idx].load(Ordering::Acquire) == irq_id {
                let vm_id = PASSTHROUGH_DEVICE_VM_ID[idx].load(Ordering::Acquire);
                if vm_id != usize::MAX {
                    return Some(vm_id);
                }
            }
        }
        None
    }
}

unsafe extern "C" fn axvisor_kthread_main(data: *mut c_void) -> c_int {
    // SAFETY: `data` comes from `KBox::into_raw` for `AxvisorTaskStart`.
    let mut start = unsafe { KBox::from_raw(data.cast::<AxvisorTaskStart>()) };
    pr_info!(
        "axvisor_adapter: kthread main entered pid={} registered={} cancelled={}\n",
        current!().pid(),
        start.registered.load(Ordering::Acquire),
        start.cancelled.load(Ordering::Acquire),
    );
    while !start.registered.load(Ordering::Acquire) {
        TaskHandle::yield_now();
    }
    pr_info!(
        "axvisor_adapter: kthread main start-ready pid={} cancelled={}\n",
        current!().pid(),
        start.cancelled.load(Ordering::Acquire),
    );
    if start.cancelled.load(Ordering::Acquire) {
        start
            .state
            .finish(Error::from_errno(-(bindings::ECANCELED as i32)).to_errno());
        return 0;
    }
    if let Some(entry) = start.entry.take() {
        pr_info!(
            "axvisor_adapter: kthread main invoking entry pid={} affinity=0x{:x} current_cpu={}\n",
            current!().pid(),
            start.cpu_affinity_mask,
            LinuxHostAdapter::current_cpu_id()
        );
        entry.call();
    }
    pr_info!("axvisor_adapter: kthread main entry returned pid={}\n", current!().pid(),);
    start.state.finish(0);
    0
}

fn cpuset_to_cpumask(cpu_affinity: AxvisorCpuSet) -> Result<CpumaskVar> {
    let mut mask = CpumaskVar::new_zero(GFP_KERNEL)?;

    if cpu_affinity.mask == 0 {
        return Err(EINVAL);
    }

    let nr = kernel::cpu::nr_cpu_ids() as usize;
    for bit in 0..usize::BITS as usize {
        if (cpu_affinity.mask & (1usize << bit)) == 0 {
            continue;
        }
        if bit >= nr {
            return Err(EINVAL);
        }
        let cpu = CpuId::from_u32(bit as u32).ok_or(EINVAL)?;
        mask.set(cpu);
    }

    if mask.empty() {
        return Err(EINVAL);
    }

    Ok(mask)
}

fn single_cpu_from_cpuset(cpu_affinity: AxvisorCpuSet) -> Option<u32> {
    let mask = cpu_affinity.mask;
    if mask != 0 && mask.count_ones() == 1 {
        Some(mask.trailing_zeros())
    } else {
        None
    }
}

fn register_task_handle(handle: &TaskHandle) -> Result {
    let mut registry = TASK_REGISTRY.lock();
    if registry.entries.iter().any(|entry| entry.pid == handle.pid()) {
        return Err(EEXIST);
    }
    registry.entries.push(
        TaskRegistryEntry {
            pid: handle.pid(),
            handle: handle.record.clone(),
        },
        GFP_KERNEL,
    )?;
    Ok(())
}

fn unregister_task_handle(pid: kernel::task::Pid) {
    TASK_REGISTRY.lock().entries.retain(|entry| entry.pid != pid);
}

fn lookup_current_task_handle() -> Option<Arc<TaskHandleRecord>> {
    let pid = current!().pid();
    TASK_REGISTRY
        .lock()
        .entries
        .iter()
        .find(|entry| entry.pid == pid)
        .map(|entry| entry.handle.clone())
}

fn lookup_wait_queue(id: usize) -> Option<Arc<LinuxWaitQueueRecord>> {
    WAIT_QUEUE_REGISTRY
        .lock()
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.queue.clone())
}

fn remove_wait_queue(id: usize) -> Option<Arc<LinuxWaitQueueRecord>> {
    let mut registry = WAIT_QUEUE_REGISTRY.lock();
    let index = registry.entries.iter().position(|entry| entry.id == id)?;
    Some(registry.entries.remove(index).ok()?.queue)
}

impl KernelTaskRuntime for LinuxKernelTaskRuntime {
    // 1. KernelTaskRuntime::spawn_task
    fn spawn_task(
        &self,
        entry: KBox<dyn TaskEntry>,
        cpu_affinity: AxvisorCpuSet,
    ) -> Result<TaskHandle> {
        let registered = Arc::new(AtomicBool::new(false), GFP_KERNEL)?;
        let cancelled = Arc::new(AtomicBool::new(false), GFP_KERNEL)?;
        let start = KBox::new(
            AxvisorTaskStart {
                entry: Some(entry),
                state: TaskState::new()?,
                registered: registered.clone(),
                cancelled: cancelled.clone(),
                cpu_affinity_mask: cpu_affinity.mask,
            },
            GFP_KERNEL,
        )?;

        let state = start.state.clone();

        let raw_start = KBox::into_raw(start).cast::<c_void>();
        pr_info!(
            "axvisor_adapter: spawn_task before_kthread_create affinity=0x{:x}\n",
            cpu_affinity.mask
        );
        let task_ptr = from_err_ptr(unsafe {
            axvisor_adapter_kthread_create(axvisor_kthread_main, raw_start, THREAD_NAME.as_char_ptr())
        });

        let task_ptr = match task_ptr {
            Ok(ptr) => ptr,
            Err(err) => {
                // SAFETY: `raw_start` comes from `KBox::into_raw` above and thread creation failed.
                unsafe { drop(KBox::from_raw(raw_start.cast::<AxvisorTaskStart>())) };
                return Err(err);
            }
        };
        pr_info!("axvisor_adapter: spawn_task after_kthread_create task={:p}\n", task_ptr);

        let cpumask = match cpuset_to_cpumask(cpu_affinity) {
            Ok(mask) => mask,
            Err(err) => {
                unsafe { axvisor_adapter_kthread_stop(task_ptr) };
                return Err(err);
            }
        };
        if let Some(cpu) = single_cpu_from_cpuset(cpu_affinity) {
            pr_info!("axvisor_adapter: spawn_task before_kthread_bind cpu={}\n", cpu);
            unsafe { axvisor_adapter_kthread_bind(task_ptr, cpu as c_uint) };
            pr_info!("axvisor_adapter: spawn_task after_kthread_bind cpu={}\n", cpu);
        } else {
            pr_info!("axvisor_adapter: spawn_task before_set_cpus_allowed\n");
            let ret = unsafe { axvisor_adapter_set_cpus_allowed_ptr(task_ptr, cpumask.as_raw()) };
            if ret < 0 {
                unsafe { axvisor_adapter_kthread_stop(task_ptr) };
                return Err(Error::from_errno(ret));
            }
            pr_info!("axvisor_adapter: spawn_task after_set_cpus_allowed ret={}\n", ret);
        }

        unsafe { axvisor_adapter_wake_up_process(task_ptr) };
        pr_info!("axvisor_adapter: spawn_task after_wake_up_process\n");

        let pid = unsafe {
            // SAFETY: `task_ptr` is a live task with an owned reference from `kthread_create`.
            (*task_ptr).pid
        };
        let record = TaskHandleRecord::new(pid, state.clone())?;
        let handle = TaskHandle {
            state,
            record,
        };

        if let Err(err) = register_task_handle(&handle) {
            cancelled.store(true, Ordering::Release);
            registered.store(true, Ordering::Release);
            return Err(err);
        }
        registered.store(true, Ordering::Release);
        pr_info!("axvisor_adapter: spawn_task registered pid={}\n", pid);
        Ok(handle)
    }
}

impl LinuxRuntimeAdapter {
    // 2. install_kernel_task_runtime
    fn install_kernel_task_runtime() -> bool {
        KERNEL_TASK_RUNTIME_INSTALLED
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn is_kernel_task_runtime_installed() -> bool {
        KERNEL_TASK_RUNTIME_INSTALLED.load(Ordering::Acquire) != 0
    }

    // 3. run
    fn run() {
        let ctx = AxvisorRuntimeStartContext {
            host_cpu_num: LinuxHostAdapter::get_host_cpu_num(),
            current_cpu_id: LinuxHostAdapter::current_cpu_id(),
            kernel_task_runtime_installed: Self::is_kernel_task_runtime_installed(),
            run_call_index: RUNTIME_RUN_CALLS.fetch_add(1, Ordering::AcqRel) + 1,
        };
        if let Some(processor) = Self::runtime_start_processor() {
            RUNTIME_LAST_PROCESSOR_PRESENT.store(1, Ordering::Release);
            RUNTIME_LAST_PROCESSOR_INVOKED.store(1, Ordering::Release);
            RUNTIME_LAST_FALLBACK_USED.store(0, Ordering::Release);
            processor(ctx);
            return;
        }

        RUNTIME_LAST_PROCESSOR_PRESENT.store(0, Ordering::Release);
        RUNTIME_LAST_PROCESSOR_INVOKED.store(0, Ordering::Release);
        RUNTIME_LAST_FALLBACK_USED.store(1, Ordering::Release);
        AxvisorCoreGlue::record_runtime_start(ctx);
        RUNTIME_LAST_START_ENTERED.store(1, Ordering::Release);
        RUNTIME_LAST_START_RETURNED.store(1, Ordering::Release);
        pr_info!("axvisor_adapter: runtime run hook entered\n");
    }
}

impl TaskHandle {
    // 18. TaskIf::join_task
    fn join_task(self) -> Result<c_int> {
        self.join()
    }

    // 19. TaskIf::current_task
    fn current_task() -> Result<Self> {
        if let Some(record) = lookup_current_task_handle() {
            return Ok(Self {
                state: record.state.clone(),
                record,
            });
        }

        Self::current()
    }

    // 20. TaskIf::yield_now
    fn task_yield_now() {
        Self::yield_now();
    }
}

impl LinuxKernelTaskRuntime {
    // 17. TaskIf::spawn_task_raw
    fn spawn_task_raw(
        &self,
        entry: KBox<dyn TaskEntry>,
        cpu_affinity: AxvisorCpuSet,
    ) -> Result<TaskHandle> {
        self.spawn_task(entry, cpu_affinity)
    }
}

fn lookup_task_record_by_raw(handle_raw: usize) -> Option<Arc<TaskHandleRecord>> {
    let pid = handle_raw as kernel::task::Pid;
    TASK_REGISTRY
        .lock()
        .entries
        .iter()
        .find(|entry| entry.pid == pid)
        .map(|entry| entry.handle.clone())
}

fn full_cpu_mask() -> usize {
    let nr = kernel::cpu::nr_cpu_ids() as usize;
    if nr >= usize::BITS as usize {
        usize::MAX
    } else if nr == 0 {
        0
    } else {
        (1usize << nr) - 1
    }
}

fn host_emerg_flush_line_bytes(line: &[u8]) {
    match core::str::from_utf8(line) {
        Ok(msg) => {
            let noisy = msg.starts_with("shell::read byte")
                || msg.starts_with("vcpus::exit_reason nothing count=")
                || msg.starts_with("riscv_vcpu::nothing count=");
            let is_dbcn_preview = msg.starts_with("dbcn_write[");
            let is_dbcn_raw_preview = msg.starts_with("dbcn_write_raw[");
            let keep_dbcn_preview = is_dbcn_preview
                && (msg.contains("Hello, world!")
                    || msg.contains("preview=\"\\n       d8888")
                    || msg.contains("preview=\"arch = ")
                    || msg.contains("preview=\"8P\\\"   d88P\\\""));
            if !noisy && (is_dbcn_raw_preview || !is_dbcn_preview || keep_dbcn_preview) {
                pr_emerg!("axvisor_bridge: {}\n", msg);
            }
        }
        Err(_) => pr_emerg!(
            "axvisor_bridge: <non-utf8 payload len={}>\n",
            line.len()
        ),
    }
}

fn host_emerg_write_buffered(bytes: &[u8]) {
    let mut buffer = HOST_EMERG_LINE_BUFFER.lock();

    for &byte in bytes {
        if byte == b'\n' {
            host_emerg_flush_line_bytes(buffer.as_slice());
            buffer.clear();
            continue;
        }

        if buffer.len() < HOST_EMERG_LINE_BUFFER_CAPACITY {
            let _ = buffer.push(byte, GFP_ATOMIC);
        }

        if buffer.len() >= HOST_EMERG_FORCE_FLUSH_THRESHOLD {
            host_emerg_flush_line_bytes(buffer.as_slice());
            buffer.clear();
        }
    }

    if !buffer.is_empty() {
        host_emerg_flush_line_bytes(buffer.as_slice());
        buffer.clear();
    }
}

fn adapter_emerg(msg: &str) {
    host_emerg_write_buffered(msg.as_bytes());
    host_emerg_write_buffered(b"\n");
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_host_get_cpu_num() -> usize {
    LinuxHostAdapter::get_host_cpu_num()
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_host_current_cpu_id() -> usize {
    LinuxHostAdapter::current_cpu_id()
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_host_init_percpu() {
    LinuxHostAdapter::init_percpu();
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_host_release_host_filesystems() -> c_int {
    LinuxHostAdapter::release_host_filesystems()
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_host_exit(exit_code: c_int) -> ! {
    LinuxHostAdapter::exit(exit_code)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_console_write_bytes(bytes: *const u8, len: usize) {
    if bytes.is_null() || len == 0 {
        return;
    }
    // SAFETY: caller guarantees `bytes..bytes+len` is readable for the call.
    let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
    LinuxConsoleAdapter::write_bytes(slice);
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_console_read_bytes(bytes: *mut u8, len: usize) -> usize {
    if bytes.is_null() || len == 0 {
        return 0;
    }
    CONSOLE_SHELL_READY.store(1, Ordering::Release);
    // SAFETY: caller guarantees `bytes..bytes+len` is writable for the call.
    let slice = unsafe { core::slice::from_raw_parts_mut(bytes, len) };
    LinuxConsoleAdapter::read_bytes(slice)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_console_shell_ready() -> bool {
    CONSOLE_SHELL_READY.load(Ordering::Acquire) != 0
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_console_enqueue_bytes(bytes: *const u8, len: usize) -> usize {
    if bytes.is_null() || len == 0 {
        return 0;
    }
    let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
    LinuxConsoleAdapter::enqueue_bytes(slice)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_guest_console_write_bytes(bytes: *const u8, len: usize) {
    if bytes.is_null() || len == 0 {
        return;
    }
    let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
    LinuxGuestConsoleAdapter::write_bytes(slice);
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_guest_console_read_bytes(bytes: *mut u8, len: usize) -> usize {
    if bytes.is_null() || len == 0 {
        return 0;
    }
    let slice = unsafe { core::slice::from_raw_parts_mut(bytes, len) };
    LinuxGuestConsoleAdapter::read_input_bytes(slice)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_guest_console_enqueue_bytes(bytes: *const u8, len: usize) -> usize {
    if bytes.is_null() || len == 0 {
        return 0;
    }
    let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
    LinuxGuestConsoleAdapter::enqueue_input_bytes(slice)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_guest_console_drain_bytes(bytes: *mut u8, len: usize) -> usize {
    if bytes.is_null() || len == 0 {
        return 0;
    }
    let slice = unsafe { core::slice::from_raw_parts_mut(bytes, len) };
    LinuxGuestConsoleAdapter::drain_output_bytes(slice)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_host_emerg_write_bytes(bytes: *const u8, len: usize) {
    if bytes.is_null() || len == 0 {
        return;
    }
    let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
    host_emerg_write_buffered(slice);
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_time_current_time_nanos() -> u64 {
    LinuxTimeAdapter::current_time_nanos()
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_time_set_oneshot_timer(deadline_nanos: u64) {
    LinuxTimeAdapter::set_oneshot_timer(deadline_nanos);
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_sync_create_wait_queue() -> usize {
    LinuxSyncAdapter::create_wait_queue().unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_sync_destroy_wait_queue(queue: usize) {
    LinuxSyncAdapter::destroy_wait_queue(queue);
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_sync_wait_queue_wait(queue: usize) {
    LinuxSyncAdapter::wait_queue_wait(queue);
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_sync_wait_queue_wait_until(
    queue: usize,
    condition_ctx: *mut c_void,
    condition_fn: unsafe extern "C" fn(*mut c_void) -> bool,
) {
    pr_debug!(
        "axvisor_adapter: ffi wait_queue_wait_until enter queue={} condition_ctx={:#x}\n",
        queue,
        condition_ctx as usize
    );
    let mut condition = || {
        // SAFETY: `condition_ctx` is owned by the caller for the duration of this call.
        unsafe { condition_fn(condition_ctx) }
    };
    LinuxSyncAdapter::wait_queue_wait_until(queue, &mut condition);
    pr_debug!(
        "axvisor_adapter: ffi wait_queue_wait_until leave queue={} condition_ctx={:#x}\n",
        queue,
        condition_ctx as usize
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_sync_wait_queue_wake_one(queue: usize) {
    LinuxSyncAdapter::wait_queue_wake_one(queue);
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_sync_wait_queue_wake_all(queue: usize) {
    LinuxSyncAdapter::wait_queue_wake_all(queue);
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_task_spawn_raw(
    _name_ptr: *const u8,
    _name_len: usize,
    _stack_size: usize,
    cpu_set_present: bool,
    cpu_set: usize,
    entry_ctx: *mut c_void,
    entry_fn: unsafe extern "C" fn(*mut c_void),
) -> usize {
    let affinity = AxvisorCpuSet::from_mask(if cpu_set_present { cpu_set } else { full_cpu_mask() });
    let entry_ctx_raw = entry_ctx as usize;
    let entry = match KBox::new(
        move || {
            // SAFETY: the target-side bridge passes a valid trampoline/context pair.
            unsafe { entry_fn(entry_ctx_raw as *mut c_void) };
        },
        GFP_KERNEL,
    ) {
        Ok(entry) => entry as KBox<dyn TaskEntry>,
        Err(_) => return 0,
    };

    let runtime = LinuxKernelTaskRuntime;
    match runtime.spawn_task_raw(entry, affinity) {
        Ok(task) => task.pid() as usize,
        Err(_) => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_task_join(handle_raw: usize) {
    let Some(record) = lookup_task_record_by_raw(handle_raw) else {
        return;
    };
    unregister_task_handle(record.pid);
    let _ = record.state.wait();
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_task_current() -> usize {
    match TaskHandle::current_task() {
        Ok(task) => task.pid() as usize,
        Err(_) => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_task_yield_now() {
    TaskHandle::task_yield_now();
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_irq_handle(vector: usize) -> bool {
    let log_idx = IRQ_HANDLE_LOG_COUNT.load(Ordering::Acquire) + 1;
    if log_idx <= 16 || log_idx.is_power_of_two() {
        pr_info!(
            "axvisor_adapter: ffi irq_handle enter vector={} next_call_index={}\n",
            vector,
            log_idx
        );
    }
    LinuxIrqAdapter::handle_irq(vector)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_irq_register(
    vector: usize,
    handler_ctx: *mut c_void,
    handler_fn: unsafe extern "C" fn(usize, *mut c_void),
) -> bool {
    fn bridge_handler_dispatch(vector: usize) {
        let (raw_fn, raw_ctx) = if LinuxIrqAdapter::is_external_irq_vector(vector) {
            (
                IRQ_BRIDGE_EXTERNAL_HANDLER_FN.load(Ordering::Acquire),
                IRQ_BRIDGE_EXTERNAL_HANDLER_CTX.load(Ordering::Acquire),
            )
        } else {
            (
                IRQ_BRIDGE_HANDLER_FN[vector].load(Ordering::Acquire),
                IRQ_BRIDGE_HANDLER_CTX[vector].load(Ordering::Acquire),
            )
        };
        if raw_fn == 0 {
            return;
        }
        // SAFETY: registration stores a valid function pointer and opaque context.
        let handler_fn: unsafe extern "C" fn(usize, *mut c_void) =
            unsafe { core::mem::transmute(raw_fn) };
        // SAFETY: opaque context is owned by the caller and lives for the module lifetime.
        unsafe { handler_fn(vector, raw_ctx as *mut c_void) };
    }

    if vector >= MAX_IRQ_VECTORS && !LinuxIrqAdapter::is_external_irq_vector(vector) {
        return false;
    }
    if LinuxIrqAdapter::is_external_irq_vector(vector) {
        IRQ_BRIDGE_EXTERNAL_HANDLER_CTX.store(handler_ctx as usize, Ordering::Release);
        IRQ_BRIDGE_EXTERNAL_HANDLER_FN.store(handler_fn as usize, Ordering::Release);
    } else {
        IRQ_BRIDGE_HANDLER_CTX[vector].store(handler_ctx as usize, Ordering::Release);
        IRQ_BRIDGE_HANDLER_FN[vector].store(handler_fn as usize, Ordering::Release);
    }
    LinuxIrqAdapter::register_irq_handler(vector, bridge_handler_dispatch)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_memory_alloc_frame() -> u64 {
    LinuxMemoryAdapter::alloc_frame().unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_memory_dealloc_frame(paddr: u64) {
    let _ = LinuxMemoryAdapter::dealloc_frame(paddr);
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_memory_phys_to_virt(paddr: u64) -> u64 {
    LinuxMemoryAdapter::phys_to_virt(paddr)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_memory_virt_to_phys(vaddr: u64) -> u64 {
    LinuxMemoryAdapter::virt_to_phys(vaddr)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_memory_register_guest_ram(paddr: u64, size: u64) -> bool {
    unsafe { axvisor_adapter_register_guest_ram(paddr, size) }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_memory_mmio_read32(paddr: u64) -> u32 {
    unsafe { axvisor_adapter_mmio_read32(paddr) }
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_memory_mmio_write32(paddr: u64, value: u32) {
    unsafe { axvisor_adapter_mmio_write32(paddr, value) };
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_riscv_plic_complete_passthrough_irq(irq_id: usize) {
    unsafe { axvisor_adapter_riscv_plic_complete_passthrough_irq(irq_id as c_uint) };
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_host_register_passthrough_device(
    vm_id: usize,
    base_hpa: u64,
    length: u64,
    irq_id: usize,
) -> bool {
    LinuxPassthroughRegistry::register_device(vm_id, base_hpa, length, irq_id)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_passthrough_device_count() -> usize {
    LinuxPassthroughRegistry::len()
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_passthrough_device_base_hpa(index: usize) -> u64 {
    LinuxPassthroughRegistry::base_hpa(index)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_passthrough_device_length(index: usize) -> u64 {
    LinuxPassthroughRegistry::length(index)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_passthrough_device_irq_id(index: usize) -> usize {
    LinuxPassthroughRegistry::irq_id(index)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_passthrough_device_vm_id(index: usize) -> usize {
    LinuxPassthroughRegistry::vm_id(index)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_passthrough_irq_vm_id(irq_id: usize) -> usize {
    LinuxPassthroughRegistry::vm_id_for_irq(irq_id).unwrap_or(usize::MAX)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_passthrough_irq_registered(irq_id: usize) -> bool {
    LinuxPassthroughRegistry::contains_irq(irq_id)
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_passthrough_irq_mark_pending(irq_id: usize) -> bool {
    let Some(vm_id) = LinuxPassthroughRegistry::vm_id_for_irq(irq_id) else {
        pr_info!(
            "axvisor_adapter: passthrough irq pending skipped unregistered irq_id={}\n",
            irq_id
        );
        return false;
    };

    #[cfg(target_arch = "x86_64")]
    let pending = vendor::axvisor_core::vmm::mark_x86_gsi_pending(vm_id, irq_id);

    #[cfg(not(target_arch = "x86_64"))]
    let pending = false;

    pr_info!(
        "axvisor_adapter: passthrough irq pending vm_id={} irq_id={} pending={} pending_depth={} drained_count={} last_irq_vector={} last_irq_external_matched={} last_irq_consumed={}\n",
        vm_id,
        irq_id,
        pending,
        IRQ_EXTERNAL_LAST_PENDING_DEPTH.load(Ordering::Acquire),
        IRQ_EXTERNAL_LAST_DRAINED_COUNT.load(Ordering::Acquire),
        GLUE_LAST_IRQ_VECTOR.load(Ordering::Acquire),
        GLUE_LAST_IRQ_EXTERNAL_MATCHED.load(Ordering::Acquire),
        GLUE_LAST_IRQ_CONSUMED.load(Ordering::Acquire)
    );
    pending
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_passthrough_irq_inject(irq_id: usize) -> bool {
    let Some(vm_id) = LinuxPassthroughRegistry::vm_id_for_irq(irq_id) else {
        pr_info!(
            "axvisor_adapter: passthrough irq inject skipped unregistered irq_id={}\n",
            irq_id
        );
        return false;
    };

    #[cfg(target_arch = "riscv64")]
    let injected = vendor::axvisor_core::arch::riscv64::inject_interrupt(vm_id, irq_id);

    #[cfg(target_arch = "x86_64")]
    let injected = vendor::axvisor_core::vmm::inject_x86_gsi(vm_id, irq_id);

    #[cfg(not(any(target_arch = "riscv64", target_arch = "x86_64")))]
    let injected = false;

    pr_info!(
        "axvisor_adapter: passthrough irq inject vm_id={} irq_id={} injected={} pending_depth={} drained_count={} last_irq_vector={} last_irq_external_matched={} last_irq_consumed={}\n",
        vm_id,
        irq_id,
        injected,
        IRQ_EXTERNAL_LAST_PENDING_DEPTH.load(Ordering::Acquire),
        IRQ_EXTERNAL_LAST_DRAINED_COUNT.load(Ordering::Acquire),
        GLUE_LAST_IRQ_VECTOR.load(Ordering::Acquire),
        GLUE_LAST_IRQ_EXTERNAL_MATCHED.load(Ordering::Acquire),
        GLUE_LAST_IRQ_CONSUMED.load(Ordering::Acquire)
    );
    injected
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_x86_passthrough_irq_unmask(irq_id: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        return unsafe { axvisor_adapter_x86_passthrough_irq_unmask(irq_id as c_uint) };
    }

    #[allow(unreachable_code)]
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_x86_passthrough_irq_handle_vector(vector: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        return unsafe { axvisor_adapter_x86_passthrough_irq_handle_vector(vector as c_uint) };
    }

    #[allow(unreachable_code)]
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_x86_passthrough_irq_poll(irq_id: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        return unsafe { axvisor_adapter_x86_passthrough_irq_poll(irq_id as c_uint) };
    }

    #[allow(unreachable_code)]
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_arch_host_fdt_vaddr() -> u64 {
    arch::host_fdt_vaddr()
}

#[unsafe(no_mangle)]
pub extern "C" fn axvisor_linux_arch_host_fdt_size() -> usize {
    arch::host_fdt_size()
}

struct AxvisorAdapterModule {
    host_task: Option<TaskHandle>,
}

impl kernel::Module for AxvisorAdapterModule {
    fn init(_module: &'static ThisModule) -> Result<Self> {
        // SAFETY: Called exactly once during module initialization.
        unsafe { TASK_REGISTRY.init() };
        // SAFETY: Called exactly once during module initialization.
        unsafe { WAIT_QUEUE_REGISTRY.init() };
        // SAFETY: Called exactly once during module initialization.
        unsafe { CONSOLE_INPUT_BUFFER.init() };
        *CONSOLE_INPUT_BUFFER.lock() =
            KVVec::with_capacity(CONSOLE_INPUT_BUFFER_CAPACITY, GFP_KERNEL)?;
        // SAFETY: Called exactly once during module initialization.
        unsafe { CONSOLE_INPUT_STATE.init() };
        // SAFETY: Called exactly once during module initialization.
        unsafe { HOST_EMERG_LINE_BUFFER.init() };
        // SAFETY: Called exactly once during module initialization.
        unsafe { GUEST_CONSOLE_INPUT_BUFFER.init() };
        *GUEST_CONSOLE_INPUT_BUFFER.lock() =
            KVVec::with_capacity(GUEST_CONSOLE_INPUT_BUFFER_CAPACITY, GFP_KERNEL)?;
        // SAFETY: Called exactly once during module initialization.
        unsafe { GUEST_CONSOLE_OUTPUT_BUFFER.init() };
        *GUEST_CONSOLE_OUTPUT_BUFFER.lock() =
            KVVec::with_capacity(GUEST_CONSOLE_OUTPUT_BUFFER_CAPACITY, GFP_KERNEL)?;
        let console_input_state = Arc::pin_init(
            pin_init::pin_init!(ConsoleInputState {
                pending <- new_mutex!(false),
                cv <- new_condvar!(),
            }),
            GFP_KERNEL,
        )?;
        *CONSOLE_INPUT_STATE.lock() = Some(console_input_state);
        if !unsafe { axvisor_adapter_console_input_install() } {
            return Err(ENOMEM);
        }
        let fdt_prepare_ret = unsafe { axvisor_adapter_host_fdt_prepare() };
        if fdt_prepare_ret < 0 {
            unsafe { axvisor_adapter_console_input_remove() };
            return Err(Error::from_errno(fdt_prepare_ret));
        }
        // SAFETY: Called exactly once during module initialization.
        unsafe { EXTERNAL_IRQ_PENDING.init() };
        // SAFETY: Called exactly once during module initialization.
        unsafe { EXTERNAL_IRQ_REGISTRATION.init() };
        arch::init_backend();
        init_axvisor_percpu_backing()?;
        arch::register_timer_bridge(linux_timer_bridge);
        let _ = AxvisorCoreGlue::install();

        #[cfg(target_arch = "x86_64")]
        {
            vendor::axvisor_core::vmm::register_x86_passthrough_gsi(X86_QEMU_BLK_GUEST_GSI);
            LinuxPassthroughRegistry::register_device(
                X86_QEMU_BLK_VM_ID,
                X86_QEMU_BLK_SYNTHETIC_BASE_HPA,
                1,
                X86_QEMU_BLK_GUEST_GSI,
            );
            let intx_ret = unsafe { axvisor_adapter_request_x86_qemu_blk_intx() };
            if intx_ret < 0 {
                pr_err!(
                    "axvisor_adapter: x86 qemu blk INTx setup failed ret={}\n",
                    intx_ret
                );
                unsafe { axvisor_adapter_host_fdt_release() };
                unsafe { axvisor_adapter_release_passthrough_irqs() };
                unsafe { axvisor_adapter_console_input_remove() };
                return Err(Error::from_errno(intx_ret));
            }
        }

        pr_info!("axvisor_adapter: init\n");
        let runtime_install_result = LinuxRuntimeAdapter::install_kernel_task_runtime();
        LinuxHostAdapter::init_percpu();
        pr_info!(
            "axvisor_adapter: runtime install result={} installed={} host_cpu_num={} percpu_ready={} last_percpu_cpu_id={} host_fdt_vaddr=0x{:x} host_fdt_size={} tsc_freq_mhz={} boot_registered={} timer_registered={} irq_registered={}\n",
            runtime_install_result,
            LinuxRuntimeAdapter::is_kernel_task_runtime_installed(),
            LinuxHostAdapter::get_host_cpu_num(),
            arch::percpu_ready(),
            arch::last_percpu_cpu_id(),
            arch::host_fdt_vaddr(),
            arch::host_fdt_size(),
            arch::host_tsc_frequency_mhz(),
            vendor::axvisor_core::boot::boot_entry_registered(),
            vendor::axvisor_core::vmm::timer::timer_entry_registered(),
            LinuxIrqAdapter::core_external_irq_entry_registered()
        );

        let host_task = LinuxKernelTaskRuntime.spawn_task_raw(
            KBox::new(
                || {
                    pr_info!("axvisor_adapter: host task entering runtime\n");
                    LinuxRuntimeAdapter::run();
                    pr_info!("axvisor_adapter: host task returned from runtime\n");
                },
                GFP_KERNEL,
            )? as KBox<dyn TaskEntry>,
            AxvisorCpuSet::from_mask(1),
        )?;

        pr_info!(
            "axvisor_adapter: host task spawned pid={}\n",
            host_task.pid()
        );

        Ok(Self {
            host_task: Some(host_task),
        })
    }
}

impl Drop for AxvisorAdapterModule {
    fn drop(&mut self) {
        unsafe { axvisor_adapter_console_input_remove() };
        unsafe { axvisor_adapter_host_fdt_release() };
        unsafe { axvisor_adapter_release_passthrough_irqs() };
        unsafe { axvisor_adapter_release_dynamic_mappings() };

        if let Some(task) = self.host_task.take() {
            match task.join_task() {
                Ok(ret) => pr_info!("axvisor_adapter: joined task ret={}\n", ret),
                Err(err) => pr_err!("axvisor_adapter: join failed: {:?}\n", err),
            }
        }

        pr_info!("axvisor_adapter: exit\n");
    }
}
