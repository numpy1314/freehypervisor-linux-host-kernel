use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axvcpu::InterruptTriggerMode;
use x86_vlapic::IoApicInterrupt;
use axvm::config::VMInterruptMode;

use crate::vmm::{vcpus, VCpuRef, VMRef};

const IOAPIC_VECTOR_BASE: usize = 0x20;
const IOAPIC_GSI_COUNT: usize = 24;
const IOAPIC_VECTOR_END: usize = IOAPIC_VECTOR_BASE + IOAPIC_GSI_COUNT;

const PIT_TIMER_GSI: usize = 0;
const COM1_GSI: usize = 4;
static PIT_CHECK_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static PIT_DUE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static PIT_NO_ROUTE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static PIT_INJECT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static QUEUE_IOAPIC_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static PIT_ARM_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_IRQ_FORWARDING_ENABLED: AtomicBool = AtomicBool::new(false);
static IOAPIC_IRQ_FORWARD_VM_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static IOAPIC_IRQ_FORWARD_VCPU_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static IOAPIC_IRQ_PENDING: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_IRQ_PENDING_LEVEL: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_IRQ_MASKED: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_IRQ_ACTIVATED: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_IRQ_EXPLICIT_PASSTHROUGH: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_IRQ_HANDLERS: [AtomicBool; IOAPIC_GSI_COUNT] =
    [const { AtomicBool::new(false) }; IOAPIC_GSI_COUNT];
static IOAPIC_DRAIN_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_FORWARD_INJECT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_FORWARD_DEFER_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_FORWARD_NO_ROUTE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_EOI_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" {
    fn axvisor_linux_x86_passthrough_irq_unmask(gsi: usize) -> bool;
    fn axvisor_linux_x86_passthrough_irq_handle_vector(vector: usize) -> bool;
    fn axvisor_linux_x86_passthrough_irq_poll(gsi: usize) -> bool;
}

fn host_emerg(msg: &str) {
    axvisor_api::host::emerg_write_bytes(msg.as_bytes());
}

fn should_log(count: usize, limit: usize) -> bool {
    // Early-boot detail plus a steady-state heartbeat (every 512th) so activity
    // during the post-LAPIC-disable freeze window remains observable instead of
    // going silent once `count` passes the last power of two.
    count <= limit || count.is_power_of_two() || count % 512 == 0
}

fn queue_ioapic_irq(vm: &VMRef, current_vcpu: &VCpuRef, irq: IoApicInterrupt) {
    let target_vcpu_id = irq.target_vcpu_id.unwrap_or(current_vcpu.id());
    // Diagnostic: record where each IOAPIC IRQ is being queued. If the guest
    // reprograms GSI0's destination APIC id after disabling its LAPIC timer,
    // target_vcpu_id could drift off the running vcpu id (0) and the interrupt
    // would land in a queue nobody drains -> jiffies freeze.
    let qcount = QUEUE_IOAPIC_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log(qcount, 32) {
        host_emerg(&alloc::format!(
            "x86_irq::queue_ioapic vm={} vector={:#x} entry_target={:?} target_vcpu={} current_vcpu={} level={} count={}\n",
            vm.id(),
            irq.vector,
            irq.target_vcpu_id,
            target_vcpu_id,
            current_vcpu.id(),
            irq.level_triggered,
            qcount
        ));
    }
    let trigger = if irq.level_triggered {
        InterruptTriggerMode::LevelTriggered
    } else {
        InterruptTriggerMode::EdgeTriggered
    };
    if let Err(err) =
        vcpus::queue_interrupt_with_trigger(vm.id(), target_vcpu_id, irq.vector as usize, trigger)
    {
        warn!(
            "Failed to queue x86 IOAPIC IRQ vector {:#x} for VM[{}] target VCpu[{}] current VCpu[{}]: {err:?}",
            irq.vector,
            vm.id(),
            target_vcpu_id,
            current_vcpu.id()
        );
    }
}

pub fn forward_passthrough_irq_from_vmexit(vm: &VMRef, vcpu: &VCpuRef, vector: usize) {
    if unsafe { axvisor_linux_x86_passthrough_irq_handle_vector(vector) } {
        return;
    }

    if !ioapic_irq_handler_registered(vector) {
        forward_passthrough_irq(vm, vcpu, vector);
    }
}

pub fn register_passthrough_gsi(gsi: usize) -> bool {
    if gsi >= IOAPIC_GSI_COUNT {
        return false;
    }
    IOAPIC_IRQ_EXPLICIT_PASSTHROUGH.fetch_or(gsi_bit(gsi), Ordering::AcqRel);
    true
}

pub fn inject_due_pit_irq0(vm: &VMRef, vcpu: &VCpuRef) {
    if !uses_emulated_ioapic(vm) {
        return;
    }

    let now_ns = axvisor_api::time::current_time_nanos();
    let check_count = PIT_CHECK_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log(check_count, 16) {
        trace!(
            "x86 PIT check_irq0 now_ns={} count={}",
            now_ns,
            check_count
        );
    }
    if !vm.get_devices().x86_pit_consume_irq0_if_due(now_ns) {
        return;
    }
    let due_count = PIT_DUE_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log(due_count, 32) {
        host_emerg(&alloc::format!(
            "x86_irq::pit_due vm={} vcpu={} now_ns={} count={}\n",
            vm.id(),
            vcpu.id(),
            now_ns,
            due_count
        ));
        info!("x86 PIT IRQ0 due now_ns={} count={}", now_ns, due_count);
    }

    let Some(irq) = vm.get_devices().x86_ioapic_assert_gsi(PIT_TIMER_GSI) else {
        let no_route_count = PIT_NO_ROUTE_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if should_log(no_route_count, 32) {
            host_emerg(&alloc::format!(
                "x86_irq::pit_no_route vm={} vcpu={} count={}\n",
                vm.id(),
                vcpu.id(),
                no_route_count
            ));
            info!(
                "x86 PIT IRQ0 due but vIOAPIC GSI0 is not ready count={}",
                no_route_count
            );
        }
        trace!("x86 PIT IRQ0 due but vIOAPIC GSI0 is not ready");
        return;
    };

    let inject_count = PIT_INJECT_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log(inject_count, 32) {
        host_emerg(&alloc::format!(
            "x86_irq::pit_inject vm={} vcpu={} vector={:#x} level={} count={}\n",
            vm.id(),
            vcpu.id(),
            irq.vector,
            irq.level_triggered,
            inject_count
        ));
        info!(
            "x86 PIT inject_irq0 vector={:#x} level_triggered={} count={}",
            irq.vector,
            irq.level_triggered,
            inject_count
        );
    }
    trace!("Injecting x86 PIT IRQ0 vector {:#x}", irq.vector);
    queue_ioapic_irq(vm, vcpu, irq);
}

/// Arm the host one-shot timer at the guest PIT channel 0's next IRQ0 deadline.
///
/// Under nested virtualization the VMX preemption timer is silently dropped, so
/// an idle guest that sits in `sti; hlt` never gets a periodic VM-exit and its
/// PIT/jiffies freeze. The software timer list (`vmm::timer`) only tracks the
/// LAPIC timer, which the guest disables after calibration, so `rearm_host_timer`
/// arms nothing during idle. Program the host hrtimer directly off the PIT
/// deadline so the idle vCPU is woken in time to inject the next IRQ0, restoring
/// the ~18ms tick cadence independent of the preemption timer.
///
/// Returns the armed deadline (absolute host monotonic ns) so the caller can
/// build a time-based wait condition that is immune to lost wake-ups.
pub fn arm_idle_wakeup_timer(vm: &VMRef) -> Option<u64> {
    if !uses_emulated_ioapic(vm) {
        return None;
    }

    let deadline_ns = vm.get_devices().x86_pit_next_irq0_deadline_ns()?;
    if deadline_ns == 0 {
        return None;
    }

    let count = PIT_ARM_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log(count, 16) {
        let now_ns = axvisor_api::time::current_time_nanos();
        host_emerg(&alloc::format!(
            "x86_irq::pit_arm vm={} deadline_ns={} now_ns={} delta_ns={} count={}\n",
            vm.id(),
            deadline_ns,
            now_ns,
            deadline_ns.saturating_sub(now_ns),
            count
        ));
    }

    axvisor_api::time::set_oneshot_timer(
        axvisor_api::time::TimeValue::from_nanos(deadline_ns),
    );
    Some(deadline_ns)
}

pub fn inject_pending_serial_irq(vm: &VMRef, vcpu: &VCpuRef) {
    if !uses_emulated_ioapic(vm) {
        return;
    }

    if !vm.get_devices().x86_serial_poll_irq() {
        return;
    }

    let Some(irq) = vm.get_devices().x86_ioapic_assert_gsi(COM1_GSI) else {
        trace!("x86 COM1 RX pending but vIOAPIC GSI4 is not ready");
        return;
    };

    trace!("Injecting x86 COM1 RX IRQ vector {:#x}", irq.vector);
    queue_ioapic_irq(vm, vcpu, irq);
}

pub fn inject_pending_ioapic_irq_after_eoi(vm: &VMRef, vcpu: &VCpuRef, vector: u8) {
    if !uses_emulated_ioapic(vm) {
        return;
    }

    let Some(eoi) = vm.get_devices().x86_ioapic_end_of_interrupt(vector) else {
        let eoi_count = IOAPIC_EOI_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if vector == IOAPIC_VECTOR_BASE as u8 || should_log(eoi_count, 32) {
            host_emerg(&alloc::format!(
                "x86_irq::eoi_no_route vector={vector:#x} count={eoi_count}\n"
            ));
        }
        return;
    };
    let pending = eoi.pending;
    let has_pending = pending.is_some();
    let pending_level = pending.is_some_and(|irq| irq.level_triggered);
    let should_rearm = !pending_level;
    let eoi_count = IOAPIC_EOI_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if vector == IOAPIC_VECTOR_BASE as u8 || should_log(eoi_count, 32) {
        host_emerg(&alloc::format!(
            "x86_irq::eoi vector={vector:#x} gsi={} pending={} pending_level={} rearm={} count={eoi_count}\n",
            eoi.gsi,
            has_pending,
            pending_level,
            should_rearm
        ));
    }

    if should_rearm {
        unmask_forwarded_host_gsi(eoi.gsi);
    }

    let Some(irq) = pending else {
        return;
    };

    trace!(
        "Injecting pending x86 IOAPIC level IRQ vector {:#x} after EOI {vector:#x}",
        irq.vector
    );
    queue_ioapic_irq(vm, vcpu, irq);
}

pub fn drain_pending_ioapic_irqs(vm: &VMRef, vcpu: &VCpuRef) {
    if !IOAPIC_IRQ_HANDLERS
        .iter()
        .any(|registered| registered.load(Ordering::Acquire))
    {
        return;
    }

    activate_ready_ioapic_forwarding_routes(vm);
    poll_activated_passthrough_gsis();

    let pending = IOAPIC_IRQ_PENDING.swap(0, Ordering::AcqRel);
    if pending == 0 {
        return;
    }
    let pending_level = IOAPIC_IRQ_PENDING_LEVEL.fetch_and(!pending, Ordering::AcqRel) & pending;
    let drain_count = IOAPIC_DRAIN_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log(drain_count, 32) {
        host_emerg(&alloc::format!(
            "x86_irq::drain pending={pending:#x} pending_level={pending_level:#x} count={drain_count}\n"
        ));
    }

    let mut retry_pending = 0;
    let mut retry_level_pending = 0;
    for gsi in 0..IOAPIC_GSI_COUNT {
        let bit = 1usize << gsi;
        if pending & bit == 0 {
            continue;
        }

        let level_triggered = pending_level & bit != 0;
        if forward_passthrough_gsi(vm, vcpu, gsi, level_triggered) {
            if !level_triggered {
                unmask_forwarded_host_gsi(gsi);
            }
        } else {
            retry_pending |= bit;
            retry_level_pending |= pending_level & bit;
        }
    }

    if retry_pending != 0 {
        IOAPIC_IRQ_PENDING.fetch_or(retry_pending, Ordering::AcqRel);
        IOAPIC_IRQ_PENDING_LEVEL.fetch_or(retry_level_pending, Ordering::AcqRel);
    }
}

pub fn enable_ioapic_irq_forwarding(vm: &VMRef, vcpu: &VCpuRef) {
    if vm.interrupt_mode() != VMInterruptMode::Passthrough {
        return;
    }

    IOAPIC_IRQ_FORWARD_VM_ID.store(vm.id(), Ordering::Release);
    IOAPIC_IRQ_FORWARD_VCPU_ID.store(vcpu.id(), Ordering::Release);

    if IOAPIC_IRQ_FORWARDING_ENABLED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let mut registered = 0;
    for vector in IOAPIC_VECTOR_BASE..IOAPIC_VECTOR_END {
        let gsi = vector - IOAPIC_VECTOR_BASE;
        if IOAPIC_IRQ_HANDLERS[gsi].load(Ordering::Acquire) {
            continue;
        }
        if axvisor_api::irq::register_irq_handler(vector, ioapic_irq_forwarding_handler) {
            IOAPIC_IRQ_HANDLERS[gsi].store(true, Ordering::Release);
            registered += 1;
        } else {
            trace!("x86 IOAPIC host vector {vector:#x} already has a host handler");
        }
    }
    info!(
        "Enabled x86 IOAPIC IRQ forwarding for host vectors {:#x}..{:#x} ({} newly registered)",
        IOAPIC_VECTOR_BASE,
        IOAPIC_VECTOR_END - 1,
        registered
    );
    activate_ready_ioapic_forwarding_routes(vm);
}

fn ioapic_irq_handler_registered(vector: usize) -> bool {
    if !(IOAPIC_VECTOR_BASE..IOAPIC_VECTOR_END).contains(&vector) {
        return false;
    }

    let gsi = vector - IOAPIC_VECTOR_BASE;
    IOAPIC_IRQ_HANDLERS[gsi].load(Ordering::Acquire)
}

pub fn disable_ioapic_irq_forwarding_for_vm(vm_id: usize) {
    if IOAPIC_IRQ_FORWARD_VM_ID.load(Ordering::Acquire) != vm_id {
        return;
    }

    IOAPIC_IRQ_FORWARD_VM_ID.store(usize::MAX, Ordering::Release);
    IOAPIC_IRQ_FORWARD_VCPU_ID.store(usize::MAX, Ordering::Release);
    IOAPIC_IRQ_PENDING.store(0, Ordering::Release);
    IOAPIC_IRQ_PENDING_LEVEL.store(0, Ordering::Release);
    IOAPIC_IRQ_MASKED.store(0, Ordering::Release);
}

fn forward_passthrough_irq(vm: &VMRef, vcpu: &VCpuRef, vector: usize) {
    if vm.interrupt_mode() != VMInterruptMode::Passthrough {
        return;
    }

    if !(IOAPIC_VECTOR_BASE..IOAPIC_VECTOR_END).contains(&vector) {
        return;
    }

    let host_gsi = vector - IOAPIC_VECTOR_BASE;
    let Some(guest_irq) = vm.get_devices().x86_ioapic_assert_gsi(host_gsi) else {
        trace!(
            "x86 passthrough IRQ vector {vector:#x} has no injectable guest vIOAPIC route for \
             host GSI {host_gsi}"
        );
        return;
    };

    debug!(
        "Forwarding x86 passthrough IRQ host GSI {host_gsi} vector {vector:#x} to guest vector \
         {:#x}",
        guest_irq.vector
    );
    queue_ioapic_irq(vm, vcpu, guest_irq);
}

fn forward_passthrough_gsi(
    vm: &VMRef,
    vcpu: &VCpuRef,
    guest_gsi: usize,
    host_level_triggered: bool,
) -> bool {
    if vm.interrupt_mode() != VMInterruptMode::Passthrough {
        return true;
    }

    if guest_gsi >= IOAPIC_GSI_COUNT {
        return true;
    }

    let Some(guest_irq) = vm.get_devices().x86_ioapic_assert_gsi(guest_gsi) else {
        if vm.get_devices().x86_ioapic_vector_for_gsi(guest_gsi).is_some() {
            let defer_count = IOAPIC_FORWARD_DEFER_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if should_log(defer_count, 32) {
                host_emerg(&alloc::format!(
                    "x86_irq::forward_defer gsi={guest_gsi} host_level={host_level_triggered} count={defer_count}\n"
                ));
            }
            if !host_level_triggered {
                unmask_forwarded_host_gsi(guest_gsi);
            }
            return true;
        }

        let no_route_count = IOAPIC_FORWARD_NO_ROUTE_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if should_log(no_route_count, 32) {
            host_emerg(&alloc::format!(
                "x86_irq::forward_no_route gsi={guest_gsi} host_level={host_level_triggered} count={no_route_count}\n"
            ));
        }
        trace!("x86 passthrough IRQ has no injectable guest vIOAPIC route for GSI {guest_gsi}");
        return false;
    };

    let inject_count = IOAPIC_FORWARD_INJECT_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log(inject_count, 32) {
        host_emerg(&alloc::format!(
            "x86_irq::forward_inject gsi={guest_gsi} vector={:#x} target={:?} guest_level={} host_level={} count={inject_count}\n",
            guest_irq.vector,
            guest_irq.target_vcpu_id,
            guest_irq.level_triggered,
            host_level_triggered
        ));
    }
    queue_ioapic_irq(vm, vcpu, guest_irq);
    true
}

fn ioapic_irq_forwarding_handler(vector: usize) {
    if !(IOAPIC_VECTOR_BASE..IOAPIC_VECTOR_END).contains(&vector) {
        return;
    }

    if IOAPIC_IRQ_FORWARD_VM_ID.load(Ordering::Acquire) == usize::MAX
        || IOAPIC_IRQ_FORWARD_VCPU_ID.load(Ordering::Acquire) == usize::MAX
    {
        return;
    }

    let bit = 1usize << (vector - IOAPIC_VECTOR_BASE);
    if IOAPIC_IRQ_EXPLICIT_PASSTHROUGH.load(Ordering::Acquire) & bit == 0 {
        return;
    }
    IOAPIC_IRQ_PENDING.fetch_or(bit, Ordering::AcqRel);
}

pub fn mark_passthrough_gsi_pending(vm_id: usize, gsi: usize) -> bool {
    if gsi >= IOAPIC_GSI_COUNT {
        return false;
    }

    if IOAPIC_IRQ_EXPLICIT_PASSTHROUGH.load(Ordering::Acquire) & gsi_bit(gsi) == 0 {
        return false;
    }

    if IOAPIC_IRQ_FORWARD_VM_ID.load(Ordering::Acquire) != vm_id
        || IOAPIC_IRQ_FORWARD_VCPU_ID.load(Ordering::Acquire) == usize::MAX
    {
        return false;
    }

    let bit = 1usize << gsi;
    IOAPIC_IRQ_MASKED.fetch_or(bit, Ordering::AcqRel);
    IOAPIC_IRQ_PENDING_LEVEL.fetch_or(bit, Ordering::AcqRel);
    IOAPIC_IRQ_PENDING.fetch_or(bit, Ordering::AcqRel);
    vcpus::notify_all_vcpus(vm_id);
    true
}

pub fn activate_ready_ioapic_forwarding_routes(vm: &VMRef) {
    if vm.interrupt_mode() != VMInterruptMode::Passthrough {
        return;
    }

    let explicit = IOAPIC_IRQ_EXPLICIT_PASSTHROUGH.load(Ordering::Acquire);
    for gsi in 0..IOAPIC_GSI_COUNT {
        let bit = 1usize << gsi;
        if explicit & bit == 0 {
            continue;
        }
        if IOAPIC_IRQ_ACTIVATED.load(Ordering::Acquire) & bit != 0 {
            continue;
        }
        if vm.get_devices().x86_ioapic_vector_for_gsi(gsi).is_none() {
            continue;
        }
        if IOAPIC_IRQ_ACTIVATED.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
            continue;
        }

        host_emerg(&alloc::format!("x86_irq::activate_gsi gsi={gsi}\n"));
        unmask_forwarded_host_gsi(gsi);
    }
}

fn poll_activated_passthrough_gsis() {
    let activated = IOAPIC_IRQ_ACTIVATED.load(Ordering::Acquire);
    let explicit = IOAPIC_IRQ_EXPLICIT_PASSTHROUGH.load(Ordering::Acquire);
    let masked = IOAPIC_IRQ_MASKED.load(Ordering::Acquire);
    let candidates = activated & explicit & !masked;

    for gsi in 0..IOAPIC_GSI_COUNT {
        let bit = gsi_bit(gsi);
        if candidates & bit == 0 {
            continue;
        }
        if unsafe { axvisor_linux_x86_passthrough_irq_poll(gsi) } {
            IOAPIC_IRQ_MASKED.fetch_or(bit, Ordering::AcqRel);
            IOAPIC_IRQ_PENDING_LEVEL.fetch_or(bit, Ordering::AcqRel);
            IOAPIC_IRQ_PENDING.fetch_or(bit, Ordering::AcqRel);
        }
    }
}

fn gsi_bit(gsi: usize) -> usize {
    1usize << gsi
}

fn unmask_forwarded_host_gsi(gsi: usize) {
    if gsi >= IOAPIC_GSI_COUNT {
        return;
    }

    let bit = 1usize << gsi;
    let was_masked = IOAPIC_IRQ_MASKED.load(Ordering::Acquire) & bit != 0;
    let was_activated = IOAPIC_IRQ_ACTIVATED.load(Ordering::Acquire) & bit != 0;

    if unsafe { axvisor_linux_x86_passthrough_irq_unmask(gsi) } {
        IOAPIC_IRQ_MASKED.fetch_and(!bit, Ordering::AcqRel);
    } else {
        IOAPIC_IRQ_MASKED.fetch_or(bit, Ordering::AcqRel);
        if was_activated {
            IOAPIC_IRQ_PENDING_LEVEL.fetch_or(bit, Ordering::AcqRel);
            IOAPIC_IRQ_PENDING.fetch_or(bit, Ordering::AcqRel);
        }
    }
    if was_masked || !was_activated {
        host_emerg(&alloc::format!(
            "x86_irq::unmask_gsi gsi={gsi} was_masked={was_masked} was_activated={was_activated}\n"
        ));
    }
}

fn uses_emulated_ioapic(vm: &VMRef) -> bool {
    matches!(
        vm.interrupt_mode(),
        VMInterruptMode::Emulated | VMInterruptMode::Passthrough
    )
}
