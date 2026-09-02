// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use alloc::{boxed::Box, format, sync::Arc};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use ax_errno::{AxResult, ax_err};
use axvisor_api::{
    time::{self, nanos_to_ticks, ticks_to_nanos},
    vmm::{self, VCpuId, VMId},
};

use crate::{
    consts::RESET_LVT_REG,
    regs::lvt::{
        LVT_TIMER::{self, TimerMode::Value as TimerMode},
        LvtTimerRegisterLocal,
    },
};

static APIC_TIMER_LVT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static APIC_TIMER_DCR_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static APIC_TIMER_ICR_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static APIC_TIMER_START_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static APIC_TIMER_EXPIRE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Lower bound on the LAPIC timer period, in nanoseconds.
///
/// Mirrors KVM's `min_timer_period_us` (default 500us). A guest can program an
/// arbitrarily small initial-count, and under nested virtualization the cost of
/// servicing one timer interrupt (VM-exit, software injection, EOI, re-arming
/// the host hrtimer) easily exceeds a few tens of microseconds. Observed under
/// Firecracker: the innermost guest programs a periodic LAPIC timer with
/// initial_count=1562 / divide=16 => 25us (40kHz). Each period the interrupt is
/// re-injected before the previous one is fully retired, so the guest spins in
/// its timer ISR forever and never leaves early boot (it is pinned in the APIC
/// calibration path). Clamping the effective period to at least 500us caps the
/// interrupt rate at ~2kHz, which nested delivery can keep up with, letting the
/// guest make forward progress. Guests that ask for a sane period (>= 500us) are
/// unaffected.
const MIN_APIC_TIMER_PERIOD_NS: u64 = 500_000;

/// Clamp a guest-requested LAPIC timer period to [`MIN_APIC_TIMER_PERIOD_NS`].
/// A zero interval means "stopped" and is passed through unchanged.
fn clamp_timer_period_ns(interval_ns: u64) -> u64 {
    if interval_ns == 0 {
        0
    } else {
        interval_ns.max(MIN_APIC_TIMER_PERIOD_NS)
    }
}

fn timer_emerg_write(msg: &str) {
    axvisor_api::host::emerg_write_bytes(msg.as_bytes());
}

/// A virtual local APIC timer. (SDM Vol. 3C, Section 11.5.4)
///
/// This struct virtualizes the access to 4 registers in the Local APIC:
///
/// - LVT Timer Register. (SDM Vol. 3A, Section 11.5.1, Figure 11-8, offset 0x320, MSR 0x832, Read/Write)
/// - Divide Configuration Register. (SDM Vol. 3A, Section 11.5.4, Figure 11-10, offset 0x3E0, MSR 0x83E, Read/Write)
/// - Initial Count Register. (SDM Vol. 3A, Section 11.5.4, Figure 11-11, offset 0x380, MSR 0x838, Read/Write)
/// - Current Count Register. (SDM Vol. 3A, Section 11.5.4, Figure 11-11, offset 0x390, MSR 0x839, Read Only)
///
/// The timer works in the following way:
///
/// - Timer is started by and only by writing to the Initial Count Register.
/// - The deadline is determined by the Initial Count Register and the Divide Configuration Register, at the time of the start.
/// - Any modification to the Divide Configuration Register or the LVT Timer Register will not affect the current timer.
/// - Any write to the Initial Count Register will restart the timer.
/// - The value of the LVT Timer is read, at the time the deadline is reached, to determine
///   - if an interrupt should be generated (not masked),
///   - if the timer should be restarted (periodic mode), and
///   - the interrupt vector number to be used.
/// - The delivery status field in the LVT Timer Register is not supported and always returns 0.
/// - The timer stops when:
///   - the deadline is reached, and the timer is in one-shot mode, or
///   - a 0 is written to the Initial Count Register.
pub struct ApicTimer {
    // the raw value of writable registers
    /// Local Vector Table Timer Register. These's another copy in [`VirtualApicRegs`](crate::VirtualApicRegs), but we
    /// keep a separate copy here for easier access.
    lvt_timer_register: LvtTimerRegisterLocal,
    /// Initial Count Register. This is the value that determines when the timer will fire.
    initial_count_register: u32,
    /// Divide Configuration Register. This determines the frequency of the timer.
    divide_configuration_register: u32,

    // internal states
    divide_shift: u8,
    last_start_ticks: u64,
    deadline_ns: u64,

    // temporary fields untils we find a permanent place for apic and its timer
    cancel_token: Option<usize>,
    where_am_i: (VMId, VCpuId), // (vm_id, vcpu_id)
    shared: Arc<ApicTimerShared>,
}

struct ApicTimerShared {
    generation: AtomicUsize,
    lvt_timer_register: AtomicU32,
    interval_ns: AtomicU64,
    deadline_ns: AtomicU64,
    /// Coalescing "a periodic tick is owed" latch (0 or 1), the analog of KVM's
    /// `struct kvm_timer::pending`. Set to 1 by the host-timer callback when the
    /// timer expires (independent of whether the target vCPU is on-core), and
    /// swapped back to 0 by the vCPU at VM-entry (`take_pending_tick`) when it
    /// injects the tick. Being a single bit, many missed periods collapse to one
    /// owed tick — no catch-up storm — exactly like KVM's coalescing counter.
    tick_pending: AtomicU32,
}

impl ApicTimer {
    pub(crate) fn new(vm_id: VMId, vcpu_id: VCpuId) -> Self {
        Self {
            lvt_timer_register: LvtTimerRegisterLocal::new(RESET_LVT_REG), /* masked, one-shot, vector 0 */
            initial_count_register: 0,                                     // 0 (stopped)
            divide_configuration_register: 0,                              // divide by 2

            divide_shift: 1, /* as `divide_configuration_register` is 0, the shift is 1 (divide by 2) */
            last_start_ticks: 0,
            deadline_ns: 0,
            cancel_token: None,
            where_am_i: (vm_id, vcpu_id),
            shared: Arc::new(ApicTimerShared {
                generation: AtomicUsize::new(0),
                lvt_timer_register: AtomicU32::new(RESET_LVT_REG),
                interval_ns: AtomicU64::new(0),
                deadline_ns: AtomicU64::new(0),
                tick_pending: AtomicU32::new(0),
            }),
        }
    }

    // /// Check if an interrupt generated. if yes, update it's states.
    // pub fn check_interrupt(&mut self) -> bool {
    //     if self.deadline_ns == 0 {
    //         false
    //     } else if H::current_time_nanos() >= self.deadline_ns {
    //         if self.is_periodic() {
    //             self.deadline_ns += self.interval_ns();
    //         } else {
    //             self.deadline_ns = 0;
    //         }
    //         !self.is_masked()
    //     } else {
    //         false
    //     }
    // }

    #[allow(dead_code)]
    pub fn read_lvt(&self) -> u32 {
        self.lvt_timer_register.get()
    }

    pub fn write_lvt(&mut self, mut value: u32) -> AxResult {
        // valid bits: 0-7, 12, 16-18
        const LVT_MASK: u32 = 0x0007_10FF;

        value &= LVT_MASK;
        self.lvt_timer_register.set(value);
        self.shared
            .lvt_timer_register
            .store(value, Ordering::Release);
        let count = APIC_TIMER_LVT_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            let (vm_id, vcpu_id) = self.where_am_i;
            timer_emerg_write(
                format!(
                    "x86_apic_timer::write_lvt vm={:?} vcpu={:?} value={:#x} vector={:#x} masked={} mode={:?} count={}\n",
                    vm_id,
                    vcpu_id,
                    value,
                    self.vector(),
                    self.is_masked(),
                    self.timer_mode(),
                    count
                )
                .as_str(),
            );
            info!(
                "x86 apic_timer write_lvt vm={:?} vcpu={:?} value={:#x} vector={:#x} masked={} mode={:?} count={}",
                vm_id,
                vcpu_id,
                value,
                self.vector(),
                self.is_masked(),
                self.timer_mode(),
                count
            );
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn read_icr(&self) -> u32 {
        self.initial_count_register
    }

    pub fn write_icr(&mut self, value: u32) -> AxResult {
        // stop the timer no matter whether it is started, and no matter the value
        self.stop_timer()?;
        self.initial_count_register = value;
        let count = APIC_TIMER_ICR_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            let (vm_id, vcpu_id) = self.where_am_i;
            timer_emerg_write(
                format!(
                    "x86_apic_timer::write_icr vm={:?} vcpu={:?} value={} vector={:#x} masked={} mode={:?} count={}\n",
                    vm_id,
                    vcpu_id,
                    value,
                    self.vector(),
                    self.is_masked(),
                    self.timer_mode(),
                    count
                )
                .as_str(),
            );
            info!(
                "x86 apic_timer write_icr vm={:?} vcpu={:?} value={} vector={:#x} masked={} mode={:?} count={}",
                vm_id,
                vcpu_id,
                value,
                self.vector(),
                self.is_masked(),
                self.timer_mode(),
                count
            );
        }

        if value > 0 {
            self.start_timer()
        } else {
            Ok(())
        }
    }

    /// Read from the Divide Configuration Register.
    #[allow(dead_code)]
    pub fn read_dcr(&self) -> u32 {
        self.divide_configuration_register
    }

    /// Write to the Divide Configuration Register.
    pub fn write_dcr(&mut self, mut value: u32) {
        const DCR_MASK: u32 = 0b1011;

        value &= DCR_MASK;
        let shift = match value {
            0b0000 => 1, // divide by 2
            0b0001 => 2, // divide by 4
            0b0010 => 3, // divide by 8
            0b0011 => 4, // divide by 16
            0b1000 => 5, // divide by 32
            0b1001 => 6, // divide by 64
            0b1010 => 7, // divide by 128
            0b1011 => 0, // divide by 1
            _ => unreachable!(
                "internal error: invalid divide configuration register value after mask"
            ),
        };

        self.divide_configuration_register = value;
        self.divide_shift = shift as u8;
        let count = APIC_TIMER_DCR_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            let (vm_id, vcpu_id) = self.where_am_i;
            info!(
                "x86 apic_timer write_dcr vm={:?} vcpu={:?} value={:#x} divide_shift={} count={}",
                vm_id,
                vcpu_id,
                value,
                self.divide_shift,
                count
            );
        }
    }

    /// Current Count Register.
    ///
    /// This is a pure, side-effect-free computation of the count remaining until
    /// the next deadline. It MUST NOT mutate `shared.deadline_ns`: guests poll
    /// CCR heavily (calibrate_APIC_clock, udelay loops), and any store here would
    /// race with the periodic re-arm in `schedule_apic_timer`. A poll landing
    /// past the deadline would push the shared deadline forward by a whole
    /// interval; the next real host-timer expiry would then observe
    /// `deadline > now` and re-arm at that polluted future deadline, so the
    /// period runs away (doubling each pass) and the guest LAPIC tick effectively
    /// stops — after which no scheduler tick wakes the idle CPUs and boot hangs.
    /// Scheduling state is owned exclusively by the expiry path.
    pub fn read_ccr(&self) -> u32 {
        if !self.is_started() {
            return 0;
        }
        let deadline_ns = self.shared.deadline_ns.load(Ordering::Acquire);
        let now_ns = time::current_time_nanos();
        let remaining_ns = if now_ns < deadline_ns {
            deadline_ns - now_ns
        } else if self.is_periodic() {
            let interval_ns = self.shared.interval_ns.load(Ordering::Acquire);
            if interval_ns == 0 {
                return 0;
            }
            // Derive the phase within the current period without advancing the
            // scheduled deadline.
            let over = (now_ns - deadline_ns) % interval_ns;
            if over == 0 { 0 } else { interval_ns - over }
        } else {
            0
        };
        let remaining_ticks = nanos_to_ticks(remaining_ns);
        (remaining_ticks >> self.divide_shift) as _
    }

    /// Get the timer mode.
    pub fn timer_mode(&self) -> TimerMode {
        self.lvt_timer_register
            .read_as_enum(LVT_TIMER::TimerMode)
            .unwrap() // just panic if the value is invalid
    }

    /// Check whether the timer interrupt is masked.
    #[allow(dead_code)]
    pub fn is_masked(&self) -> bool {
        self.lvt_timer_register.is_set(LVT_TIMER::Mask)
    }

    /// The timer interrupt vector number.
    pub fn vector(&self) -> u8 {
        self.lvt_timer_register.read(LVT_TIMER::Vector) as u8
    }

    /// Acknowledge that the guest has retired (EOI'd) the outstanding periodic
    /// timer tick.
    ///
    /// In the KVM-aligned model, tick retirement is implicit: the vCPU clears
    /// the pending latch at VM-entry when it injects the tick
    /// (`take_pending_tick`). The EOI hook is therefore a no-op, kept only so the
    /// vlapic EOI path (`VirtualApicRegs::process_eoi`) still compiles and can be
    /// re-purposed later without touching that call site.
    pub fn ack_tick(&self) {}

    /// Take the owed periodic tick for injection at VM-entry, mirroring KVM's
    /// `kvm_inject_apic_timer_irqs`. Returns the timer vector if a tick is owed
    /// and the LVT is not masked, atomically clearing the latch. The 0/1 latch
    /// makes this idempotent: a racing set is either consumed now or on the next
    /// VM-entry, and it can never cause a double injection.
    pub fn take_pending_tick(&self) -> Option<u8> {
        if self.is_masked() {
            return None;
        }
        if self.shared.tick_pending.swap(0, Ordering::AcqRel) != 0 {
            Some(self.vector())
        } else {
            None
        }
    }

    /// Check whether the timer is started.
    pub fn is_started(&self) -> bool {
        // these two conditions are equivalent actually, we check both for clarity and robustness
        self.initial_count_register > 0 && self.cancel_token.is_some()
    }

    /// Restart the timer. Will not start the timer if it is not started.
    pub fn restart_timer(&mut self) -> AxResult {
        if !self.is_started() {
            Ok(())
        } else {
            self.stop_timer()?;
            self.start_timer()
        }
    }

    /// Start the timer.
    pub fn start_timer(&mut self) -> AxResult {
        if self.is_started() {
            return ax_err!(BadState, "Timer already started");
        }

        let current_ns = time::current_time_nanos();
        let interval_ticks = (self.initial_count_register as u64) << self.divide_shift;
        // Clamp the guest-requested period to a sane minimum. See
        // MIN_APIC_TIMER_PERIOD_NS: a too-small period causes a timer interrupt
        // storm under nested virtualization. The clamped value is what we store
        // into shared.interval_ns, so the periodic reschedule callback inherits
        // the same floor.
        let interval_ns = clamp_timer_period_ns(ticks_to_nanos(interval_ticks));
        let deadline_ns = current_ns.saturating_add(interval_ns);
        let (vm_id, vcpu_id) = self.where_am_i;
        let vector = self.vector();
        let generation = self.next_generation();

        trace!(
            "vlapic @ (vm {vm_id}, vcpu {vcpu_id}) starts timer @ ns {current_ns:?}, \
             deadline ns {deadline_ns:?}"
        );

        self.last_start_ticks = current_ns;
        self.deadline_ns = deadline_ns;
        // A freshly (re)started timer has no owed tick yet.
        self.shared.tick_pending.store(0, Ordering::Release);
        self.shared
            .interval_ns
            .store(interval_ns, Ordering::Release);
        self.shared
            .deadline_ns
            .store(self.deadline_ns, Ordering::Release);
        let count = APIC_TIMER_START_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            timer_emerg_write(
                format!(
                    "x86_apic_timer::start vm={:?} vcpu={:?} initial_count={} divide_shift={} vector={:#x} masked={} mode={:?} current_ns={} interval_ns={} deadline_ns={} count={}\n",
                    vm_id,
                    vcpu_id,
                    self.initial_count_register,
                    self.divide_shift,
                    vector,
                    self.is_masked(),
                    self.timer_mode(),
                    current_ns,
                    interval_ns,
                    self.deadline_ns,
                    count
                )
                .as_str(),
            );
            info!(
                "x86 apic_timer start vm={:?} vcpu={:?} initial_count={} divide_shift={} vector={:#x} masked={} mode={:?} current_ns={} interval_ns={} deadline_ns={} count={}",
                vm_id,
                vcpu_id,
                self.initial_count_register,
                self.divide_shift,
                vector,
                self.is_masked(),
                self.timer_mode(),
                current_ns,
                interval_ns,
                self.deadline_ns,
                count
            );
        }

        self.cancel_token = Some(schedule_apic_timer(
            time::current_time() + core::time::Duration::from_nanos(interval_ns),
            Arc::clone(&self.shared),
            generation,
            vm_id,
            vcpu_id,
        ));

        Ok(())
    }

    pub fn stop_timer(&mut self) -> AxResult {
        // TODO: maybe disable irq here?
        self.next_generation();
        self.last_start_ticks = 0;
        self.deadline_ns = 0;
        self.shared.interval_ns.store(0, Ordering::Release);
        self.shared.deadline_ns.store(0, Ordering::Release);
        // Drop any owed tick: on guest reprogram/stop, a tick from the old
        // program must not survive (mirrors KVM `cancel_apic_timer` clearing
        // `pending`). `next_generation` above already voids an in-flight
        // callback's re-arm, and `cancel_timer` below removes the table entry.
        self.shared.tick_pending.store(0, Ordering::Release);

        if let Some(token) = self.cancel_token.take() {
            time::cancel_timer(token);
        }

        Ok(())
    }

    /// Whether the timer mode is periodic.
    pub fn is_periodic(&self) -> bool {
        self.timer_mode() == TimerMode::Periodic
    }

    fn next_generation(&self) -> usize {
        self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn invalidate_timer(&self) {
        self.next_generation();
        self.shared.interval_ns.store(0, Ordering::Release);
        self.shared.deadline_ns.store(0, Ordering::Release);
    }

    // /// Set LVT Timer Register.
    // pub fn set_lvt_timer(&mut self, bits: u32) -> RvmResult {
    //     let timer_mode = bits.get_bits(17..19);
    //     if timer_mode == TimerMode::TscDeadline as _ {
    //         return rvm_err!(Unsupported); // TSC deadline mode was not supported
    //     } else if timer_mode == 0b11 {
    //         return rvm_err!(InvalidParam); // reserved
    //     }
    //     self.lvt_timer_bits = bits;
    //     self.start_timer();
    //     Ok(())
    // }

    // /// Set Initial Count Register.
    // pub fn set_initial_count(&mut self, initial: u32) -> RvmResult {
    //     self.initial_count = initial;
    //     self.start_timer();
    //     Ok(())
    // }

    // /// Set Divide Configuration Register.
    // pub fn set_divide(&mut self, dcr: u32) -> RvmResult {
    //     let shift = (dcr & 0b11) | ((dcr & 0b1000) >> 1);
    //     self.divide_shift = (shift + 1) as u8 & 0b111;
    //     self.start_timer();
    //     Ok(())
    // }

    // const fn interval_ns(&self) -> u64 {
    //     (self.initial_count as u64 * APIC_CYCLE_NANOS) << self.divide_shift
    // }

    // fn start_timer(&mut self) {
    //     if self.initial_count != 0 {
    //         self.last_start_cycle = H::current_time_nanos();
    //         self.deadline_ns = self.last_start_cycle + self.interval_ns();
    //     } else {
    //         self.deadline_ns = 0;
    //     }
    // }
}

impl Drop for ApicTimer {
    fn drop(&mut self) {
        self.invalidate_timer();
    }
}

fn schedule_apic_timer(
    deadline: core::time::Duration,
    shared: Arc<ApicTimerShared>,
    generation: usize,
    vm_id: VMId,
    vcpu_id: VCpuId,
) -> usize {
    vmm::register_timer_on_vcpu(
        vm_id,
        vcpu_id,
        deadline,
        Box::new(move |_| {
            // Mirror KVM's `apic_timer_fn`: this host-timer callback does NOT
            // inject directly and does NOT gate the periodic re-arm behind any
            // inject/drop decision. It (1) latches a single coalescing "tick
            // pending" flag (KVM `lapic_timer.pending`), (2) kicks the target
            // vCPU so it runs and drains the latch at VM-entry, and (3) always
            // re-arms a live periodic timer on the host clock. Tick liveness is
            // thus owned by the host timer wheel, not by the target vCPU being
            // scheduled — a starved off-core vCPU's tick keeps advancing.
            //
            // The generation check is retained ONLY to stop a callback of an
            // already-cancelled timer (stop_timer/write_icr bumped generation
            // then removed the table entry) from re-arming a stale timer.
            if shared.generation.load(Ordering::Acquire) != generation {
                return;
            }

            let lvt = shared.lvt_timer_register.load(Ordering::Acquire);
            let vector = (lvt & 0xff) as u8;
            let masked = (lvt & LVT_TIMER::Mask::SET.mask()) != 0;
            let mode = (lvt & LVT_TIMER::TimerMode::SET.mask()) >> 17;

            if !masked {
                let count = APIC_TIMER_EXPIRE_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if count <= 32 || count.is_power_of_two() {
                    timer_emerg_write(
                        format!(
                            "x86_apic_timer::expire vm={:?} vcpu={:?} vector={:#x} masked={} mode={} lvt={:#x} generation={} count={}\n",
                            vm_id,
                            vcpu_id,
                            vector,
                            masked,
                            mode,
                            lvt,
                            generation,
                            count
                        )
                        .as_str(),
                    );
                }

                // (1) Latch the owed tick (0/1 coalescing, like KVM
                // `atomic_inc(&lapic_timer.pending)` with its early-return
                // coalescing collapsed to a single-bit store). The vCPU drains
                // this at VM-entry via `take_pending_tick`.
                shared.tick_pending.store(1, Ordering::Release);

                // (2) Kick the target vCPU. `vmm::inject_interrupt` queues into
                // the vCPU's coalescing pending_events and wakes/boosts it; it
                // is used here purely as the kick. Both this and the VM-entry
                // latch feed 0/1-coalescing structures, so no double injection.
                vmm::inject_interrupt(vm_id, vcpu_id, vector);
            }

            // (3) Always re-arm a live periodic timer, decoupled from any
            // inject/drop/mask decision. `next_periodic_deadline_ns` advances by
            // one interval and clamps to `now` when behind (KVM
            // `advance_periodic_target_expiration`), so a starved vCPU never
            // replays missed periods.
            if mode == TimerMode::Periodic as u32
                && shared.generation.load(Ordering::Acquire) == generation
            {
                let interval_ns = shared.interval_ns.load(Ordering::Acquire);
                if interval_ns != 0 {
                    let old_deadline = shared.deadline_ns.load(Ordering::Acquire);
                    let next_deadline_ns = next_periodic_deadline_ns(
                        old_deadline,
                        interval_ns,
                        time::current_time_nanos(),
                    );
                    shared
                        .deadline_ns
                        .store(next_deadline_ns, Ordering::Release);
                    let _ = schedule_apic_timer(
                        core::time::Duration::from_nanos(next_deadline_ns),
                        shared,
                        generation,
                        vm_id,
                        vcpu_id,
                    );
                }
            }
        }),
    )
}

fn next_periodic_deadline_ns(deadline_ns: u64, interval_ns: u64, now_ns: u64) -> u64 {
    // Advance by exactly one interval from the previous scheduled deadline so
    // the periodic cadence is fixed and cannot be perturbed by anything but the
    // expiry path itself. Only if we have fallen a full interval or more behind
    // `now` (e.g. the host hrtimer fired late under nested delivery) do we catch
    // up by whole intervals, preserving phase alignment to the original grid.
    let mut next = deadline_ns.saturating_add(interval_ns);
    if next <= now_ns {
        let missed = (now_ns - next) / interval_ns + 1;
        next = next.saturating_add(interval_ns.saturating_mul(missed));
    }
    next
}

#[cfg(test)]
mod tests {
    use axvisor_api::vmm::{VCpuId, VMId};

    use crate::{regs::lvt::LVT_TIMER::TimerMode::Value as TimerMode, timer::ApicTimer};

    #[test]
    fn test_apic_timer_creation() {
        let vm_id = VMId::from(1 as usize);
        let vcpu_id = VCpuId::from(0 as usize);
        let timer = ApicTimer::new(vm_id, vcpu_id);
        // Initial state should be stopped
        assert!(!timer.is_started());
        assert_eq!(timer.read_icr(), 0);
        assert_eq!(timer.read_dcr(), 0);
        // assert_eq!(timer.read_ccr(), 0);
        assert!(timer.is_masked());
        assert_eq!(timer.timer_mode(), TimerMode::OneShot);
        assert_eq!(timer.vector(), 0);
    }

    #[test]
    fn test_lvt_register_operations() {
        let vm_id = VMId::from(1 as usize);
        let vcpu_id = VCpuId::from(0 as usize);
        let mut timer = ApicTimer::new(vm_id, vcpu_id);

        // Test LVT write with valid bits
        assert!(timer.write_lvt(0x000710FF).is_ok());
        assert_eq!(timer.read_lvt() & 0x000710FF, 0x000710FF);

        // Test LVT write with invalid bits (should be masked)
        assert!(timer.write_lvt(0xFFFFFFFF).is_ok());
        assert_eq!(timer.read_lvt() & !0x000710FF, 0);

        // Test vector number
        assert!(timer.write_lvt(0x50).is_ok()); // vector 0x50
        assert_eq!(timer.vector(), 0x50);
    }

    #[test]
    fn test_divide_configuration_register() {
        let vm_id = VMId::from(1 as usize);
        let vcpu_id = VCpuId::from(0 as usize);
        let mut timer = ApicTimer::new(vm_id, vcpu_id);

        // Test different divide values
        timer.write_dcr(0b0000); // divide by 2
        assert_eq!(timer.read_dcr(), 0b0000);

        timer.write_dcr(0b0001); // divide by 4
        assert_eq!(timer.read_dcr(), 0b0001);

        timer.write_dcr(0b1011); // divide by 1
        assert_eq!(timer.read_dcr(), 0b1011);

        // Test invalid bits are masked
        timer.write_dcr(0xFFFFFFFF);
        assert_eq!(timer.read_dcr() & !0b1011, 0);
    }

    #[test]
    fn test_timer_mode() {
        let vm_id = VMId::from(1 as usize);
        let vcpu_id = VCpuId::from(0 as usize);
        let mut timer = ApicTimer::new(vm_id, vcpu_id);

        // Default should be one-shot
        assert_eq!(timer.timer_mode(), TimerMode::OneShot);
        assert!(!timer.is_periodic());

        // Set periodic mode (bit 17 = 1)
        assert!(timer.write_lvt(0x20000).is_ok());
        assert_eq!(timer.timer_mode(), TimerMode::Periodic);
        assert!(timer.is_periodic());
    }

    #[test]
    fn test_timer_mask() {
        let vm_id = VMId::from(1 as usize);
        let vcpu_id = VCpuId::from(0 as usize);
        let mut timer = ApicTimer::new(vm_id, vcpu_id);

        // Default should be masked
        assert!(timer.is_masked());

        // Unmask timer (bit 16 = 0)
        assert!(timer.write_lvt(0x50).is_ok()); // vector 0x50, not masked
        assert!(!timer.is_masked());

        // Mask timer (bit 16 = 1)
        assert!(timer.write_lvt(0x10050).is_ok()); // vector 0x50, masked
        assert!(timer.is_masked());
    }

    #[test]
    fn test_multiple_timers() {
        let vm_id = VMId::from(1 as usize);
        let timer1 = ApicTimer::new(vm_id, VCpuId::from(0 as usize));
        let timer2 = ApicTimer::new(vm_id, VCpuId::from(1 as usize));

        // Both timers should be independent
        assert!(!timer1.is_started());
        assert!(!timer2.is_started());
        assert_eq!(timer1.read_icr(), timer2.read_icr());
        assert_eq!(timer1.read_dcr(), timer2.read_dcr());
    }
}
