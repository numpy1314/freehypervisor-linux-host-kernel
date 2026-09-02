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

use alloc::sync::Arc;
use core::alloc::Layout;

use ax_errno::{AxResult, ax_err_type};
use axaddrspace::GuestPhysAddr;
use axvm::{
    VMMemoryRegion,
    config::{
        AxVMConfig, AxVMCrateConfig, VMBootProtocol, VmMemMappingType, adjusted_kernel_load_gpa,
    },
};

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "loongarch64",
    target_arch = "riscv64"
))]
use crate::vmm::fdt::*;
use crate::vmm::{VM, images::ImageLoader, vm_list::push_vm};

#[allow(dead_code)]
pub mod vmcfg {
    use alloc::{string::String, vec::Vec};

    /// Default static VM configs. Used when no VM config is provided.
    pub fn default_static_vm_configs() -> Vec<&'static str> {
        vec![]
    }

    /// Read VM configs from filesystem
    #[cfg(feature = "fs")]
    pub fn filesystem_vm_configs() -> Vec<String> {
        use axvisor_api::fs as host_fs;

        let config_dir = "/guest/vm_default";

        let mut configs = Vec::new();

        crate::println!("filesystem_vm_configs: enter dir={}", config_dir);
        debug!("Read VM config files from filesystem.");

        let entries = match host_fs::read_dir(config_dir) {
            Ok(entries) => {
                crate::println!("filesystem_vm_configs: read_dir ok dir={}", config_dir);
                info!("Find dir: {}", config_dir);
                entries
            }
            Err(_e) => {
                crate::println!("filesystem_vm_configs: read_dir miss dir={}", config_dir);
                info!("NOT find dir: {} in filesystem", config_dir);
                return configs;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    crate::println!("filesystem_vm_configs: entry err");
                    warn!("Failed to read config directory entry: {e:?}");
                    continue;
                }
            };
            let path_str = entry.path();
            crate::println!("filesystem_vm_configs: consider path={}", path_str);
            debug!("Considering file: {}", path_str);
            if path_str.ends_with(".toml") {
                crate::println!("filesystem_vm_configs: reading toml path={}", path_str);
                let content = match host_fs::read_to_string(path_str) {
                    Ok(content) => content,
                    Err(e) => {
                        crate::println!("filesystem_vm_configs: read_to_string err path={}", path_str);
                        error!("Failed to read config file {}: {:?}", path_str, e);
                        continue;
                    }
                };
                let file_size = content.len();
                crate::println!(
                    "filesystem_vm_configs: read_to_string ok path={} size={}",
                    path_str,
                    file_size
                );

                info!("File {} size: {}", path_str, file_size);

                if file_size == 0 {
                    warn!("File {} is empty", path_str);
                    continue;
                }

                debug!(
                    "Successfully read config file {} as UTF-8, size: {}",
                    path_str, file_size
                );

                match axvm::config::AxVMCrateConfig::from_toml(&content) {
                    Ok(_) => {
                        crate::println!("filesystem_vm_configs: toml valid path={}", path_str);
                        configs.push(content);
                        info!(
                            "TOML config: {} is valid, start the virtual machine directly now. ",
                            path_str
                        );
                    }
                    Err(e) => {
                        crate::println!("filesystem_vm_configs: toml invalid path={}", path_str);
                        warn!(
                            "File {} does not contain a valid VM config: {:?}",
                            path_str, e
                        );
                    }
                }
            }
        }

        crate::println!("filesystem_vm_configs: leave count={}", configs.len());
        configs
    }

    /// Fallback function for when "fs" feature is not enabled
    #[cfg(not(feature = "fs"))]
    pub fn filesystem_vm_configs() -> Vec<String> {
        Vec::new()
    }

    include!("../vm_configs_static.rs");
}

pub fn get_vm_dtb_arc(_vm_cfg: &AxVMConfig) -> Option<Arc<[u8]>> {
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "loongarch64",
        target_arch = "riscv64"
    ))]
    {
        let cache_lock = dtb_cache().lock();
        if let Some(dtb) = cache_lock.get(&_vm_cfg.id()) {
            return Some(Arc::from(dtb.as_slice()));
        }
    }
    None
}

pub fn init_guest_vms() {
    crate::println!("init_guest_vms: enter");
    info!("init_guest_vms: enter");
    // Initialize the DTB cache in the fdt module
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "loongarch64",
        target_arch = "riscv64"
    ))]
    {
        crate::println!("init_guest_vms: before init_dtb_cache");
        init_dtb_cache();
        crate::println!("init_guest_vms: after init_dtb_cache");
    }

    // First try to get configs from filesystem if fs feature is enabled
    crate::println!("init_guest_vms: before filesystem_vm_configs");
    let mut gvm_raw_configs = vmcfg::filesystem_vm_configs();
    crate::println!(
        "init_guest_vms: after filesystem_vm_configs count={}",
        gvm_raw_configs.len()
    );
    info!(
        "init_guest_vms: filesystem configs count={}",
        gvm_raw_configs.len()
    );

    // If no filesystem configs found, fallback to static configs
    if gvm_raw_configs.is_empty() {
        let static_configs = vmcfg::static_vm_configs();
        if static_configs.is_empty() {
            info!("Static VM configs are empty.");
            info!("Now axvisor will entry the shell...");
        } else {
            info!("Using static VM configs.");
        }
        // Convert static configs to String type
        gvm_raw_configs.extend(static_configs.into_iter().map(|s| s.into()));
    }

    for raw_cfg_str in gvm_raw_configs {
        crate::println!("init_guest_vms: before init_guest_vm raw_cfg_bytes={}", raw_cfg_str.len());
        debug!("Initializing guest VM with config: {:#?}", raw_cfg_str);
        if let Err(e) = init_guest_vm(&raw_cfg_str) {
            crate::println!("init_guest_vms: init_guest_vm failed err={:?}", e);
            error!("Failed to initialize guest VM: {e:?}");
        }
        crate::println!("init_guest_vms: after init_guest_vm");
    }
    crate::println!("init_guest_vms: leave");
    info!("init_guest_vms: leave");
}

pub fn init_guest_vm(raw_cfg: &str) -> AxResult<usize> {
    crate::println!("init_guest_vm: begin raw_cfg_bytes={}", raw_cfg.len());
    info!("init_guest_vm: begin raw_cfg_bytes={}", raw_cfg.len());
    #[allow(unused_mut)]
    let mut vm_create_config = AxVMCrateConfig::from_toml(raw_cfg).map_err(|e| {
        error!("init_guest_vm: AxVMCrateConfig::from_toml failed: {:?}", e);
        ax_err_type!(InvalidData, format!("Failed to resolve VM config: {e:?}"))
    })?;
    info!(
        "init_guest_vm: parsed config vm_id={} name={} mem_regions={}",
        vm_create_config.base.id,
        vm_create_config.base.name,
        vm_create_config.kernel.memory_regions.len()
    );

    if let Some(linux) = super::images::get_image_header(&vm_create_config) {
        debug!(
            "VM[{}] Linux header: {:#x?}",
            vm_create_config.base.id, linux
        );
    }

    #[allow(unused_mut)]
    let mut vm_config = AxVMConfig::from(vm_create_config.clone());

    // Handle FDT-related operations for architectures that boot guests with DTB.
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "loongarch64",
        target_arch = "riscv64"
    ))]
    crate::println!("init_guest_vm: before handle_fdt_operations vm_id={}", vm_config.id());
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "loongarch64",
        target_arch = "riscv64"
    ))]
    info!("init_guest_vm: before handle_fdt_operations vm_id={}", vm_config.id());
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "loongarch64",
        target_arch = "riscv64"
    ))]
    handle_fdt_operations(&mut vm_config, &mut vm_create_config).map_err(|e| {
        error!("init_guest_vm: handle_fdt_operations failed: {:?}", e);
        e
    })?;
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "loongarch64",
        target_arch = "riscv64"
    ))]
    crate::println!("init_guest_vm: after handle_fdt_operations vm_id={}", vm_config.id());
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "loongarch64",
        target_arch = "riscv64"
    ))]
    info!("init_guest_vm: after handle_fdt_operations vm_id={}", vm_config.id());

    #[cfg(target_arch = "x86_64")]
    let skip_guest_address_adjustment = x86_linux_direct_boot_config(&vm_create_config);
    #[cfg(not(target_arch = "x86_64"))]
    let skip_guest_address_adjustment = false;

    // info!("after parse_vm_interrupt, crate VM[{}] with config: {:#?}", vm_config.id(), vm_config);
    info!("Creating VM[{}] {:?}", vm_config.id(), vm_config.name());

    // Create VM.
    crate::println!("init_guest_vm: before VM::new vm_id={}", vm_config.id());
    info!("init_guest_vm: before VM::new vm_id={}", vm_config.id());
    let vm = VM::new(vm_config).map_err(|e| {
        error!("init_guest_vm: VM::new failed: {:?}", e);
        ax_err_type!(InvalidData, format!("Failed to create VM: {e:?}"))
    })?;
    let vm_id = vm.id();
    crate::println!("init_guest_vm: after VM::new vm_id={}", vm_id);
    info!("init_guest_vm: after VM::new vm_id={}", vm_id);

    crate::println!("init_guest_vm: before vm_alloc_memory_regions vm_id={}", vm_id);
    info!("init_guest_vm: before vm_alloc_memory_regions vm_id={}", vm_id);
    vm_alloc_memory_regions(&vm_create_config, &vm).map_err(|e| {
        error!("init_guest_vm: vm_alloc_memory_regions failed: {:?}", e);
        e
    })?;
    crate::println!("init_guest_vm: after vm_alloc_memory_regions vm_id={}", vm_id);
    info!("init_guest_vm: after vm_alloc_memory_regions vm_id={}", vm_id);

    let main_mem = vm
        .memory_regions()
        .first()
        .cloned()
        .ok_or_else(|| ax_err_type!(InvalidData, "VM must have at least one memory region"))?;
    info!(
        "init_guest_vm: selected main_mem vm_id={} start={:#x} size={:#x}",
        vm_id,
        main_mem.gpa.as_usize(),
        main_mem.size()
    );

    if !skip_guest_address_adjustment {
        crate::println!("init_guest_vm: before config_guest_address vm_id={}", vm_id);
        info!("init_guest_vm: before config_guest_address vm_id={}", vm_id);
        config_guest_address(
            &vm,
            &main_mem,
            vm_create_config.kernel.effective_boot_protocol(),
        );
        crate::println!("init_guest_vm: after config_guest_address vm_id={}", vm_id);
        info!("init_guest_vm: after config_guest_address vm_id={}", vm_id);
    }

    // Load corresponding images for VM.
    info!("VM[{}] created success, loading images...", vm.id());

    let mut loader = ImageLoader::new(main_mem, vm_create_config, vm.clone());
    crate::println!("init_guest_vm: before loader.load vm_id={}", vm_id);
    info!("init_guest_vm: before loader.load vm_id={}", vm_id);
    loader.load().map_err(|e| {
        error!("init_guest_vm: ImageLoader::load failed: {:?}", e);
        e
    })?;
    crate::println!("init_guest_vm: after loader.load vm_id={}", vm_id);
    info!("init_guest_vm: after loader.load vm_id={}", vm_id);

    crate::println!("init_guest_vm: before vm.init vm_id={}", vm_id);
    info!("init_guest_vm: before vm.init vm_id={}", vm_id);
    vm.init().map_err(|e| {
        error!("init_guest_vm: vm.init failed for VM[{}]: {:?}", vm.id(), e);
        ax_err_type!(InvalidData, format!("VM[{}] setup failed: {e:?}", vm.id()))
    })?;
    crate::println!("init_guest_vm: after vm.init vm_id={}", vm_id);
    info!("init_guest_vm: after vm.init vm_id={}", vm_id);

    vm.set_vm_status(axvm::VMStatus::Loaded);
    crate::println!("init_guest_vm: set status Loaded vm_id={}", vm_id);
    info!("init_guest_vm: set status Loaded vm_id={}", vm_id);
    push_vm(vm);
    crate::println!("init_guest_vm: pushed vm_id={} to vm_list", vm_id);
    info!("init_guest_vm: pushed vm_id={} to vm_list", vm_id);

    Ok(vm_id)
}

#[cfg(target_arch = "x86_64")]
fn config_guest_address(vm: &VM, main_memory: &VMMemoryRegion, boot_protocol: VMBootProtocol) {
    vm.with_config(|config| {
        if let Some(kernel_addr) = adjusted_kernel_load_gpa(
            main_memory,
            boot_protocol,
            config.image_config.bios_load_gpa,
        ) {
            debug!(
                "Adjusting kernel load address from {:#x} to {:#x}",
                config.image_config.kernel_load_gpa, kernel_addr
            );
            config.relocate_kernel_image(kernel_addr);
        }
    });
}

#[cfg(not(target_arch = "x86_64"))]
fn config_guest_address(_vm: &VM, _main_memory: &VMMemoryRegion, _boot_protocol: VMBootProtocol) {
    // Non-x86 guests provide explicit load addresses in the VM config.
    // Rewriting them to the start of the reserved/identical RAM region breaks
    // the expected boot layout on the Linux-host RISC-V path.
}

#[cfg(target_arch = "x86_64")]
fn x86_linux_direct_boot_config(config: &AxVMCrateConfig) -> bool {
    crate::vmm::images::is_x86_linux_image_config(config)
}

fn vm_alloc_memory_regions(vm_create_config: &AxVMCrateConfig, vm: &VM) -> AxResult {
    const MB: usize = 1024 * 1024;
    const ALIGN: usize = 2 * MB;

    let make_layout = |memory: &axvm::config::VmMemConfig| {
        Layout::from_size_align(memory.size, ALIGN).map_err(|e| {
            ax_err_type!(
                InvalidInput,
                format!("Invalid VM memory layout {:?}: {e:?}", memory)
            )
        })
    };

    for memory in &vm_create_config.kernel.memory_regions {
        match memory.map_type {
            VmMemMappingType::MapAlloc => {
                vm.alloc_memory_region(make_layout(memory)?, Some(GuestPhysAddr::from(memory.gpa)))
                    .map_err(|e| {
                        ax_err_type!(
                            NoMemory,
                            format!("Failed to allocate memory region for VM: {e:?}")
                        )
                    })?;
            }
            VmMemMappingType::MapIdentical => {
                vm.map_identical_memory_region(
                    make_layout(memory)?,
                    Some(GuestPhysAddr::from(memory.gpa)),
                )
                    .map_err(|e| {
                        ax_err_type!(
                            NoMemory,
                            format!("Failed to map identical memory region for VM: {e:?}")
                        )
                    })?;
            }
            VmMemMappingType::MapReserved => {
                debug!("VM[{}] map same region: {:#x?}", vm.id(), memory);
                vm.map_reserved_memory_region(
                    make_layout(memory)?,
                    Some(GuestPhysAddr::from(memory.gpa)),
                )
                .map_err(|e| {
                    ax_err_type!(
                        NoMemory,
                        format!("Failed to map memory region for VM: {e:?}")
                    )
                })?;
            }
        }
    }
    Ok(())
}
