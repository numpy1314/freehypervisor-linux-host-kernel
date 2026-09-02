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
            HrTimerHandle, HrTimerPointer, HrTimerRestart, RelativeMode,
        },
    },
};

use kernel::ffi::c_uint;

unsafe extern "C" {
    fn axvisor_adapter_host_tsc_frequency_mhz() -> c_uint;
}

const NO_DEADLINE_NANOS: u64 = u64::MAX;

static X86_PERCPU_READY: AtomicBool = AtomicBool::new(false);
static X86_TIMER_DEADLINE_NANOS: AtomicU64 = AtomicU64::new(NO_DEADLINE_NANOS);
static X86_TIMER_FIRE_COUNT: AtomicU64 = AtomicU64::new(0);
static X86_TIMER_PROGRAM_COUNT: AtomicU64 = AtomicU64::new(0);
static X86_TIMER_CANCEL_COUNT: AtomicU64 = AtomicU64::new(0);
static X86_TIMER_LOG_COUNT: AtomicU64 = AtomicU64::new(0);
static X86_LAST_PERCPU_CPU_ID: AtomicU64 = AtomicU64::new(u64::MAX);
static X86_LAST_TIMER_CALLBACK_CPU_ID: AtomicU64 = AtomicU64::new(u64::MAX);
static X86_LAST_TIMER_PROGRAM_CPU_ID: AtomicU64 = AtomicU64::new(u64::MAX);
static X86_LAST_TIMER_CANCEL_CPU_ID: AtomicU64 = AtomicU64::new(u64::MAX);

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static X86_TIMER_BRIDGE: Mutex<Option<fn(usize, u64)>> = None;
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before first use.
    unsafe(uninit) static X86_TIMER_BACKEND: Mutex<X86TimerBackend> = X86TimerBackend {
        events: KVVec::new(),
        slots: KVVec::new(),
    };
}

struct X86TimerEvent {
    cpu_id: usize,
    deadline_nanos: u64,
}

#[pin_data]
struct X86TimerCallbackState {
    #[pin]
    timer: HrTimer<Self>,
    cpu_id: AtomicU64,
    deadline_nanos: AtomicU64,
    generation: AtomicU64,
}

struct X86TimerBackend {
    events: KVVec<Arc<X86TimerEvent>>,
    slots: KVVec<X86TimerSlot>,
}

struct X86TimerSlot {
    cpu_id: usize,
    callback: Arc<X86TimerCallbackState>,
    active_timer: Option<ArcHrTimerHandle<X86TimerCallbackState>>,
}

impl X86TimerEvent {
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

impl X86TimerCallbackState {
    fn new(cpu_id: usize, deadline_nanos: u64) -> impl pin_init::PinInit<Self> {
        pin_init::pin_init!(Self {
            timer <- HrTimer::new(),
            cpu_id <- AtomicU64::new(cpu_id as u64),
            deadline_nanos <- AtomicU64::new(deadline_nanos),
            generation <- AtomicU64::new(0),
        })
    }
}

impl HrTimerCallback for X86TimerCallbackState {
    type Pointer<'a> = Arc<Self>;

    fn run(
        this: kernel::sync::ArcBorrow<'_, Self>,
        _ctx: HrTimerCallbackContext<'_, Self>,
    ) -> HrTimerRestart {
        let deadline_nanos = this.deadline_nanos.load(Ordering::Acquire);
        let cpu_id = usize::try_from(this.cpu_id.load(Ordering::Acquire)).unwrap_or(0);
        let generation = this.generation.load(Ordering::Acquire);
        X86_TIMER_DEADLINE_NANOS.store(NO_DEADLINE_NANOS, Ordering::Release);
        X86_TIMER_FIRE_COUNT.fetch_add(1, Ordering::AcqRel);
        X86_LAST_TIMER_CALLBACK_CPU_ID.store(cpu_id as u64, Ordering::Release);
        let log_idx = X86_TIMER_LOG_COUNT.fetch_add(1, Ordering::AcqRel) + 1;
        if log_idx <= 16 || log_idx.is_power_of_two() {
            pr_info!(
                "axvisor_adapter: x86 hrtimer callback cpu_id={} deadline_nanos={} generation={} log_idx={}\n",
                cpu_id,
                deadline_nanos,
                generation,
                log_idx
            );
        }
        this.deadline_nanos.store(NO_DEADLINE_NANOS, Ordering::Release);
        let mut backend = X86_TIMER_BACKEND.lock();
        if let Some(slot) = backend.slots.iter_mut().find(|slot| slot.cpu_id == cpu_id) {
            slot.active_timer = None;
        }
        backend
            .events
            .retain(|entry| !(entry.cpu_id == cpu_id && entry.deadline_nanos == deadline_nanos));
        drop(backend);
        if let Some(bridge) = *X86_TIMER_BRIDGE.lock() {
            bridge(cpu_id, deadline_nanos);
        }
        HrTimerRestart::NoRestart
    }
}

impl_has_hr_timer! {
    impl HasHrTimer<Self> for X86TimerCallbackState {
        mode: RelativeMode<Monotonic>, field: self.timer
    }
}

pub(crate) fn init_backend() {
    // SAFETY: Called exactly once during module initialization.
    unsafe {
        X86_TIMER_BRIDGE.init();
        X86_TIMER_BACKEND.init();
    };
}

pub(crate) fn register_timer_bridge(handler: fn(usize, u64)) {
    *X86_TIMER_BRIDGE.lock() = Some(handler);
}

pub(crate) fn init_percpu(cpu_id: usize) -> bool {
    X86_PERCPU_READY.store(true, Ordering::Release);
    X86_LAST_PERCPU_CPU_ID.store(cpu_id as u64, Ordering::Release);
    true
}

pub(crate) fn set_oneshot_timer(deadline_nanos: u64) {
    let cpu_id = usize::try_from(u32::from(CpuId::current())).unwrap_or(0);
    X86_TIMER_PROGRAM_COUNT.fetch_add(1, Ordering::AcqRel);
    X86_LAST_TIMER_PROGRAM_CPU_ID.store(cpu_id as u64, Ordering::Release);
    X86_TIMER_DEADLINE_NANOS.store(deadline_nanos, Ordering::Release);
    let program_count = X86_TIMER_PROGRAM_COUNT.load(Ordering::Acquire);
    if program_count <= 16 || program_count.is_power_of_two() {
        let now_nanos = Instant::<Monotonic>::now().as_nanos();
        pr_info!(
            "axvisor_adapter: x86 set_oneshot_timer cpu_id={} deadline_nanos={} now_nanos={} program_count={}\n",
            cpu_id,
            deadline_nanos,
            now_nanos,
            program_count
        );
    }

    let mut backend = X86_TIMER_BACKEND.lock();
    backend.events.retain(|entry| entry.cpu_id != cpu_id);
    if deadline_nanos != NO_DEADLINE_NANOS {
        if let Ok(event) = X86TimerEvent::new(cpu_id, deadline_nanos) {
            let _ = backend.events.push(event, GFP_KERNEL);
        }
    }

    if backend.slots.iter().all(|slot| slot.cpu_id != cpu_id) {
        if let Ok(callback) =
            Arc::pin_init(X86TimerCallbackState::new(cpu_id, NO_DEADLINE_NANOS), GFP_KERNEL)
        {
            let _ = backend.slots.push(
                X86TimerSlot {
                    cpu_id,
                    callback,
                    active_timer: None,
                },
                GFP_KERNEL,
            );
        }
    }

    let Some(slot) = backend.slots.iter_mut().find(|slot| slot.cpu_id == cpu_id) else {
        pr_info!(
            "axvisor_adapter: x86 set_oneshot_timer failed to allocate timer slot cpu_id={}\n",
            cpu_id
        );
        return;
    };

    slot.callback
        .cpu_id
        .store(cpu_id as u64, Ordering::Release);
    slot.callback
        .deadline_nanos
        .store(deadline_nanos, Ordering::Release);
    slot.callback.generation.fetch_add(1, Ordering::AcqRel);
    if let Some(mut active_timer) = slot.active_timer.take() {
        let _ = active_timer.cancel();
        X86_TIMER_CANCEL_COUNT.fetch_add(1, Ordering::AcqRel);
        X86_LAST_TIMER_CANCEL_CPU_ID.store(cpu_id as u64, Ordering::Release);
    }
    if deadline_nanos != NO_DEADLINE_NANOS {
        let now_nanos = Instant::<Monotonic>::now().as_nanos();
        let deadline_i64 = deadline_nanos.min(i64::MAX as u64) as i64;
        let relative_nanos = if deadline_i64 > now_nanos {
            deadline_i64 - now_nanos
        } else {
            0
        };
        slot.active_timer = Some(slot.callback.clone().start(Delta::from_nanos(relative_nanos)));
    }
}

pub(crate) fn timer_fire_count() -> u64 {
    X86_TIMER_FIRE_COUNT.load(Ordering::Acquire)
}

pub(crate) fn percpu_ready() -> bool {
    X86_PERCPU_READY.load(Ordering::Acquire)
}

pub(crate) fn last_percpu_cpu_id() -> u64 {
    X86_LAST_PERCPU_CPU_ID.load(Ordering::Acquire)
}

pub(crate) fn dispatch_external_irq(_vector: usize) -> bool {
    false
}

pub(crate) fn register_irq_vector(_vector: usize) -> bool {
    true
}

pub(crate) fn host_fdt_vaddr() -> u64 {
    0
}

pub(crate) fn host_fdt_size() -> usize {
    0
}

pub(crate) fn host_tsc_frequency_mhz() -> u32 {
    unsafe { axvisor_adapter_host_tsc_frequency_mhz() as u32 }
}
