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

use alloc::{collections::BTreeMap, format, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use ax_cpumask::CpuMask;
use ax_errno::{AxResult, ax_err_type};
use ax_kspin::SpinNoIrq as Mutex;
use axaddrspace::GuestPhysAddr;
use axvcpu::{AxArchVCpu, AxVCpuExitReason, InterruptTriggerMode, VCpuState};
use axvisor_api::{
    host,
    sync::WaitQueue,
    task::{TaskHandle, TaskOptions},
};

use crate::vmm::{VCpuRef, VMRef, sub_running_vm_count};

const KERNEL_STACK_SIZE: usize = 0x40000; // 256 KiB

#[cfg(axvisor_host_riscv64)]
unsafe extern "C" {
    fn axvisor_linux_bridge_handle_irq(vector: usize) -> bool;
}

/// A global map that holds the vCPU task state for each VM.
static VM_VCPU_TASKS: Mutex<BTreeMap<usize, Arc<VMVCpus>>> = Mutex::new(BTreeMap::new());

#[derive(Clone, Copy)]
struct CpuUpRequest {
    target_vcpu_id: usize,
    entry_point: GuestPhysAddr,
    arg: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct PendingInterrupt {
    vector: usize,
    trigger: InterruptTriggerMode,
}

fn get_vm_vcpus(vm_id: usize) -> Option<Arc<VMVCpus>> {
    VM_VCPU_TASKS.lock().get(&vm_id).cloned()
}

/// A structure representing the VCpus of a specific VM, including a wait queue
/// and a list of tasks associated with the VCpus.
pub struct VMVCpus {
    // The ID of the VM to which these VCpus belong.
    _vm_id: usize,
    // A wait queue to manage task scheduling for the VCpus.
    wait_queue: WaitQueue,
    // A map of tasks associated with the VCpus of this VM, keyed by vCPU ID.
    vcpu_task_list: Mutex<BTreeMap<usize, TaskHandle>>,
    vcpu_task_names: Mutex<BTreeMap<usize, alloc::string::String>>,
    // Interrupts queued from host callbacks or other vCPU contexts. They are injected only by the
    // owning vCPU task before entering guest mode, matching the tgoskits x86 runtime model.
    pending_interrupts: Mutex<BTreeMap<usize, Vec<PendingInterrupt>>>,
    pending_cpu_ups: Mutex<Vec<CpuUpRequest>>,
    active_cpu_ups: Mutex<Vec<usize>>,
    cpu_up_wait_queue: WaitQueue,
    cpu_up_worker: Mutex<Option<TaskHandle>>,
    /// The number of currently running or halting VCpus. Used to track when the VM is fully
    /// shutdown.
    ///
    /// This number is incremented when a VCpu starts running and decremented when it exits because
    /// of the VM being shutdown.
    running_halting_vcpu_count: AtomicUsize,
}

impl VMVCpus {
    /// Creates a new `VMVCpus` instance for the given VM.
    ///
    /// # Arguments
    ///
    /// * `vm` - A reference to the VM for which the VCpus are being created.
    ///
    /// # Returns
    ///
    /// A new `VMVCpus` instance with an empty task list and a fresh wait queue.
    fn new(vm: VMRef) -> Self {
        Self {
            _vm_id: vm.id(),
            wait_queue: WaitQueue::new(),
            vcpu_task_list: Mutex::new(BTreeMap::new()),
            vcpu_task_names: Mutex::new(BTreeMap::new()),
            pending_interrupts: Mutex::new(BTreeMap::new()),
            pending_cpu_ups: Mutex::new(Vec::new()),
            active_cpu_ups: Mutex::new(Vec::new()),
            cpu_up_wait_queue: WaitQueue::new(),
            cpu_up_worker: Mutex::new(None),
            running_halting_vcpu_count: AtomicUsize::new(0),
        }
    }

    /// Adds a VCpu task to the list of VCpu tasks for this VM.
    ///
    /// # Arguments
    ///
    /// * `vcpu_task` - A reference to the task associated with a VCpu that is to be added.
    fn add_vcpu_task(
        &self,
        vcpu_id: usize,
        vcpu_task: TaskHandle,
        task_name: alloc::string::String,
    ) {
        self.vcpu_task_list.lock().insert(vcpu_id, vcpu_task);
        self.vcpu_task_names.lock().insert(vcpu_id, task_name);
        self.pending_interrupts.lock().entry(vcpu_id).or_default();
    }

    fn queue_interrupt(&self, vcpu_id: usize, interrupt: PendingInterrupt) -> AxResult {
        if !self.vcpu_task_list.lock().contains_key(&vcpu_id) {
            return Err(ax_err_type!(
                NotFound,
                format!("vCPU {vcpu_id} task not found")
            ));
        }
        self.pending_interrupts
            .lock()
            .entry(vcpu_id)
            .or_default()
            .push(interrupt);
        Ok(())
    }

    fn drain_pending_interrupts(&self, vcpu_id: usize) -> Vec<PendingInterrupt> {
        self.pending_interrupts
            .lock()
            .get_mut(&vcpu_id)
            .map(core::mem::take)
            .unwrap_or_default()
    }

    fn queue_cpu_up(&self, request: CpuUpRequest) -> bool {
        if self
            .vcpu_task_list
            .lock()
            .contains_key(&request.target_vcpu_id)
        {
            return false;
        }

        let mut active = self.active_cpu_ups.lock();
        if active.contains(&request.target_vcpu_id) {
            return false;
        }

        let mut pending = self.pending_cpu_ups.lock();
        if pending
            .iter()
            .any(|queued| queued.target_vcpu_id == request.target_vcpu_id)
        {
            return false;
        }

        active.push(request.target_vcpu_id);
        drop(active);
        pending.push(request);
        drop(pending);
        self.cpu_up_wait_queue.wake_all();
        true
    }

    fn rollback_cpu_up(&self, target_vcpu_id: usize) {
        self.active_cpu_ups
            .lock()
            .retain(|active_vcpu_id| *active_vcpu_id != target_vcpu_id);
    }

    fn drain_cpu_up_requests(&self) -> Vec<CpuUpRequest> {
        core::mem::take(&mut *self.pending_cpu_ups.lock())
    }

    fn has_pending_cpu_up(&self) -> bool {
        !self.pending_cpu_ups.lock().is_empty()
    }

    fn set_cpu_up_worker(&self, task: TaskHandle) {
        *self.cpu_up_worker.lock() = Some(task);
    }

    fn take_cpu_up_worker(&self) -> Option<TaskHandle> {
        self.cpu_up_worker.lock().take()
    }

    fn wake_cpu_up_worker(&self) {
        self.cpu_up_wait_queue.wake_all();
    }

    /// Blocks the current thread on the wait queue associated with the VCpus of this VM.
    fn wait(&self) {
        self.wait_queue.wait()
    }

    /// Blocks the current thread on the wait queue associated with the VCpus of this VM
    /// until the provided condition is met.
    fn wait_until<F>(&self, condition: F)
    where
        F: Fn() -> bool + Send + 'static,
    {
        self.wait_queue.wait_until(condition)
    }

    #[allow(dead_code)]
    fn notify_one(&self) {
        self.wait_queue.wake_one();
    }

    /// Notify all waiting vCPU threads to wake up.
    /// This is useful when shutting down a VM to ensure all vCPUs can check the shutdown flag.
    fn notify_all(&self) {
        self.wait_queue.wake_all();
    }

    /// Increments the count of running or halting VCpus by one.
    fn mark_vcpu_running(&self) {
        self.running_halting_vcpu_count
            .fetch_add(1, Ordering::Relaxed);
        // Relaxed is enough here, as we only need to ensure that the count is incremented and
        // decremented correctly, and there is no other data synchronization needed.
    }

    /// Decrements the count of running or halting VCpus by one. Returns true if this was the last
    /// VCpu to exit.
    fn mark_vcpu_exiting(&self) -> bool {
        self.running_halting_vcpu_count.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |count| count.checked_sub(1),
        ) == Ok(1)
        // Relaxed is enough here, as we only need to ensure that the count is incremented and
        // decremented correctly, and there is no other data synchronization needed.
    }
}

/// Blocks the current thread until it is explicitly woken up, using the wait queue
/// associated with the VCpus of the specified VM.
///
/// # Arguments
///
/// * `vm_id` - The ID of the VM whose VCpu wait queue is used to block the current thread.
fn wait(vm_id: usize) {
    if let Some(vm_vcpus) = get_vm_vcpus(vm_id) {
        info!("vcpus: VM[{vm_id}] entering wait()");
        vm_vcpus.wait();
        info!("vcpus: VM[{vm_id}] woke from wait()");
    } else {
        warn!("VM[{vm_id}] vCPU wait queue not found");
    }
}

/// Blocks the current thread until the provided condition is met, using the wait queue
/// associated with the VCpus of the specified VM.
///
/// # Arguments
///
/// * `vm_id` - The ID of the VM whose VCpu wait queue is used to block the current thread.
/// * `condition` - A closure that returns a boolean value indicating whether the condition is met.
fn wait_for<F>(vm_id: usize, condition: F)
where
    F: Fn() -> bool + Send + 'static,
{
    if let Some(vm_vcpus) = get_vm_vcpus(vm_id) {
        info!("vcpus: VM[{vm_id}] entering wait_for()");
        vm_vcpus.wait_until(condition);
        info!("vcpus: VM[{vm_id}] wait_for() condition satisfied");
    } else {
        warn!("VM[{vm_id}] vCPU wait queue not found");
    }
}

/// Notifies the primary VCpu task associated with the specified VM to wake up and resume execution.
/// This function is used to notify the primary VCpu of a VM to start running after the VM has been booted.
///
/// # Arguments
///
/// * `vm_id` - The ID of the VM whose VCpus are to be notified.
pub(crate) fn notify_primary_vcpu(vm_id: usize) {
    // Generally, the primary VCpu is the first and **only** VCpu in the list.
    if let Some(vm_vcpus) = get_vm_vcpus(vm_id) {
        info!("vcpus: notify_primary_vcpu vm_id={vm_id}");
        vm_vcpus.notify_one();
        info!("vcpus: notify_primary_vcpu vm_id={vm_id} wake_one sent");
    } else {
        warn!("VM[{vm_id}] vCPU resources not found");
    }
}

/// Notifies all VCpu tasks associated with the specified VM to wake up.
/// This is useful when shutting down a VM to ensure all waiting vCPUs can check the shutdown flag.
///
/// # Arguments
///
/// * `vm_id` - The ID of the VM whose VCpus should be notified.
pub(crate) fn notify_all_vcpus(vm_id: usize) {
    if let Some(vm_vcpus) = get_vm_vcpus(vm_id) {
        info!("vcpus: notify_all_vcpus vm_id={vm_id}");
        vm_vcpus.notify_all();
        info!("vcpus: notify_all_vcpus vm_id={vm_id} wake_all sent");
    }
}

/// Wakes all vCPU tasks for every registered VM.
///
/// Linux-host timer callbacks do not carry a VM id. This helper gives the
/// host timer bridge a conservative wakeup edge for vCPUs blocked after HLT.
pub fn notify_all_registered_vcpus() {
    for vm in super::vm_list::get_vm_list() {
        notify_all_vcpus(vm.id());
    }
}

pub(crate) fn queue_interrupt(vm_id: usize, vcpu_id: usize, vector: usize) -> AxResult {
    queue_interrupt_with_trigger(vm_id, vcpu_id, vector, InterruptTriggerMode::EdgeTriggered)
}

pub(crate) fn queue_interrupt_with_trigger(
    vm_id: usize,
    vcpu_id: usize,
    vector: usize,
    trigger: InterruptTriggerMode,
) -> AxResult {
    let vm = super::vm_list::get_vm_by_id(vm_id)
        .ok_or_else(|| ax_err_type!(NotFound, format!("VM[{vm_id}] not found")))?;
    if vm.stopping() || vm.stopped() {
        return Err(ax_err_type!(
            BadState,
            format!("VM[{vm_id}] is not accepting interrupts")
        ));
    }

    let vm_vcpus = get_vm_vcpus(vm_id)
        .ok_or_else(|| ax_err_type!(NotFound, format!("VM[{vm_id}] vCPU resources not found")))?;
    vm_vcpus.queue_interrupt(vcpu_id, PendingInterrupt { vector, trigger })?;
    vm_vcpus.notify_all();
    Ok(())
}

pub(crate) fn inject_pending_interrupts(vm_id: usize, vcpu_id: usize, vcpu: &VCpuRef) {
    let Some(vm_vcpus) = get_vm_vcpus(vm_id) else {
        warn!("VM[{vm_id}] vCPU resources not found, cannot drain VCpu[{vcpu_id}] interrupts");
        return;
    };

    let drained = vm_vcpus.drain_pending_interrupts(vcpu_id);

    for interrupt in drained {
        trace!(
            "Injecting queued interrupt {:#x} into VM[{vm_id}] VCpu[{vcpu_id}]",
            interrupt.vector
        );
        // This function runs inside the owning vCPU task. Calling AxVCpu::inject_* here would
        // re-enter the current-vCPU guard set by AxVCpu::run(), so inject through the arch vCPU
        // directly while preserving the "same physical CPU as the vCPU" requirement.
        let inject_result = vcpu
            .get_arch_vcpu()
            .inject_interrupt_with_trigger(interrupt.vector, interrupt.trigger);
        if let Err(err) = inject_result {
            warn!(
                "Failed to inject queued interrupt {:#x} into VM[{vm_id}] VCpu[{vcpu_id}]: {err:?}",
                interrupt.vector
            );
        }
    }
}

fn set_vcpu_return_value_current_context(vcpu: &VCpuRef, val: usize) {
    // vcpu_run() already executes below AxVCpu::run() in the owning vCPU task. Public AxVCpu
    // helpers re-enter the current-vCPU guard, so update the arch vCPU directly here.
    vcpu.get_arch_vcpu().set_return_value(val);
}

/// Cleans up VCpu resources for a VM that is being deleted.
/// This removes the VM's entry from the global VCpu wait queue.
///
/// # Arguments
///
/// * `vm_id` - The ID of the VM whose VCpu resources should be cleaned up.
///
/// # Note
///
/// This should be called after all VCpu threads have exited to avoid resource leaks.
/// It will join all VCpu tasks to ensure they are fully cleaned up.
#[cfg(feature = "shell")]
pub(crate) fn cleanup_vm_vcpus(vm_id: usize) {
    use alloc::vec::Vec;

    if let Some(vm_vcpus) = VM_VCPU_TASKS.lock().remove(&vm_id) {
        vm_vcpus.wake_cpu_up_worker();
        if let Some(worker) = vm_vcpus.take_cpu_up_worker() {
            axvisor_api::task::join_task(worker);
        }
        // Take task references out before joining so we never block while
        // holding the per-VM task-list lock.
        let tasks: Vec<_> = vm_vcpus
            .vcpu_task_list
            .lock()
            .iter()
            .map(|(&vcpu_id, &task)| {
                let name = vm_vcpus
                    .vcpu_task_names
                    .lock()
                    .get(&vcpu_id)
                    .cloned()
                    .unwrap_or_else(|| alloc::format!("VM[{vm_id}]-VCpu[{vcpu_id}]"));
                (vcpu_id, task, name)
            })
            .collect();
        let task_count = tasks.len();

        info!("VM[{}] Joining {} VCpu tasks...", vm_id, task_count);

        // Join all VCpu tasks to ensure they have fully exited and cleaned up
        for (idx, (_vcpu_id, task, task_name)) in tasks.iter().enumerate() {
            debug!("VM[{}] Joining VCpu task[{}]: {}", vm_id, idx, task_name);
            axvisor_api::task::join_task(*task);
            debug!("VM[{}] VCpu task[{}] exited", vm_id, idx);
        }

        info!(
            "VM[{}] VCpu resources cleaned up, {} VCpu tasks joined successfully",
            vm_id, task_count
        );
    } else {
        warn!("VM[{}] VCpu resources not found in queue", vm_id);
    }
}

/// Marks the VCpu of the specified VM as running.
fn mark_vcpu_running(vm_id: usize) {
    if let Some(vm_vcpus) = get_vm_vcpus(vm_id) {
        vm_vcpus.mark_vcpu_running();
    }
}

/// Marks the VCpu of the specified VM as exiting for VM shutdown. Returns true if this was the last
/// VCpu to exit.
fn mark_vcpu_exiting(vm_id: usize) -> bool {
    get_vm_vcpus(vm_id).is_some_and(|vm_vcpus| vm_vcpus.mark_vcpu_exiting())
}

/// Boot target VCpu on the specified VM.
/// This function is used to boot a secondary VCpu on a VM, setting the entry point and argument for the VCpu.
///
/// # Arguments
///
/// * `vm_id` - The ID of the VM on which the VCpu is to be booted.
/// * `vcpu_id` - The ID of the VCpu to be booted.
/// * `entry_point` - The entry point of the VCpu.
/// * `arg` - The argument to be passed to the VCpu.
fn vcpu_on(vm: VMRef, vcpu_id: usize, entry_point: GuestPhysAddr, arg: usize) -> AxResult {
    let vcpu = vm
        .vcpu_list()
        .get(vcpu_id)
        .cloned()
        .ok_or_else(|| ax_err_type!(NotFound, format!("vCPU {vcpu_id} not found")))?;
    if vcpu.state() != VCpuState::Free {
        return Err(ax_err_type!(
            BadState,
            format!("vCPU {} invalid state {:?}", vcpu.id(), vcpu.state())
        ));
    }

    #[cfg(target_arch = "x86_64")]
    {
        vm.setup_x86_ap_vcpu_entry(&vcpu, entry_point)?;
    }
    #[cfg(not(target_arch = "x86_64"))]
    vcpu.set_entry(entry_point)?;

    #[cfg(not(target_arch = "riscv64"))]
    vcpu.set_gpr(0, arg);

    #[cfg(target_arch = "riscv64")]
    {
        info!(
            "vcpu_on: vcpu[{}] entry={:x} opaque={:x}",
            vcpu_id, entry_point, arg
        );
        vcpu.set_gpr(riscv_vcpu::GprIndex::A0 as usize, vcpu_id);
        vcpu.set_gpr(riscv_vcpu::GprIndex::A1 as usize, arg);
    }

    let (vcpu_task, task_name) = alloc_vcpu_task(&vm, vcpu);

    let vm_vcpus = get_vm_vcpus(vm.id()).ok_or_else(|| {
        ax_err_type!(
            NotFound,
            format!("VM[{}] vCPU resources not found", vm.id())
        )
    })?;
    vm_vcpus.add_vcpu_task(vcpu_id, vcpu_task, task_name);
    Ok(())
}

fn cpu_up_worker_run(vm_id: usize) {
    loop {
        let Some(vm_vcpus) = get_vm_vcpus(vm_id) else {
            return;
        };
        let Some(vm) = super::vm_list::get_vm_by_id(vm_id) else {
            return;
        };
        if vm.stopping() || vm.stopped() {
            return;
        }

        vm_vcpus
            .cpu_up_wait_queue
            .wait_until(move || {
                get_vm_vcpus(vm_id)
                    .map(|vm_vcpus| vm_vcpus.has_pending_cpu_up())
                    .unwrap_or(true)
                    || super::vm_list::get_vm_by_id(vm_id)
                        .map(|vm| vm.stopping() || vm.stopped())
                        .unwrap_or(true)
            });

        let Some(vm_vcpus) = get_vm_vcpus(vm_id) else {
            return;
        };
        let Some(vm) = super::vm_list::get_vm_by_id(vm_id) else {
            return;
        };
        if vm.stopping() || vm.stopped() {
            return;
        }

        let requests = vm_vcpus.drain_cpu_up_requests();
        for request in requests {
            if let Err(err) = vcpu_on(
                vm.clone(),
                request.target_vcpu_id,
                request.entry_point,
                request.arg,
            ) {
                vm_vcpus.rollback_cpu_up(request.target_vcpu_id);
                warn!(
                    "Failed to boot VM[{vm_id}] VCpu[{}] from cpu_up_worker: {err:?}",
                    request.target_vcpu_id
                );
            }
        }
    }
}

/// Sets up the primary VCpu for the given VM,
/// generally the first VCpu in the VCpu list,
/// and initializing their respective wait queues and task lists.
/// VM's secondary VCpus are not started at this point.
///
/// # Arguments
///
/// * `vm` - A reference to the VM for which the VCpus are being set up.
pub fn setup_vm_primary_vcpu(vm: VMRef) {
    info!("Initializing VM[{}]'s {} vcpus", vm.id(), vm.vcpu_num());
    let vm_id = vm.id();
    if get_vm_vcpus(vm_id).is_some() {
        debug!("VM[{vm_id}] vCPU resources already exist");
        return;
    }
    let vm_vcpus = Arc::new(VMVCpus::new(vm.clone()));

    let primary_vcpu_id = 0;

    let Some(primary_vcpu) = vm.vcpu_list().get(primary_vcpu_id).cloned() else {
        warn!("VM[{vm_id}] has no primary vCPU");
        return;
    };
    VM_VCPU_TASKS.lock().insert(vm_id, vm_vcpus);
    let (primary_vcpu_task, task_name) = alloc_vcpu_task(&vm, primary_vcpu);
    let vm_vcpus = get_vm_vcpus(vm_id).expect("vCPU resources must exist after registration");
    vm_vcpus.add_vcpu_task(0, primary_vcpu_task, task_name);
    let worker_task = axvisor_api::task::spawn_task(
        TaskOptions {
            name: alloc::format!("VM[{vm_id}]-CpuUpWorker"),
            stack_size: KERNEL_STACK_SIZE,
            cpu_set: None,
        },
        move || cpu_up_worker_run(vm_id),
    );
    vm_vcpus.set_cpu_up_worker(worker_task);
}

/// Finds the task associated with the specified vCPU of the specified VM.
// pub fn find_vcpu_task(vm_id: usize, vcpu_id: usize) -> Option<AxTaskRef> {
//     with_vcpu_task(vm_id, vcpu_id, |task| task.clone())
// }
/// Executes the provided closure with the task associated with the specified vCPU of the specified VM.
pub fn with_vcpu_task<T, F: FnOnce(&TaskHandle) -> T>(
    vm_id: usize,
    vcpu_id: usize,
    f: F,
) -> Option<T> {
    get_vm_vcpus(vm_id)?
        .vcpu_task_list
        .lock()
        .get(&vcpu_id)
        .map(f)
}

/// Allocates arceos task for vcpu, set the task's entry function to [`vcpu_run()`],
/// also initializes the CPU mask if the VCpu has a dedicated physical CPU set.
///
/// # Arguments
///
/// * `vm` - A reference to the VM for which the VCpu task is being allocated.
/// * `vcpu` - A reference to the VCpu for which the task is being allocated.
///
/// # Returns
///
/// A reference to the task that has been allocated for the VCpu.
///
/// # Note
///
/// * The task associated with the VCpu is created with a kernel stack size of 256 KiB.
/// * The task is created in blocked state and added to the wait queue directly,
///   instead of being added to the ready queue. It will be woken up by notify_primary_vcpu().
fn alloc_vcpu_task(vm: &VMRef, vcpu: VCpuRef) -> (TaskHandle, alloc::string::String) {
    info!("Spawning task for VM[{}] VCpu[{}]", vm.id(), vcpu.id());
    let vm_id = vm.id();
    let vcpu_id = vcpu.id();
    let task_name = alloc::format!("VM[{vm_id}]-VCpu[{vcpu_id}]");
    let task = axvisor_api::task::spawn_task(
        TaskOptions {
            name: task_name.clone(),
            stack_size: KERNEL_STACK_SIZE,
            cpu_set: vcpu.phys_cpu_set(),
        },
        move || vcpu_run(vm_id, vcpu_id),
    );
    info!("VCpu task {} created", task_name);
    (task, task_name)
}

/// The main routine for VCpu task.
/// This function is the entry point for the VCpu tasks, which are spawned for each VCpu of a VM.
///
/// When the VCpu first starts running, it waits for the VM to be in the running state.
/// It then enters a loop where it runs the VCpu and handles the various exit reasons.
fn vcpu_run(vm_id: usize, vcpu_id: usize) {
    host::init_percpu();
    let _context_guard = crate::context::bind_current_vcpu_context(vm_id, vcpu_id);

    let (vm, vcpu) = super::with_vm_and_vcpu(vm_id, vcpu_id, |vm, vcpu| (vm, vcpu))
        .expect("current vCPU task is not bound to a live VM/vCPU");

    info!("VM[{}] VCpu[{}] waiting for running", vm.id(), vcpu.id());
    let vm_for_wait = vm.clone();
    wait_for(vm_id, move || vm_for_wait.running());

    info!("VM[{}] VCpu[{}] running...", vm.id(), vcpu.id());
    #[cfg(target_arch = "x86_64")]
    super::devices::x86::enable_ioapic_irq_forwarding(&vm, &vcpu);
    mark_vcpu_running(vm_id);

    loop {
        inject_pending_interrupts(vm_id, vcpu_id, &vcpu);

        #[cfg(target_arch = "x86_64")]
        super::devices::x86::drain_pending_ioapic_irqs(&vm, &vcpu);

        match vm.run_vcpu(vcpu_id) {
            Ok(exit_reason) => {
                match exit_reason {
                AxVCpuExitReason::Hypercall { nr, args } => {
                    debug!("Hypercall [{nr}] args {args:x?}");
                    use crate::vmm::hvc::HyperCall;

                    match HyperCall::new(vm.clone(), nr, args) {
                        Ok(hypercall) => {
                            let ret_val = match hypercall.execute() {
                                Ok(ret_val) => ret_val as isize,
                                Err(err) => {
                                    warn!("Hypercall [{nr:#x}] failed: {err:?}");
                                    -1
                                }
                            };
                            set_vcpu_return_value_current_context(&vcpu, ret_val as usize);
                        }
                        Err(err) => {
                            warn!("Hypercall [{nr:#x}] failed: {err:?}");
                        }
                    }
                }
                AxVCpuExitReason::FailEntry {
                    hardware_entry_failure_reason,
                } => {
                    warn!(
                        "VM[{vm_id}] VCpu[{vcpu_id}] run failed with exit code \
                         {hardware_entry_failure_reason}"
                    );
                }
                AxVCpuExitReason::ExternalInterrupt { vector } => {
                    debug!("VM[{vm_id}] run VCpu[{vcpu_id}] get irq {vector}");

                    // TODO: maybe move this irq dispatcher to lower layer to accelerate the interrupt handling
                    #[cfg(axvisor_host_riscv64)]
                    let linux_irq_handled =
                        unsafe { axvisor_linux_bridge_handle_irq(vector as usize) };
                    #[cfg(all(not(axvisor_host_riscv64), not(target_arch = "x86_64")))]
                    let linux_irq_handled = {
                        axvisor_api::irq::handle_irq(vector as usize);
                        true
                    };
                    #[cfg(target_arch = "x86_64")]
                    let linux_irq_handled = false;
                    super::timer::check_events();
                    #[cfg(target_arch = "x86_64")]
                    super::devices::x86::forward_passthrough_irq_from_vmexit(
                        &vm,
                        &vcpu,
                        vector as usize,
                    );
                    #[cfg(target_arch = "x86_64")]
                    super::devices::x86::inject_pending_serial_irq(&vm, &vcpu);
                    #[cfg(target_arch = "riscv64")]
                    {
                        if linux_irq_handled {
                            vcpu.get_arch_vcpu().latch_hvip_from_hw();
                        }
                    }
                }
                AxVCpuExitReason::PreemptionTimer => {
                    super::timer::check_events();
                    #[cfg(target_arch = "x86_64")]
                    super::devices::x86::inject_due_pit_irq0(&vm, &vcpu);
                    #[cfg(target_arch = "x86_64")]
                    super::devices::x86::inject_pending_serial_irq(&vm, &vcpu);
                }
                AxVCpuExitReason::InterruptEnd { vector: _vector } => {
                    #[cfg(target_arch = "x86_64")]
                    if let Some(vector) = _vector {
                        super::devices::x86::inject_pending_ioapic_irq_after_eoi(
                            &vm, &vcpu, vector,
                        );
                    }
                }
                AxVCpuExitReason::Halt => {
                    debug!("VM[{vm_id}] run VCpu[{vcpu_id}] Halt");
                    // The VMX preemption timer is unreliable under nested
                    // virtualization (on QEMU/KVM L0 the pin-based control bit
                    // is silently dropped, so it never counts down and never
                    // forces a periodic exit). Without it, an idle guest that
                    // sits in `sti; hlt` gets no periodic wake-up to advance its
                    // PIT/LAPIC ticks, so jiffies freeze. Drive the timers off
                    // the host wall clock here instead: service the software
                    // timer list and re-check the PIT/serial/IOAPIC sources on
                    // every halt so a due tick is always queued for the next
                    // guest entry, independent of the preemption timer.
                    #[cfg(target_arch = "x86_64")]
                    super::timer::check_events();
                    #[cfg(target_arch = "x86_64")]
                    super::devices::x86::inject_due_pit_irq0(&vm, &vcpu);
                    #[cfg(target_arch = "x86_64")]
                    super::devices::x86::inject_pending_serial_irq(&vm, &vcpu);
                    #[cfg(target_arch = "x86_64")]
                    super::devices::x86::drain_pending_ioapic_irqs(&vm, &vcpu);
                    // Arm the host hrtimer at the next PIT deadline. The software
                    // timer list is empty once the guest disables its LAPIC timer,
                    // so check_events()/rearm_host_timer arms nothing; without this
                    // the idle vCPU only re-enters via the low-priority yield loop
                    // and jiffies freeze. Programming the PIT deadline gives the
                    // host a concrete wake-up so the ~18ms tick cadence resumes.
                    #[cfg(target_arch = "x86_64")]
                    {
                        let deadline_ns = super::devices::x86::arm_idle_wakeup_timer(&vm);
                        match deadline_ns {
                            // Block until the armed PIT deadline elapses. Using a
                            // time-based condition (rather than a bare wait()) makes
                            // this immune to lost wake-ups: even if the hrtimer's
                            // notify_all fires before we sleep, the condition is
                            // re-evaluated against the host clock on entry and each
                            // wake, so we never block past the deadline. The hrtimer
                            // callback's notify_all_registered_vcpus() delivers the
                            // wake; the time check is the correctness backstop.
                            Some(deadline_ns) => {
                                wait_for(vm_id, move || {
                                    axvisor_api::time::current_time_nanos() >= deadline_ns
                                });
                            }
                            // No armed PIT period (e.g. before the guest programs
                            // channel 0). Fall back to yielding so we re-poll soon
                            // without busy-spinning the host CPU.
                            None => {
                                axvisor_api::task::yield_now();
                                continue;
                            }
                        }
                    }
                    // Non-x86 hosts rely on the architectural timer to wake the
                    // idle vCPU, so a plain blocking wait is sufficient.
                    #[cfg(not(target_arch = "x86_64"))]
                    wait(vm_id)
                }
                AxVCpuExitReason::Nothing => {
                    axvisor_api::task::yield_now();
                }
                AxVCpuExitReason::NestedPageFault { addr, access_flags } => {
                    warn!(
                        "VM[{vm_id}] VCpu[{vcpu_id}] unhandled nested page fault: gpa={:#x} flags={access_flags:?}",
                        addr.as_usize()
                    );
                    if let Err(err) = vm.shutdown() {
                        warn!("VM[{vm_id}] shutdown failed after nested page fault: {err:?}");
                    }
                    notify_all_vcpus(vm_id);
                }
                AxVCpuExitReason::MmioRead {
                    addr,
                    width,
                    reg,
                    reg_width,
                    signed_ext,
                } => {
                    warn!(
                        "VM[{vm_id}] VCpu[{vcpu_id}] unhandled MMIO read: gpa={:#x} width={width:?} reg={reg} reg_width={reg_width:?} signed_ext={signed_ext}",
                        addr.as_usize()
                    );
                }
                AxVCpuExitReason::MmioWrite { addr, width, data } => {
                    warn!(
                        "VM[{vm_id}] VCpu[{vcpu_id}] unhandled MMIO write: gpa={:#x} width={width:?} data={data:#x}",
                        addr.as_usize()
                    );
                }
                AxVCpuExitReason::CpuDown { _state } => {
                    warn!("VM[{vm_id}] run VCpu[{vcpu_id}] CpuDown state {_state:#x}");
                    wait(vm_id)
                }
                AxVCpuExitReason::CpuUp {
                    target_cpu,
                    entry_point,
                    arg,
                } => {
                    info!(
                        "VM[{vm_id}]'s VCpu[{vcpu_id}] try to boot target_cpu [{target_cpu}] \
                         entry_point={entry_point:x} arg={arg:#x}"
                    );

                    // Get the mapping relationship between all vCPUs and physical CPUs from the configuration
                    let vcpu_mappings = vm.get_vcpu_affinities_pcpu_ids();

                    // Find the vCPU ID corresponding to the physical ID
                    let Some(target_vcpu_id) =
                        vcpu_mappings.iter().find_map(|(vcpu_id, _, phys_id)| {
                            (*phys_id == target_cpu as usize).then_some(*vcpu_id)
                        })
                    else {
                        warn!("Physical CPU ID {target_cpu} not found in VM configuration");
                        set_vcpu_return_value_current_context(&vcpu, usize::MAX);
                        continue;
                    };

                    #[cfg(target_arch = "x86_64")]
                    {
                        // x86 SIPI handling is latency-sensitive: Linux waits only briefly for
                        // the AP to report alive. Create the AP vCPU task in the VM-exit path so
                        // a crowded two-host-CPU run does not depend on a separate helper kthread
                        // being scheduled before the guest's CPU-up timeout expires.
                        if with_vcpu_task(vm_id, target_vcpu_id, |_| ()).is_some() {
                            set_vcpu_return_value_current_context(&vcpu, 0);
                        } else {
                            match vcpu_on(vm.clone(), target_vcpu_id, entry_point, arg as _) {
                                Ok(()) => {
                                    set_vcpu_return_value_current_context(&vcpu, 0);
                                    axvisor_api::task::yield_now();
                                }
                                Err(err) => {
                                    warn!(
                                        "Failed to boot VM[{vm_id}] VCpu[{target_vcpu_id}] from CpuUp: {err:?}"
                                    );
                                    set_vcpu_return_value_current_context(&vcpu, usize::MAX);
                                }
                            }
                        }
                    }
                    #[cfg(not(target_arch = "x86_64"))]
                    {
                        match vcpu_on(vm.clone(), target_vcpu_id, entry_point, arg as _) {
                            Ok(()) => {
                                #[cfg(not(target_arch = "riscv64"))]
                                vcpu.set_gpr(0, 0);
                                #[cfg(target_arch = "riscv64")]
                                vcpu.set_gpr(riscv_vcpu::GprIndex::A0 as usize, 0);
                            }
                            Err(err) => {
                                warn!("Failed to boot VM[{vm_id}] VCpu[{target_vcpu_id}]: {err:?}");
                                set_vcpu_return_value_current_context(&vcpu, usize::MAX);
                            }
                        }
                    }
                }
                AxVCpuExitReason::SystemDown => {
                    warn!("VM[{vm_id}] run VCpu[{vcpu_id}] SystemDown");
                    if let Err(err) = vm.shutdown() {
                        warn!("VM[{vm_id}] shutdown failed: {err:?}");
                    }
                    // Notify all vCPUs to wake up to check the shutdown flag
                    notify_all_vcpus(vm_id);
                }
                AxVCpuExitReason::SendIPI {
                    target_cpu,
                    target_cpu_aux,
                    send_to_all,
                    send_to_self,
                    vector,
                } => {
                    debug!(
                        "VM[{vm_id}] run VCpu[{vcpu_id}] SendIPI, target_cpu={target_cpu:#x}, \
                         target_cpu_aux={target_cpu_aux:#x}, vector={vector}",
                    );
                    if send_to_all {
                        warn!("Send IPI to all CPUs is not implemented yet");
                        continue;
                    }

                    if target_cpu == vcpu_id as u64 || send_to_self {
                        // Self-IPI is handled in the current vCPU task context; avoid the public
                        // AxVCpu injection wrapper because this branch is already below run().
                        if let Err(err) = vcpu.get_arch_vcpu().inject_interrupt(vector as _) {
                            warn!(
                                "Failed to inject interrupt {vector} to current VM[{vm_id}] \
                                 VCpu[{vcpu_id}]: {err:?}"
                            );
                        }
                    } else if let Err(err) =
                        vm.inject_interrupt_to_vcpu(CpuMask::one_shot(target_cpu as _), vector as _)
                    {
                        warn!(
                            "Failed to inject interrupt {vector} to VM[{vm_id}] CPU {target_cpu}: \
                             {err:?}"
                        );
                    }
                }
                e => {
                    warn!("VM[{vm_id}] run VCpu[{vcpu_id}] unhandled vmexit: {e:?}");
                }
            }
            }
            Err(err) => {
                error!(
                    "vcpus: VM[{vm_id}] VCpu[{vcpu_id}] vm.run_vcpu returned error: {err:?}"
                );
                if let Err(err) = vm.shutdown() {
                    warn!("VM[{vm_id}] shutdown failed after vCPU error: {err:?}");
                }
                // Notify all vCPUs to wake up to check the shutdown flag
                notify_all_vcpus(vm_id);
            }
        }

        // Check if the VM is suspended
        if vm.suspending() {
            debug!(
                "VM[{}] VCpu[{}] is suspended, waiting for resume...",
                vm_id, vcpu_id
            );
            let vm_for_wait = vm.clone();
            wait_for(vm_id, move || !vm_for_wait.suspending());
            info!("VM[{}] VCpu[{}] resumed from suspend", vm_id, vcpu_id);
            continue;
        }

        // Check if the VM is stopping.
        if vm.stopping() {
            warn!(
                "VM[{}] VCpu[{}] stopping because of VM stopping",
                vm_id, vcpu_id
            );

            if mark_vcpu_exiting(vm_id) {
                info!("VM[{vm_id}] VCpu[{vcpu_id}] last VCpu exiting, decreasing running VM count");

                // Transition from Stopping to Stopped
                vm.set_vm_status(axvm::VMStatus::Stopped);
                info!("VM[{}] state changed to Stopped", vm_id);

                #[cfg(target_arch = "x86_64")]
                super::devices::x86::disable_ioapic_irq_forwarding_for_vm(vm_id);

                sub_running_vm_count(1);
                super::VMM.wake_one();
            }

            break;
        }
    }

    info!("VM[{}] VCpu[{}] exiting...", vm_id, vcpu_id);
}
