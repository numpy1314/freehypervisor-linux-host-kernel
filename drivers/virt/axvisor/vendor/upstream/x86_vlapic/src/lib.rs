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

//! Emulated Local APIC.
#![no_std]
#![doc = include_str!("../README.md")]

extern crate alloc;

#[macro_use]
extern crate log;

mod consts;
mod regs;
mod timer;
mod utils;
mod vlapic;

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicUsize, Ordering},
};

use ax_errno::{AxResult, ax_err};
use ax_kspin::SpinNoIrq as Mutex;
use ax_memory_addr::{AddrRange, PAGE_SIZE_4K};
use axaddrspace::{
    GuestPhysAddr, HostPhysAddr, HostVirtAddr,
    device::{AccessWidth, Port, PortRange, SysRegAddr, SysRegAddrRange},
};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use axvisor_api::{
    console,
    memory,
    time,
    vmm::{VCpuId, VMId},
};

use crate::{
    consts::{x2apic::x2apic_msr_access_reg, xapic::xapic_mmio_access_reg_offset},
    vlapic::{VirtualApicRegs, VlapicCpuUp},
};

#[repr(align(4096))]
struct APICAccessPage([u8; PAGE_SIZE_4K]);

static VIRTUAL_APIC_ACCESS_PAGE: APICAccessPage = APICAccessPage([0; PAGE_SIZE_4K]);

/// A emulated local APIC device.
pub struct EmulatedLocalApic {
    vlapic_regs: UnsafeCell<VirtualApicRegs>,
}

const IOAPIC_BASE: usize = 0xfec0_0000;
const IOAPIC_SIZE: usize = 0x1000;
const IOREGSEL: usize = 0x00;
const IOWIN: usize = 0x10;
const IOAPIC_ID: u32 = 0x00;
const IOAPIC_VER: u32 = 0x01;
const IOAPIC_ARB: u32 = 0x02;
const IOREDTBL_BASE: u32 = 0x10;
const IOAPIC_ID_VALUE: u32 = 0xfe << 24;
const MAX_REDIRECTION_ENTRY: usize = 23;
const REDIRECTION_ENTRY_COUNT: usize = MAX_REDIRECTION_ENTRY + 1;
const IOAPIC_VERSION_VALUE: u32 = 0x11 | ((MAX_REDIRECTION_ENTRY as u32) << 16);
const REDIRECTION_ENTRY_MASKED: u64 = 1 << 16;
const REDIRECTION_ENTRY_TRIGGER_MODE: u64 = 1 << 15;
const REDIRECTION_ENTRY_REMOTE_IRR: u64 = 1 << 14;
const REDIRECTION_ENTRY_DELIVERY_MODE_MASK: u64 = 0b111 << 8;
const REDIRECTION_ENTRY_DESTINATION_MODE: u64 = 1 << 11;
const REDIRECTION_ENTRY_DESTINATION_SHIFT: u64 = 56;
const REDIRECTION_ENTRY_DESTINATION_BROADCAST: u8 = 0xff;
const TRACE_GSI: usize = 5;
const TRACE_VECTOR: u8 = 0x20;

static VIOAPIC_ASSERT_TRACE_COUNT: AtomicUsize = AtomicUsize::new(0);
static VIOAPIC_EOI_TRACE_COUNT: AtomicUsize = AtomicUsize::new(0);
static VIOAPIC_WRITE_TRACE_COUNT: AtomicUsize = AtomicUsize::new(0);

fn vioapic_trace(counter: &AtomicUsize, message: alloc::string::String) {
    let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 32 || count.is_power_of_two() {
        console::write_bytes(message.as_bytes());
    }
}


#[derive(Debug)]
struct IoApicState {
    selector: u32,
    redirection_table: [u64; REDIRECTION_ENTRY_COUNT],
    pending_level: [bool; REDIRECTION_ENTRY_COUNT],
}

impl IoApicState {
    const fn new() -> Self {
        Self {
            selector: 0,
            redirection_table: [REDIRECTION_ENTRY_MASKED; REDIRECTION_ENTRY_COUNT],
            pending_level: [false; REDIRECTION_ENTRY_COUNT],
        }
    }

    fn interrupt_for_entry(&mut self, gsi: usize) -> Option<IoApicInterrupt> {
        let entry = self.redirection_table.get_mut(gsi)?;
        if *entry & REDIRECTION_ENTRY_MASKED != 0 {
            return None;
        }

        if *entry & REDIRECTION_ENTRY_DELIVERY_MODE_MASK != 0 {
            debug!("vIOAPIC GSI {gsi} uses unsupported delivery mode entry {entry:#x}");
            return None;
        }

        let vector = (*entry & 0xff) as u8;
        if vector < 16 {
            return None;
        }
        let target_vcpu_id = if *entry & REDIRECTION_ENTRY_DESTINATION_MODE == 0 {
            let destination = ((*entry >> REDIRECTION_ENTRY_DESTINATION_SHIFT) & 0xff) as u8;
            (destination != REDIRECTION_ENTRY_DESTINATION_BROADCAST).then_some(destination as usize)
        } else {
            debug!("vIOAPIC GSI {gsi} uses logical destination mode entry {entry:#x}");
            None
        };

        let level_triggered = *entry & REDIRECTION_ENTRY_TRIGGER_MODE != 0;
        if level_triggered {
            if *entry & REDIRECTION_ENTRY_REMOTE_IRR != 0 {
                self.pending_level[gsi] = true;
                return None;
            }
            *entry |= REDIRECTION_ENTRY_REMOTE_IRR;
        }

        Some(IoApicInterrupt {
            vector,
            level_triggered,
            target_vcpu_id,
        })
    }
}

/// Interrupt description returned by the virtual IOAPIC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApicInterrupt {
    /// Guest interrupt vector.
    pub vector: u8,
    /// Whether the interrupt is level-triggered.
    pub level_triggered: bool,
    /// Target vCPU selected by the IOAPIC redirection entry, when it is a physical APIC ID.
    pub target_vcpu_id: Option<usize>,
}

/// Result of a virtual IOAPIC EOI broadcast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApicEoi {
    /// The GSI whose remote-IRR state was cleared.
    pub gsi: usize,
    /// A deferred level-triggered interrupt that should be injected now.
    pub pending: Option<IoApicInterrupt>,
}

/// Minimal x86 IOAPIC model used by the AxVisor device layer.
pub struct EmulatedIoApic {
    base: GuestPhysAddr,
    size: usize,
    state: UnsafeCell<IoApicState>,
}

impl EmulatedIoApic {
    /// Create a new emulated IOAPIC.
    pub fn new(base: GuestPhysAddr, size: Option<usize>) -> Self {
        Self {
            base,
            size: size.unwrap_or(IOAPIC_SIZE),
            state: UnsafeCell::new(IoApicState::new()),
        }
    }

    /// Returns the guest vector programmed for a GSI.
    pub fn vector_for_gsi(&self, gsi: usize) -> Option<u8> {
        let state = unsafe { &*self.state.get() };
        let entry = *state.redirection_table.get(gsi)?;
        if entry & REDIRECTION_ENTRY_MASKED != 0 {
            return None;
        }
        if entry & REDIRECTION_ENTRY_DELIVERY_MODE_MASK != 0 {
            debug!("vIOAPIC GSI {gsi} uses unsupported delivery mode entry {entry:#x}");
            return None;
        }

        let vector = (entry & 0xff) as u8;
        (vector >= 16).then_some(vector)
    }

    /// Replace one redirection-table entry with a KVM-compatible raw value.
    pub fn set_redirection_entry(&self, gsi: usize, entry: u64) -> bool {
        let state = unsafe { &mut *self.state.get() };
        let Some(redir) = state.redirection_table.get_mut(gsi) else {
            return false;
        };
        *redir = entry & !REDIRECTION_ENTRY_REMOTE_IRR;
        if *redir & REDIRECTION_ENTRY_MASKED != 0 {
            state.pending_level[gsi] = false;
        }
        true
    }

    /// Assert a GSI and return the interrupt to inject into the guest LAPIC.
    pub fn assert_gsi(&self, gsi: usize) -> Option<IoApicInterrupt> {
        let state = unsafe { &mut *self.state.get() };
        let before = state.redirection_table.get(gsi).copied().unwrap_or(0);
        let pending_before = state.pending_level.get(gsi).copied().unwrap_or(false);
        let irq = state.interrupt_for_entry(gsi);
        if gsi == TRACE_GSI {
            let after = state.redirection_table.get(gsi).copied().unwrap_or(0);
            let pending_after = state.pending_level.get(gsi).copied().unwrap_or(false);
            vioapic_trace(
                &VIOAPIC_ASSERT_TRACE_COUNT,
                alloc::format!(
                    "vioapic::assert_gsi gsi={gsi} before={before:#x} after={after:#x} pending_before={pending_before} pending_after={pending_after} irq={irq:?}\n"
                ),
            );
        }
        irq
    }

    /// Mark a level-triggered interrupt complete and return its GSI and deferred interrupt.
    pub fn end_of_interrupt(&self, vector: u8) -> Option<IoApicEoi> {
        let state = unsafe { &mut *self.state.get() };
        if vector == TRACE_VECTOR {
            let entry = state.redirection_table[TRACE_GSI];
            let pending = state.pending_level[TRACE_GSI];
            vioapic_trace(
                &VIOAPIC_EOI_TRACE_COUNT,
                alloc::format!(
                    "vioapic::eoi_enter vector={vector:#x} trace_gsi={TRACE_GSI} entry={entry:#x} pending={pending}\n"
                ),
            );
        }
        for gsi in 0..REDIRECTION_ENTRY_COUNT {
            let matched = {
                let entry = &mut state.redirection_table[gsi];
                if (*entry & 0xff) as u8 != vector
                    || *entry & REDIRECTION_ENTRY_TRIGGER_MODE == 0
                    || *entry & REDIRECTION_ENTRY_REMOTE_IRR == 0
                {
                    false
                } else {
                    *entry &= !REDIRECTION_ENTRY_REMOTE_IRR;
                    true
                }
            };
            if !matched {
                continue;
            }

            let pending = core::mem::take(&mut state.pending_level[gsi])
                .then(|| state.interrupt_for_entry(gsi))
                .flatten();
            if vector == TRACE_VECTOR || gsi == TRACE_GSI {
                let entry = state.redirection_table[gsi];
                vioapic_trace(
                    &VIOAPIC_EOI_TRACE_COUNT,
                    alloc::format!(
                        "vioapic::eoi_match vector={vector:#x} gsi={gsi} entry={entry:#x} pending={pending:?}\n"
                    ),
                );
            }
            return Some(IoApicEoi { gsi, pending });
        }

        if vector == TRACE_VECTOR {
            let entry = state.redirection_table[TRACE_GSI];
            let pending = state.pending_level[TRACE_GSI];
            vioapic_trace(
                &VIOAPIC_EOI_TRACE_COUNT,
                alloc::format!(
                    "vioapic::eoi_no_match vector={vector:#x} trace_gsi={TRACE_GSI} entry={entry:#x} pending={pending}\n"
                ),
            );
        }
        None
    }

    fn read_selected_register(state: &IoApicState) -> AxResult<u32> {
        match state.selector {
            IOAPIC_ID => Ok(IOAPIC_ID_VALUE),
            IOAPIC_VER => Ok(IOAPIC_VERSION_VALUE),
            IOAPIC_ARB => Ok(IOAPIC_ID_VALUE),
            reg @ IOREDTBL_BASE..=0x3f => {
                let index = ((reg - IOREDTBL_BASE) / 2) as usize;
                if index >= REDIRECTION_ENTRY_COUNT {
                    return ax_err!(InvalidInput, "IOAPIC redirection index out of range");
                }
                let entry = state.redirection_table[index];
                if (reg - IOREDTBL_BASE) & 1 == 0 {
                    Ok(entry as u32)
                } else {
                    Ok((entry >> 32) as u32)
                }
            }
            reg => {
                debug!("vIOAPIC read from unsupported register {reg:#x}");
                Ok(0)
            }
        }
    }

    fn write_selected_register(state: &mut IoApicState, value: u32) -> AxResult {
        match state.selector {
            IOAPIC_ID | IOAPIC_VER | IOAPIC_ARB => Ok(()),
            reg @ IOREDTBL_BASE..=0x3f => {
                let index = ((reg - IOREDTBL_BASE) / 2) as usize;
                if index >= REDIRECTION_ENTRY_COUNT {
                    return ax_err!(InvalidInput, "IOAPIC redirection index out of range");
                }
                let entry = &mut state.redirection_table[index];
                if (reg - IOREDTBL_BASE) & 1 == 0 {
                    let old_entry = *entry;
                    let old_low = *entry & !REDIRECTION_ENTRY_REMOTE_IRR & 0xffff_ffff;
                    let new_low = (value as u64) & !REDIRECTION_ENTRY_REMOTE_IRR;
                    let remote_irr = if old_low == new_low {
                        *entry & REDIRECTION_ENTRY_REMOTE_IRR
                    } else {
                        state.pending_level[index] = false;
                        0
                    };
                    *entry = (*entry & !0xffff_ffff) | new_low | remote_irr;
                    if *entry & REDIRECTION_ENTRY_MASKED != 0 {
                        state.pending_level[index] = false;
                    }
                    if index == TRACE_GSI {
                        let new_entry = *entry;
                        let pending = state.pending_level[index];
                        vioapic_trace(
                            &VIOAPIC_WRITE_TRACE_COUNT,
                            alloc::format!(
                                "vioapic::write_low gsi={index} value={value:#x} old_entry={old_entry:#x} new_entry={new_entry:#x} pending={pending}\n"
                            ),
                        );
                    }
                } else {
                    *entry = (*entry & 0xffff_ffff) | ((value as u64) << 32);
                    if index == TRACE_GSI {
                        let new_entry = *entry;
                        let pending = state.pending_level[index];
                        vioapic_trace(
                            &VIOAPIC_WRITE_TRACE_COUNT,
                            alloc::format!(
                                "vioapic::write_high gsi={index} value={value:#x} new_entry={new_entry:#x} pending={pending}\n"
                            ),
                        );
                    }
                }
                Ok(())
            }
            reg => {
                debug!("vIOAPIC write to unsupported register {reg:#x} = {value:#x}");
                Ok(())
            }
        }
    }
}

impl BaseDeviceOps<AddrRange<GuestPhysAddr>> for EmulatedIoApic {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::X86IoApic
    }

    fn address_range(&self) -> AddrRange<GuestPhysAddr> {
        AddrRange::from_start_size(self.base, self.size)
    }

    fn handle_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> AxResult<usize> {
        if !matches!(width, AccessWidth::Dword | AccessWidth::Qword) {
            return ax_err!(Unsupported, "unsupported IOAPIC read width");
        }

        let offset = addr.as_usize().saturating_sub(self.base.as_usize());
        let state = unsafe { &*self.state.get() };
        match offset {
            IOREGSEL => Ok(state.selector as usize),
            IOWIN => Ok(Self::read_selected_register(state)? as usize),
            _ => {
                debug!("vIOAPIC read from unsupported offset {offset:#x}");
                Ok(0)
            }
        }
    }

    fn handle_write(&self, addr: GuestPhysAddr, width: AccessWidth, val: usize) -> AxResult {
        if !matches!(width, AccessWidth::Dword | AccessWidth::Qword) {
            return ax_err!(Unsupported, "unsupported IOAPIC write width");
        }

        let offset = addr.as_usize().saturating_sub(self.base.as_usize());
        let state = unsafe { &mut *self.state.get() };
        match offset {
            IOREGSEL => {
                state.selector = val as u32;
                Ok(())
            }
            IOWIN => Self::write_selected_register(state, val as u32),
            _ => {
                debug!("vIOAPIC write to unsupported offset {offset:#x} = {val:#x}");
                Ok(())
            }
        }
    }
}

const PIT_CHANNEL0: u16 = 0x40;
const PIT_CHANNEL2: u16 = 0x42;
const PIT_COMMAND: u16 = 0x43;
const PIT_SPEAKER_CONTROL: u16 = 0x61;
const PIT_PORT_END: u16 = PIT_SPEAKER_CONTROL;

const PIT_BASE_FREQUENCY_HZ: u64 = 1_193_182;
const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;
const MIN_PERIOD_NS: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessMode {
    LatchCount,
    LowByte,
    HighByte,
    LowThenHigh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PitMode {
    InterruptOnTerminalCount,
    HardwareRetriggerableOneShot,
    RateGenerator,
    SquareWaveGenerator,
    SoftwareTriggeredStrobe,
    HardwareTriggeredStrobe,
}

impl PitMode {
    fn from_command(command: u8) -> Self {
        match (command >> 1) & 0b111 {
            0 => Self::InterruptOnTerminalCount,
            1 => Self::HardwareRetriggerableOneShot,
            2 | 6 => Self::RateGenerator,
            3 | 7 => Self::SquareWaveGenerator,
            4 => Self::SoftwareTriggeredStrobe,
            _ => Self::HardwareTriggeredStrobe,
        }
    }

    const fn raw_bits(self) -> u8 {
        match self {
            Self::InterruptOnTerminalCount => 0,
            Self::HardwareRetriggerableOneShot => 1,
            Self::RateGenerator => 2,
            Self::SquareWaveGenerator => 3,
            Self::SoftwareTriggeredStrobe => 4,
            Self::HardwareTriggeredStrobe => 5,
        }
    }

    const fn is_periodic_irq(self) -> bool {
        matches!(self, Self::RateGenerator | Self::SquareWaveGenerator)
    }
}

impl AccessMode {
    fn from_command(command: u8) -> Self {
        match (command >> 4) & 0b11 {
            0 => Self::LatchCount,
            1 => Self::LowByte,
            2 => Self::HighByte,
            _ => Self::LowThenHigh,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PitChannel {
    access_mode: AccessMode,
    mode: PitMode,
    reload_value: u16,
    write_low_latched: Option<u8>,
    read_high_next: bool,
    latched_count: Option<u16>,
    latched_status: Option<u8>,
    null_count: bool,
    start_ns: u64,
    period_ns: Option<u64>,
    next_deadline_ns: u64,
    irq_fired: bool,
}

impl PitChannel {
    const fn new() -> Self {
        Self {
            access_mode: AccessMode::LowThenHigh,
            mode: PitMode::SquareWaveGenerator,
            reload_value: 0,
            write_low_latched: None,
            read_high_next: false,
            latched_count: None,
            latched_status: None,
            null_count: true,
            start_ns: 0,
            period_ns: None,
            next_deadline_ns: 0,
            irq_fired: false,
        }
    }

    fn divisor(&self) -> u64 {
        if self.reload_value == 0 {
            0x1_0000
        } else {
            self.reload_value as u64
        }
    }

    fn program_reload(&mut self, reload_value: u16, now_ns: u64) {
        self.reload_value = reload_value;
        let divisor = self.divisor();
        let period_ns =
            ((divisor * NANOSECONDS_PER_SECOND) / PIT_BASE_FREQUENCY_HZ).max(MIN_PERIOD_NS);
        self.start_ns = now_ns;
        self.period_ns = Some(period_ns);
        self.next_deadline_ns = now_ns.saturating_add(period_ns);
        self.read_high_next = false;
        self.latched_count = None;
        self.latched_status = None;
        self.null_count = false;
        self.irq_fired = false;
    }

    fn write_count(&mut self, value: u8, now_ns: u64) {
        match self.access_mode {
            AccessMode::LatchCount => {}
            AccessMode::LowByte => self.program_reload(value as u16, now_ns),
            AccessMode::HighByte => self.program_reload((value as u16) << 8, now_ns),
            AccessMode::LowThenHigh => {
                if let Some(low) = self.write_low_latched.take() {
                    self.program_reload(((value as u16) << 8) | low as u16, now_ns);
                } else {
                    self.write_low_latched = Some(value);
                }
            }
        }
    }

    fn elapsed_ticks(&self, now_ns: u64) -> u64 {
        let elapsed_ns = now_ns.saturating_sub(self.start_ns);
        elapsed_ns.saturating_mul(PIT_BASE_FREQUENCY_HZ) / NANOSECONDS_PER_SECOND
    }

    fn current_count(&self, now_ns: u64) -> u16 {
        let Some(_) = self.period_ns else {
            return self.reload_value;
        };
        let divisor = self.divisor();
        let elapsed_ticks = self.elapsed_ticks(now_ns);

        if !self.mode.is_periodic_irq() && elapsed_ticks >= divisor {
            return 0;
        }

        let remaining = divisor - (elapsed_ticks % divisor);
        if remaining == 0x1_0000 {
            0
        } else {
            remaining as u16
        }
    }

    fn output_high(&self, now_ns: u64) -> bool {
        let Some(_) = self.period_ns else {
            return true;
        };
        let divisor = self.divisor();
        let elapsed_ticks = self.elapsed_ticks(now_ns);
        match self.mode {
            PitMode::InterruptOnTerminalCount | PitMode::SoftwareTriggeredStrobe => {
                elapsed_ticks >= divisor
            }
            PitMode::RateGenerator => elapsed_ticks % divisor != divisor.saturating_sub(1),
            PitMode::SquareWaveGenerator => (elapsed_ticks % divisor) < divisor.div_ceil(2),
            PitMode::HardwareRetriggerableOneShot | PitMode::HardwareTriggeredStrobe => true,
        }
    }

    fn latch_status(&mut self, now_ns: u64) {
        if self.latched_status.is_none() {
            let mut status = (self.output_high(now_ns) as u8) << 7;
            status |= (self.null_count as u8) << 6;
            status |= match self.access_mode {
                AccessMode::LatchCount => 0,
                AccessMode::LowByte => 1,
                AccessMode::HighByte => 2,
                AccessMode::LowThenHigh => 3,
            } << 4;
            status |= self.mode.raw_bits() << 1;
            self.latched_status = Some(status);
        }
    }

    fn latch_count(&mut self, now_ns: u64) {
        if self.latched_count.is_none() {
            self.latched_count = Some(self.current_count(now_ns));
            self.read_high_next = false;
        }
    }

    fn read_count(&mut self, now_ns: u64) -> u8 {
        if let Some(status) = self.latched_status.take() {
            return status;
        }

        let value = self
            .latched_count
            .unwrap_or_else(|| self.current_count(now_ns));
        match self.access_mode {
            AccessMode::HighByte => {
                self.latched_count = None;
                (value >> 8) as u8
            }
            AccessMode::LowThenHigh => {
                if self.read_high_next {
                    self.read_high_next = false;
                    self.latched_count = None;
                    (value >> 8) as u8
                } else {
                    self.read_high_next = true;
                    value as u8
                }
            }
            AccessMode::LatchCount | AccessMode::LowByte => {
                self.latched_count = None;
                value as u8
            }
        }
    }
}

#[derive(Debug)]
struct PitState {
    channel0: PitChannel,
    channel2: PitChannel,
    speaker_control: u8,
}

impl PitState {
    const fn new() -> Self {
        Self {
            channel0: PitChannel::new(),
            channel2: PitChannel::new(),
            speaker_control: 0,
        }
    }
}

/// A minimal emulated x86 PIT/8254 device.
pub struct EmulatedPit {
    state: Mutex<PitState>,
}

impl EmulatedPit {
    /// Create a new PIT device.
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(PitState::new()),
        }
    }

    /// Return whether channel 0 has reached its next IRQ0 deadline.
    ///
    /// When a deadline is reached, this advances the deadline by whole periods so the timer
    /// remains periodic without queueing a burst of missed ticks.
    pub fn consume_irq0_if_due(&self, now_ns: u64) -> bool {
        let mut state = self.state.lock();
        let channel = &mut state.channel0;
        let Some(period_ns) = channel.period_ns else {
            return false;
        };
        if now_ns < channel.next_deadline_ns {
            return false;
        }

        if channel.mode.is_periodic_irq() {
            let elapsed = now_ns.saturating_sub(channel.next_deadline_ns);
            let missed_periods = elapsed / period_ns;
            channel.next_deadline_ns = channel
                .next_deadline_ns
                .saturating_add((missed_periods + 1).saturating_mul(period_ns));
        } else {
            if channel.irq_fired {
                return false;
            }
            channel.irq_fired = true;
        }
        true
    }

    /// Return the absolute wall-clock deadline (ns) of channel 0's next IRQ0,
    /// if the channel is armed. Used by the idle path to arm a host wake-up
    /// timer so an idle guest still receives periodic ticks even when the VMX
    /// preemption timer is unavailable.
    pub fn next_irq0_deadline_ns(&self) -> Option<u64> {
        let state = self.state.lock();
        let channel = &state.channel0;
        channel.period_ns?;
        if !channel.mode.is_periodic_irq() && channel.irq_fired {
            return None;
        }
        Some(channel.next_deadline_ns)
    }

    fn channel_mut(state: &mut PitState, channel: u8) -> Option<&mut PitChannel> {
        match channel {
            0 => Some(&mut state.channel0),
            2 => Some(&mut state.channel2),
            _ => None,
        }
    }

    fn write_command(state: &mut PitState, command: u8, now_ns: u64) {
        let channel = (command >> 6) & 0b11;
        if channel == 0b11 {
            Self::write_read_back_command(state, command, now_ns);
            return;
        }

        let access_mode = AccessMode::from_command(command);
        let mode = PitMode::from_command(command);
        let Some(pit_channel) = Self::channel_mut(state, channel) else {
            debug!("x86 PIT command for unsupported channel {channel}: {command:#x}");
            return;
        };

        if access_mode == AccessMode::LatchCount {
            pit_channel.latch_count(now_ns);
            return;
        }

        pit_channel.access_mode = access_mode;
        pit_channel.mode = mode;
        pit_channel.write_low_latched = None;
        pit_channel.read_high_next = false;
        pit_channel.latched_count = None;
        pit_channel.latched_status = None;
        pit_channel.null_count = true;
    }

    fn write_read_back_command(state: &mut PitState, command: u8, now_ns: u64) {
        let latch_count = command & (1 << 5) == 0;
        let latch_status = command & (1 << 4) == 0;
        let selected = command & 0b1110;

        if selected & (1 << 1) != 0 {
            if latch_count {
                state.channel0.latch_count(now_ns);
            }
            if latch_status {
                state.channel0.latch_status(now_ns);
            }
        }
        if selected & (1 << 3) != 0 {
            if latch_count {
                state.channel2.latch_count(now_ns);
            }
            if latch_status {
                state.channel2.latch_status(now_ns);
            }
        }
    }
}

impl Default for EmulatedPit {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseDeviceOps<PortRange> for EmulatedPit {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::X86Pit
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(Port::new(PIT_CHANNEL0), Port::new(PIT_PORT_END))
    }

    fn handle_read(&self, port: Port, width: AccessWidth) -> AxResult<usize> {
        if width != AccessWidth::Byte {
            return ax_err!(Unsupported, "x86 PIT only supports byte port reads");
        }

        let now_ns = time::current_time_nanos();
        let mut state = self.state.lock();
        let value = match port.number() {
            PIT_CHANNEL0 => state.channel0.read_count(now_ns),
            PIT_CHANNEL2 => state.channel2.read_count(now_ns),
            PIT_COMMAND => 0,
            PIT_SPEAKER_CONTROL => {
                let output = state.channel2.output_high(now_ns) as u8;
                (state.speaker_control & !0x20) | (output << 5)
            }
            _ => return ax_err!(Unsupported, "unsupported x86 PIT read port"),
        };
        Ok(value as usize)
    }

    fn handle_write(&self, port: Port, width: AccessWidth, val: usize) -> AxResult {
        if width != AccessWidth::Byte {
            return ax_err!(Unsupported, "x86 PIT only supports byte port writes");
        }

        let now_ns = time::current_time_nanos();
        let mut state = self.state.lock();
        match port.number() {
            PIT_CHANNEL0 => state.channel0.write_count(val as u8, now_ns),
            PIT_CHANNEL2 => state.channel2.write_count(val as u8, now_ns),
            PIT_COMMAND => Self::write_command(&mut state, val as u8, now_ns),
            PIT_SPEAKER_CONTROL => state.speaker_control = val as u8,
            _ => return ax_err!(Unsupported, "unsupported x86 PIT write port"),
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct PicChip {
    imr: u8,
    irr: u8,
    isr: u8,
    offset: u8,
    init_step: u8,
    expect_icw4: bool,
    read_isr: bool,
}

impl PicChip {
    const fn new(offset: u8) -> Self {
        Self {
            imr: 0,
            irr: 0,
            isr: 0,
            offset,
            init_step: 0,
            expect_icw4: false,
            read_isr: false,
        }
    }

    fn reset_for_init(&mut self, icw1: u8) {
        self.imr = 0;
        self.irr = 0;
        self.isr = 0;
        self.init_step = 1;
        self.expect_icw4 = icw1 & 0x01 != 0;
        self.read_isr = false;
    }

    fn write_command(&mut self, value: u8) {
        if value & 0x10 != 0 {
            self.reset_for_init(value);
            return;
        }
        if value & 0x18 == 0x08 {
            self.read_isr = value & 0x03 == 0x03;
            return;
        }
        if value & 0x20 != 0 {
            if value & 0x40 != 0 {
                self.isr &= !(1 << (value & 0x07));
            } else if self.isr != 0 {
                self.isr &= !(1 << self.isr.trailing_zeros());
            }
        }
    }

    fn write_data(&mut self, value: u8) {
        match self.init_step {
            1 => {
                self.offset = value & 0xf8;
                self.init_step = 2;
            }
            2 => {
                self.init_step = if self.expect_icw4 { 3 } else { 0 };
            }
            3 => {
                self.init_step = 0;
            }
            _ => {
                self.imr = value;
            }
        }
    }

    fn read_command(&self) -> u8 {
        if self.read_isr { self.isr } else { self.irr }
    }

    fn read_data(&self) -> u8 {
        self.imr
    }

    fn assert_irq(&mut self, irq: u8) -> Option<u8> {
        if irq >= 8 {
            return None;
        }
        let bit = 1u8 << irq;
        self.irr |= bit;
        if self.imr & bit != 0 {
            return None;
        }
        self.irr &= !bit;
        self.isr |= bit;
        Some(self.offset.wrapping_add(irq))
    }

    fn end_of_interrupt(&mut self, vector: u8) {
        if vector < self.offset || vector >= self.offset.saturating_add(8) {
            return;
        }
        self.isr &= !(1 << (vector - self.offset));
    }
}

#[derive(Clone, Copy)]
struct PicState {
    master: PicChip,
    slave: PicChip,
    elcr_master: u8,
    elcr_slave: u8,
}

impl PicState {
    const fn new() -> Self {
        Self {
            master: PicChip::new(0x20),
            slave: PicChip::new(0x28),
            elcr_master: 0,
            elcr_slave: 0,
        }
    }
}

/// Minimal 8259A-compatible PIC model for KVM in-kernel irqchip guests.
pub struct EmulatedPic {
    state: UnsafeCell<PicState>,
}

impl EmulatedPic {
    /// Create a new emulated legacy PIC.
    pub const fn new() -> Self {
        Self {
            state: UnsafeCell::new(PicState::new()),
        }
    }

    /// Assert a legacy IRQ and return the interrupt vector to inject.
    pub fn assert_irq(&self, irq: u8) -> Option<u8> {
        let state = unsafe { &mut *self.state.get() };
        if irq < 8 {
            state.master.assert_irq(irq)
        } else {
            let vector = state.slave.assert_irq(irq - 8)?;
            let _ = state.master.assert_irq(2);
            Some(vector)
        }
    }

    /// Complete a legacy PIC interrupt vector.
    pub fn end_of_interrupt(&self, vector: u8) {
        let state = unsafe { &mut *self.state.get() };
        state.master.end_of_interrupt(vector);
        state.slave.end_of_interrupt(vector);
    }
}

impl BaseDeviceOps<PortRange> for EmulatedPic {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::X86Pic
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(Port::new(0x20), Port::new(0x4d1))
    }

    fn handle_read(&self, addr: Port, _width: AccessWidth) -> AxResult<usize> {
        let state = unsafe { &mut *self.state.get() };
        let value = match addr.number() {
            0x20 => state.master.read_command(),
            0x21 => state.master.read_data(),
            0xa0 => state.slave.read_command(),
            0xa1 => state.slave.read_data(),
            0x4d0 => state.elcr_master,
            0x4d1 => state.elcr_slave,
            _ => 0,
        };
        Ok(value as usize)
    }

    fn handle_write(&self, addr: Port, _width: AccessWidth, val: usize) -> AxResult {
        let state = unsafe { &mut *self.state.get() };
        let value = val as u8;
        match addr.number() {
            0x20 => state.master.write_command(value),
            0x21 => state.master.write_data(value),
            0xa0 => state.slave.write_command(value),
            0xa1 => state.slave.write_data(value),
            0x4d0 => state.elcr_master = value & !0x03,
            0x4d1 => state.elcr_slave = value,
            _ => {}
        }
        Ok(())
    }
}

const COM1_BASE: u16 = 0x3f8;
const COM1_END: u16 = COM1_BASE + 7;

const REG_RBR_THR_DLL: u16 = 0;
const REG_IER_DLM: u16 = 1;
const REG_IIR_FCR: u16 = 2;
const REG_LCR: u16 = 3;
const REG_MCR: u16 = 4;
const REG_LSR: u16 = 5;
const REG_MSR: u16 = 6;
const REG_SCR: u16 = 7;

const IER_RX_AVAILABLE: u8 = 1 << 0;
const IER_THR_EMPTY: u8 = 1 << 1;

const IIR_NO_INTERRUPT: u8 = 0x01;
const IIR_THR_EMPTY: u8 = 0x02;
const IIR_RX_AVAILABLE: u8 = 0x04;
const IIR_FIFO_16550A: u8 = 0xc0;

const LCR_DLAB: u8 = 1 << 7;

const MCR_DTR: u8 = 1 << 0;
const MCR_RTS: u8 = 1 << 1;
const MCR_OUT1: u8 = 1 << 2;
const MCR_OUT2: u8 = 1 << 3;
const MCR_LOOPBACK: u8 = 1 << 4;

const LSR_DATA_READY: u8 = 1 << 0;
const LSR_THR_EMPTY: u8 = 1 << 5;
const LSR_TRANSMITTER_EMPTY: u8 = 1 << 6;

const MSR_RI: u8 = 1 << 6;
const MSR_DCD: u8 = 1 << 7;
const MSR_DSR: u8 = 1 << 5;
const MSR_CTS: u8 = 1 << 4;

const FIFO_CAPACITY: usize = 128;

#[derive(Debug)]
struct SerialState {
    ier: u8,
    fcr: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    thr_interrupt_pending: bool,
    rx_fifo: [u8; FIFO_CAPACITY],
    rx_head: usize,
    rx_len: usize,
}

impl SerialState {
    const fn new() -> Self {
        Self {
            ier: 0,
            fcr: 0,
            lcr: 0x03,
            mcr: 0,
            scr: 0,
            dll: 1,
            dlm: 0,
            thr_interrupt_pending: false,
            rx_fifo: [0; FIFO_CAPACITY],
            rx_head: 0,
            rx_len: 0,
        }
    }

    fn dlab(&self) -> bool {
        self.lcr & LCR_DLAB != 0
    }

    fn pop_rx(&mut self) -> Option<u8> {
        if self.rx_len == 0 {
            return None;
        }
        let byte = self.rx_fifo[self.rx_head];
        self.rx_head = (self.rx_head + 1) % self.rx_fifo.len();
        self.rx_len -= 1;
        Some(byte)
    }

    fn push_rx(&mut self, byte: u8) {
        if self.rx_len == self.rx_fifo.len() {
            return;
        }
        let index = (self.rx_head + self.rx_len) % self.rx_fifo.len();
        self.rx_fifo[index] = byte;
        self.rx_len += 1;
    }

    fn clear_rx(&mut self) {
        self.rx_head = 0;
        self.rx_len = 0;
    }

    fn lsr(&self) -> u8 {
        let mut value = LSR_THR_EMPTY | LSR_TRANSMITTER_EMPTY;
        if self.rx_len != 0 {
            value |= LSR_DATA_READY;
        }
        value
    }

    fn iir(&self) -> u8 {
        if self.ier & IER_RX_AVAILABLE != 0 && self.rx_len != 0 {
            IIR_FIFO_16550A | IIR_RX_AVAILABLE
        } else if self.ier & IER_THR_EMPTY != 0 && self.thr_interrupt_pending {
            IIR_FIFO_16550A | IIR_THR_EMPTY
        } else {
            IIR_FIFO_16550A | IIR_NO_INTERRUPT
        }
    }

    fn msr(&self) -> u8 {
        if self.mcr & MCR_LOOPBACK == 0 {
            return MSR_DCD | MSR_DSR | MSR_CTS;
        }

        let mut value = 0;
        if self.mcr & MCR_DTR != 0 {
            value |= MSR_DSR;
        }
        if self.mcr & MCR_RTS != 0 {
            value |= MSR_CTS;
        }
        if self.mcr & MCR_OUT1 != 0 {
            value |= MSR_RI;
        }
        if self.mcr & MCR_OUT2 != 0 {
            value |= MSR_DCD;
        }
        value
    }
}

/// 16550-compatible COM1 model adapted from rcore-os/tgoskits.
pub struct EmulatedSerialPort {
    state: Mutex<SerialState>,
}

impl EmulatedSerialPort {
    /// Create a new emulated serial port.
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(SerialState::new()),
        }
    }

    /// Return whether a RX interrupt is pending.
    pub fn poll_irq(&self) -> bool {
        let state = self.state.lock();
        (state.ier & IER_RX_AVAILABLE != 0 && state.rx_len != 0)
            || (state.ier & IER_THR_EMPTY != 0 && state.thr_interrupt_pending)
    }
}

impl Default for EmulatedSerialPort {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseDeviceOps<PortRange> for EmulatedSerialPort {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::Console
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(Port::new(COM1_BASE), Port::new(COM1_END))
    }

    fn handle_read(&self, addr: Port, width: AccessWidth) -> AxResult<usize> {
        if width != AccessWidth::Byte {
            return ax_err!(Unsupported, "x86 serial only supports byte port reads");
        }

        let mut state = self.state.lock();
        let offset = addr.number() - COM1_BASE;
        let value = match offset {
            REG_RBR_THR_DLL if state.dlab() => state.dll,
            REG_RBR_THR_DLL => state.pop_rx().unwrap_or(0),
            REG_IER_DLM if state.dlab() => state.dlm,
            REG_IER_DLM => state.ier,
            REG_IIR_FCR => {
                let value = state.iir();
                if value & 0x0f == IIR_THR_EMPTY {
                    state.thr_interrupt_pending = false;
                }
                value
            }
            REG_LCR => state.lcr,
            REG_MCR => state.mcr,
            REG_LSR => state.lsr(),
            REG_MSR => state.msr(),
            REG_SCR => state.scr,
            _ => return ax_err!(Unsupported, "unsupported x86 serial read port"),
        };
        Ok(value as usize)
    }

    fn handle_write(&self, addr: Port, width: AccessWidth, val: usize) -> AxResult {
        if width != AccessWidth::Byte {
            return ax_err!(Unsupported, "x86 serial only supports byte port writes");
        }

        let mut state = self.state.lock();
        let offset = addr.number() - COM1_BASE;
        let value = val as u8;
        match offset {
            REG_RBR_THR_DLL if state.dlab() => state.dll = value,
            REG_RBR_THR_DLL => {
                if state.mcr & MCR_LOOPBACK != 0 {
                    state.push_rx(value);
                } else {
                    console::write_bytes(&[value]);
                }
                if state.ier & IER_THR_EMPTY != 0 {
                    state.thr_interrupt_pending = true;
                }
            }
            REG_IER_DLM if state.dlab() => state.dlm = value,
            REG_IER_DLM => {
                state.ier = value & 0x0f;
                if state.ier & IER_THR_EMPTY != 0 {
                    state.thr_interrupt_pending = true;
                } else {
                    state.thr_interrupt_pending = false;
                }
            }
            REG_IIR_FCR => {
                state.fcr = value;
                if value & (1 << 1) != 0 {
                    state.clear_rx();
                }
            }
            REG_LCR => state.lcr = value,
            REG_MCR => state.mcr = value,
            REG_LSR | REG_MSR => {}
            REG_SCR => state.scr = value,
            _ => return ax_err!(Unsupported, "unsupported x86 serial write port"),
        }
        Ok(())
    }
}

impl EmulatedLocalApic {
    /// Create a new `EmulatedLocalApic`.
    pub fn new(vm_id: VMId, vcpu_id: VCpuId) -> Self {
        EmulatedLocalApic {
            vlapic_regs: UnsafeCell::new(VirtualApicRegs::new(vm_id, vcpu_id)),
        }
    }

    fn get_vlapic_regs(&self) -> &VirtualApicRegs {
        unsafe { &*self.vlapic_regs.get() }
    }

    #[allow(clippy::mut_from_ref)] // SAFETY: get_mut_vlapic_regs is never called concurrently.
    fn get_mut_vlapic_regs(&self) -> &mut VirtualApicRegs {
        unsafe { &mut *self.vlapic_regs.get() }
    }
}

impl EmulatedLocalApic {
    /// APIC-access address (64 bits).
    /// This field contains the physical address of the 4-KByte APIC-access page.
    /// If the “virtualize APIC accesses” VM-execution control is 1,
    /// access to this page may cause VM exits or be virtualized by the processor.
    /// See Section 30.4.
    pub fn virtual_apic_access_addr() -> HostPhysAddr {
        memory::virt_to_phys(HostVirtAddr::from_usize(
            VIRTUAL_APIC_ACCESS_PAGE.0.as_ptr() as usize,
        ))
    }

    /// Virtual-APIC address (64 bits).
    /// This field contains the physical address of the 4-KByte virtual-APIC page.
    /// The processor uses the virtual-APIC page to virtualize certain accesses to APIC registers and to manage virtual interrupts;
    /// see Chapter 30.
    pub fn virtual_apic_page_addr(&self) -> HostPhysAddr {
        self.get_vlapic_regs().virtual_apic_page_addr()
    }

    /// Returns the current IA32_APIC_BASE MSR value.
    pub fn apic_base(&self) -> u64 {
        self.get_vlapic_regs().apic_base()
    }

    /// Sets the IA32_APIC_BASE MSR value.
    pub fn set_apic_base(&self, value: u64) -> AxResult {
        self.get_mut_vlapic_regs().set_apic_base(value)
    }

    /// Process a guest EOI and return the vector that needs an IOAPIC EOI broadcast.
    pub fn handle_eoi(&self) -> Option<u8> {
        self.get_mut_vlapic_regs().handle_eoi()
    }

    /// Apply side effects for a VMX APIC-write VM exit.
    pub fn handle_apic_write_exit(&self, offset: usize) -> AxResult<Option<u8>> {
        self.get_mut_vlapic_regs().handle_apic_write_exit(offset)
    }

    /// Record that the guest accepted an injected interrupt.
    pub fn accept_interrupt(&self, vector: u8, level_triggered: bool) {
        self.get_mut_vlapic_regs()
            .accept_interrupt(vector, level_triggered);
    }

    /// Return and clear a pending SIPI-derived CPU-up request.
    pub fn take_pending_cpu_up(&self) -> Option<VlapicCpuUp> {
        self.get_mut_vlapic_regs().take_pending_cpu_up()
    }

    /// Take the owed periodic LAPIC timer tick for injection at VM-entry
    /// (kvm_inject_apic_timer_irqs analog). Returns the timer vector if a tick is
    /// owed and the LVT is unmasked, clearing the latch.
    pub fn take_pending_timer_tick(&self) -> Option<u8> {
        self.get_mut_vlapic_regs().take_pending_timer_tick()
    }
}

impl BaseDeviceOps<AddrRange<GuestPhysAddr>> for EmulatedLocalApic {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::InterruptController
    }

    fn address_range(&self) -> AddrRange<GuestPhysAddr> {
        use crate::consts::xapic::{APIC_MMIO_SIZE, DEFAULT_APIC_BASE};
        AddrRange::new(
            GuestPhysAddr::from_usize(DEFAULT_APIC_BASE),
            GuestPhysAddr::from_usize(DEFAULT_APIC_BASE + APIC_MMIO_SIZE),
        )
    }

    fn handle_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> AxResult<usize> {
        debug!("EmulatedLocalApic::handle_read: addr={addr:?}, width={width:?}");
        let reg_off = xapic_mmio_access_reg_offset(addr);
        self.get_vlapic_regs().handle_read(reg_off, width)
    }

    fn handle_write(&self, addr: GuestPhysAddr, width: AccessWidth, val: usize) -> AxResult {
        debug!("EmulatedLocalApic::handle_write: addr={addr:?}, width={width:?}, val={val:#x}");
        let reg_off = xapic_mmio_access_reg_offset(addr);
        self.get_mut_vlapic_regs().handle_write(reg_off, val, width)
    }
}

impl BaseDeviceOps<SysRegAddrRange> for EmulatedLocalApic {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::InterruptController
    }

    fn address_range(&self) -> SysRegAddrRange {
        use crate::consts::x2apic::{X2APIC_MSE_REG_BASE, X2APIC_MSE_REG_SIZE};
        SysRegAddrRange::new(
            SysRegAddr(X2APIC_MSE_REG_BASE),
            SysRegAddr(X2APIC_MSE_REG_BASE + X2APIC_MSE_REG_SIZE),
        )
    }

    fn handle_read(&self, addr: SysRegAddr, width: AccessWidth) -> AxResult<usize> {
        debug!("EmulatedLocalApic::handle_read: addr={addr:?}, width={width:?}");
        let reg_off = x2apic_msr_access_reg(addr);
        self.get_vlapic_regs().handle_read(reg_off, width)
    }

    fn handle_write(&self, addr: SysRegAddr, width: AccessWidth, val: usize) -> AxResult {
        debug!("EmulatedLocalApic::handle_write: addr={addr:?}, width={width:?}, val={val:#x}");
        let reg_off = x2apic_msr_access_reg(addr);
        self.get_mut_vlapic_regs().handle_write(reg_off, val, width)
    }
}
