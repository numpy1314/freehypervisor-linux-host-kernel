// SPDX-License-Identifier: GPL-2.0

use core::convert::TryFrom;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kernel::{
    alloc::kvec::KVVec,
    cpu::CpuId,
    impl_has_hr_timer,
    prelude::*,
    sync::Arc,
    time::{
        Delta, Instant, Monotonic,
        hrtimer::{
            ArcHrTimerHandle, HrTimer, HrTimerCallback, HrTimerCallbackContext, HrTimerExpires,
            HrTimerPointer, HrTimerHandle, HrTimerRestart, RelativeMode,
        },
    },
};

use kernel::ffi::{c_uint, c_ulonglong};

unsafe extern "C" {
    fn axvisor_adapter_host_fdt_vaddr() -> c_ulonglong;
    fn axvisor_adapter_host_fdt_size() -> usize;
    fn axvisor_adapter_host_tsc_frequency_mhz() -> c_uint;
}

const NO_DEADLINE_NANOS: u64 = u64::MAX;
const MAX_ARCH_IRQ_VECTORS: usize = 256;
const RISCV_S_EXT_CAUSE: usize = 9;
const RISCV_S_EXT_VECTOR: usize = (1usize << (usize::BITS - 1)) + 9;

static RISCV_PERCPU_READY: AtomicBool = AtomicBool::new(false);
static RISCV_TIMER_DEADLINE_NANOS: AtomicU64 = AtomicU64::new(NO_DEADLINE_NANOS);
static RISCV_TIMER_FIRE_COUNT: AtomicU64 = AtomicU64::new(0);
static RISCV_TIMER_PROGRAM_COUNT: AtomicU64 = AtomicU64::new(0);
static RISCV_TIMER_CANCEL_COUNT: AtomicU64 = AtomicU64::new(0);
static RISCV_TIMER_LOG_COUNT: AtomicU64 = AtomicU64::new(0);
static RISCV_IRQ_VECTORS: [AtomicBool; MAX_ARCH_IRQ_VECTORS] = [const { AtomicBool::new(false) }; MAX_ARCH_IRQ_VECTORS];
static RISCV_S_EXT_IRQ_ENABLED: AtomicBool = AtomicBool::new(false);
static RISCV_LAST_PERCPU_CPU_ID: AtomicU64 = AtomicU64::new(u64::MAX);
static RISCV_LAST_TIMER_CALLBACK_CPU_ID: AtomicU64 = AtomicU64::new(u64::MAX);
static RISCV_LAST_TIMER_PROGRAM_CPU_ID: AtomicU64 = AtomicU64::new(u64::MAX);
static RISCV_LAST_TIMER_CANCEL_CPU_ID: AtomicU64 = AtomicU64::new(u64::MAX);

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static RISCV_TIMER_BRIDGE: Mutex<Option<fn(usize, u64)>> = None;
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static RISCV_TIMER_BACKEND: Mutex<RiscvTimerBackend> = RiscvTimerBackend {
        events: KVVec::new(),
        callback: None,
        active_timer: None,
    };
}

struct RiscvTimerEvent {
    cpu_id: usize,
    deadline_nanos: u64,
}

#[pin_data]
struct RiscvTimerCallbackState {
    #[pin]
    timer: HrTimer<Self>,
    cpu_id: AtomicU64,
    deadline_nanos: AtomicU64,
    generation: AtomicU64,
}

struct RiscvTimerBackend {
    events: KVVec<Arc<RiscvTimerEvent>>,
    callback: Option<Arc<RiscvTimerCallbackState>>,
    active_timer: Option<ArcHrTimerHandle<RiscvTimerCallbackState>>,
}

impl RiscvTimerEvent {
    fn new(cpu_id: usize, deadline_nanos: u64) -> Result<Arc<Self>> {
        Ok(Arc::new(
            Self {
                cpu_id,
                deadline_nanos,
            },
            GFP_KERNEL,
        )?)
    }
}

impl RiscvTimerCallbackState {
    fn new(cpu_id: usize, deadline_nanos: u64) -> impl pin_init::PinInit<Self> {
        pin_init::pin_init!(Self {
            timer <- HrTimer::new(),
            cpu_id <- AtomicU64::new(cpu_id as u64),
            deadline_nanos <- AtomicU64::new(deadline_nanos),
            generation <- AtomicU64::new(0),
        })
    }
}

impl HrTimerCallback for RiscvTimerCallbackState {
    type Pointer<'a> = Arc<Self>;

    fn run(
        this: kernel::sync::ArcBorrow<'_, Self>,
        _ctx: HrTimerCallbackContext<'_, Self>,
    ) -> HrTimerRestart {
        let deadline_nanos = this.deadline_nanos.load(Ordering::Acquire);
        let cpu_id = usize::try_from(this.cpu_id.load(Ordering::Acquire)).unwrap_or(0);
        let generation = this.generation.load(Ordering::Acquire);
        RISCV_TIMER_DEADLINE_NANOS.store(NO_DEADLINE_NANOS, Ordering::Release);
        RISCV_TIMER_FIRE_COUNT.fetch_add(1, Ordering::AcqRel);
        RISCV_LAST_TIMER_CALLBACK_CPU_ID.store(cpu_id as u64, Ordering::Release);
        let log_idx = RISCV_TIMER_LOG_COUNT.fetch_add(1, Ordering::AcqRel) + 1;
        if log_idx <= 16 || log_idx.is_power_of_two() {
            pr_info!(
                "axvisor_adapter: hrtimer callback cpu_id={} deadline_nanos={} generation={} log_idx={}\n",
                cpu_id,
                deadline_nanos,
                generation,
                log_idx
            );
        }
        this.deadline_nanos.store(NO_DEADLINE_NANOS, Ordering::Release);
        let mut backend = RISCV_TIMER_BACKEND.lock();
        backend.active_timer = None;
        backend
            .events
            .retain(|entry| !(entry.cpu_id == cpu_id && entry.deadline_nanos == deadline_nanos));
        drop(backend);
        if let Some(bridge) = *RISCV_TIMER_BRIDGE.lock() {
            bridge(cpu_id, deadline_nanos);
        }
        let _ = generation;
        HrTimerRestart::NoRestart
    }
}

impl_has_hr_timer! {
    impl HasHrTimer<Self> for RiscvTimerCallbackState {
        mode: RelativeMode<Monotonic>, field: self.timer
    }
}

pub(crate) fn init_backend() {
    // SAFETY: Called exactly once during module initialization.
    unsafe {
        RISCV_TIMER_BRIDGE.init();
        RISCV_TIMER_BACKEND.init();
    };
    let mut backend = RISCV_TIMER_BACKEND.lock();
    if backend.callback.is_none() {
        let cpu_id = usize::try_from(u32::from(CpuId::current())).unwrap_or(0);
        backend.callback = Arc::pin_init(RiscvTimerCallbackState::new(cpu_id, NO_DEADLINE_NANOS), GFP_KERNEL).ok();
    }
}

pub(crate) fn register_timer_bridge(handler: fn(usize, u64)) {
    *RISCV_TIMER_BRIDGE.lock() = Some(handler);
}

pub(crate) fn init_percpu(cpu_id: usize) -> bool {
    // 5. HostIf::init_percpu
    RISCV_PERCPU_READY.store(true, Ordering::Release);
    RISCV_LAST_PERCPU_CPU_ID.store(cpu_id as u64, Ordering::Release);
    true
}

pub(crate) fn set_oneshot_timer(deadline_nanos: u64) {
    // 10. TimeIf::set_oneshot_timer
    let cpu_id = usize::try_from(u32::from(CpuId::current())).unwrap_or(0);
    RISCV_TIMER_PROGRAM_COUNT.fetch_add(1, Ordering::AcqRel);
    RISCV_LAST_TIMER_PROGRAM_CPU_ID.store(cpu_id as u64, Ordering::Release);
    RISCV_TIMER_DEADLINE_NANOS.store(deadline_nanos, Ordering::Release);
    let program_count = RISCV_TIMER_PROGRAM_COUNT.load(Ordering::Acquire);
    if program_count <= 16 || program_count.is_power_of_two() {
        let now_nanos = Instant::<Monotonic>::now().as_nanos();
        pr_info!(
            "axvisor_adapter: set_oneshot_timer cpu_id={} deadline_nanos={} now_nanos={} program_count={}\n",
            cpu_id,
            deadline_nanos,
            now_nanos,
            program_count
        );
    }
    let mut backend = RISCV_TIMER_BACKEND.lock();
    backend.events.retain(|entry| entry.cpu_id != cpu_id);
    if deadline_nanos != NO_DEADLINE_NANOS {
        if let Ok(event) = RiscvTimerEvent::new(cpu_id, deadline_nanos) {
            let _ = backend.events.push(event, GFP_KERNEL);
        }
    }
    if let Some(callback) = backend.callback.clone() {
        callback.cpu_id.store(cpu_id as u64, Ordering::Release);
        callback.deadline_nanos.store(deadline_nanos, Ordering::Release);
        callback.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(mut active_timer) = backend.active_timer.take() {
            let _ = active_timer.cancel();
            RISCV_TIMER_CANCEL_COUNT.fetch_add(1, Ordering::AcqRel);
            RISCV_LAST_TIMER_CANCEL_CPU_ID.store(cpu_id as u64, Ordering::Release);
        }
        if deadline_nanos != NO_DEADLINE_NANOS {
            let now_nanos = Instant::<Monotonic>::now().as_nanos();
            let deadline_i64 = deadline_nanos.min(i64::MAX as u64) as i64;
            let relative_nanos = if deadline_i64 > now_nanos {
                deadline_i64 - now_nanos
            } else {
                0
            };
            backend.active_timer = Some(callback.start(Delta::from_nanos(relative_nanos)));
        }
    }
}

pub(crate) fn timer_deadline_nanos() -> u64 {
    RISCV_TIMER_DEADLINE_NANOS.load(Ordering::Acquire)
}

pub(crate) fn timer_fire_count() -> u64 {
    RISCV_TIMER_FIRE_COUNT.load(Ordering::Acquire)
}

pub(crate) fn timer_program_count() -> u64 {
    RISCV_TIMER_PROGRAM_COUNT.load(Ordering::Acquire)
}

pub(crate) fn timer_cancel_count() -> u64 {
    RISCV_TIMER_CANCEL_COUNT.load(Ordering::Acquire)
}

pub(crate) fn timer_is_armed() -> bool {
    RISCV_TIMER_DEADLINE_NANOS.load(Ordering::Acquire) != NO_DEADLINE_NANOS
}

pub(crate) fn dispatch_external_irq(vector: usize) -> bool {
    // 21. IrqIf::handle_irq
    if is_supervisor_external_vector(vector) {
        /*
         * On the Linux-host path this vector is delivered as a vCPU VM-exit
         * for HS-level supervisor external interrupts. The physical PLIC
         * source is owned by Linux's irqchip/request_irq path; this function
         * only tells the vCPU loop to latch virtual IRQ state that may have
         * been raised by the Linux passthrough IRQ handler.
         */
        return true;
    }

    if vector >= MAX_ARCH_IRQ_VECTORS {
        return false;
    }
    RISCV_IRQ_VECTORS[vector].load(Ordering::Acquire)
}

pub(crate) fn register_irq_vector(vector: usize) -> bool {
    // 22. IrqIf::register_irq_handler
    if is_supervisor_external_vector(vector) {
        return RISCV_S_EXT_IRQ_ENABLED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
    }

    if vector >= MAX_ARCH_IRQ_VECTORS {
        return false;
    }

    RISCV_IRQ_VECTORS[vector]
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

pub(crate) fn riscv_supervisor_external_vector() -> usize {
    RISCV_S_EXT_VECTOR
}

pub(crate) fn is_supervisor_external_vector(vector: usize) -> bool {
    /*
     * AxVCpuExitReason carries RISC-V interrupt vectors as u64, but not every
     * path historically agreed on whether the high scause interrupt bit is
     * included. Treat both encodings as HS supervisor external interrupts.
     */
    vector == RISCV_S_EXT_VECTOR || vector == RISCV_S_EXT_CAUSE
}

pub(crate) fn percpu_ready() -> bool {
    RISCV_PERCPU_READY.load(Ordering::Acquire)
}

pub(crate) fn last_percpu_cpu_id() -> u64 {
    RISCV_LAST_PERCPU_CPU_ID.load(Ordering::Acquire)
}

pub(crate) fn last_timer_callback_cpu_id() -> u64 {
    RISCV_LAST_TIMER_CALLBACK_CPU_ID.load(Ordering::Acquire)
}

pub(crate) fn last_timer_program_cpu_id() -> u64 {
    RISCV_LAST_TIMER_PROGRAM_CPU_ID.load(Ordering::Acquire)
}

pub(crate) fn last_timer_cancel_cpu_id() -> u64 {
    RISCV_LAST_TIMER_CANCEL_CPU_ID.load(Ordering::Acquire)
}

pub(crate) fn external_irq_registered() -> bool {
    RISCV_S_EXT_IRQ_ENABLED.load(Ordering::Acquire)
}

pub(crate) fn host_fdt_vaddr() -> u64 {
    unsafe { axvisor_adapter_host_fdt_vaddr() as u64 }
}

pub(crate) fn host_fdt_size() -> usize {
    unsafe { axvisor_adapter_host_fdt_size() }
}

pub(crate) fn host_tsc_frequency_mhz() -> u32 {
    // 30. ArchIf::host_tsc_frequency_mhz
    unsafe { axvisor_adapter_host_tsc_frequency_mhz() as u32 }
}
