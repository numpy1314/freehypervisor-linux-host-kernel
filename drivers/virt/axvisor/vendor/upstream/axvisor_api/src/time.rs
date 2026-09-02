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

//! Host time APIs for the AxVisor hypervisor.
//!
//! This module provides host monotonic time measurement and host timer
//! programming APIs, which are essential for implementing virtual timers and
//! time-related virtualization features.
//!
//! # Overview
//!
//! The time APIs provide:
//! - Current monotonic time queries
//! - Host one-shot timer programming
//!
//! # Types
//!
//! - [`TimeValue`] - A time value represented as [`Duration`].
//! - [`Nanos`] - Nanoseconds count (u64).
//! # Helper Functions
//!
//! In addition to the core API trait, this module provides helper functions:
//! - [`current_time`] - Get the current time as a [`TimeValue`].
//!
//! # Implementation
//!
//! To implement these APIs, use the [`api_impl`](crate::api_impl) attribute
//! macro on an impl block:
//!
//! ```rust,ignore
//! struct TimeIfImpl;
//!
//! #[axvisor_api::api_impl]
//! impl axvisor_api::time::TimeIf for TimeIfImpl {
//!     fn current_time_nanos() -> Nanos {
//!         // Read the host monotonic clock
//!     }
//!     // ... implement other functions
//! }
//! ```

extern crate alloc;

use alloc::boxed::Box;
use core::time::Duration;

use crate::vmm::{self, CancelToken, InterruptVector, VCpuId, VMId};

/// Time value type.
///
/// Represents a point in time or a duration as a [`Duration`].
pub type TimeValue = Duration;

/// Nanoseconds count type.
///
/// Used for high-precision time measurements in nanoseconds.
pub type Nanos = u64;

/// Timer action used by legacy virtual timer users.
pub enum TimerAction {
    /// Inject a virtual interrupt when the timer expires.
    InjectInterrupt {
        /// Target VM id.
        vm_id: VMId,
        /// Target vCPU id.
        vcpu_id: VCpuId,
        /// Interrupt vector.
        vector: InterruptVector,
    },
}

/// The API trait for host time functionalities.
///
/// This trait defines the host time interface required by the hypervisor.
/// Implementations should be provided by the host system or HAL layer.
#[crate::api_def]
pub trait TimeIf {
    /// Get the current host monotonic time in nanoseconds.
    fn current_time_nanos() -> Nanos;

    /// Program the host one-shot timer to fire at `deadline`.
    ///
    /// The deadline is expressed in the same monotonic time domain as
    /// [`current_time_nanos`].
    fn set_oneshot_timer(deadline: TimeValue);
}

/// Get the current time as a [`TimeValue`].
///
/// This is a convenience function that returns the current time as a
/// [`Duration`].
///
/// # Returns
///
/// The current time as a [`TimeValue`] (Duration).
pub fn current_time() -> TimeValue {
    Duration::from_nanos(current_time_nanos())
}

/// Return the current virtual timer tick.
///
/// The compatibility façade uses one nanosecond as one tick. This preserves the
/// absolute deadline semantics required by the LAPIC timer while avoiding an
/// extra host-specific tick frequency contract in `axvisor_api`.
pub fn current_ticks() -> u64 {
    current_time_nanos()
}

/// Convert virtual timer ticks to nanoseconds.
pub const fn ticks_to_nanos(ticks: u64) -> u64 {
    ticks
}

/// Convert nanoseconds to virtual timer ticks.
pub const fn nanos_to_ticks(nanos: u64) -> u64 {
    nanos
}

/// Convert virtual timer ticks to a time value.
pub fn ticks_to_time(ticks: u64) -> TimeValue {
    Duration::from_nanos(ticks_to_nanos(ticks))
}

/// Register a VMM timer from a legacy action value.
pub fn register_timer(deadline: TimeValue, action: TimerAction) -> CancelToken {
    let callback: Box<dyn FnOnce(TimeValue) + Send + 'static> = match action {
        TimerAction::InjectInterrupt {
            vm_id,
            vcpu_id,
            vector,
        } => Box::new(move |_| vmm::inject_interrupt(vm_id, vcpu_id, vector)),
    };

    vmm::register_timer(deadline, callback)
}

/// Cancel a previously registered VMM timer.
pub fn cancel_timer(token: CancelToken) {
    vmm::cancel_timer(token);
}
