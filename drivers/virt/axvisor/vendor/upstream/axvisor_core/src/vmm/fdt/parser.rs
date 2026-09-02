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

//! FDT parsing and processing functionality.

use alloc::{format, string::{String, ToString}, vec::Vec};

use ax_errno::{AxResult, ax_err_type};
use axaddrspace::MappingFlags;
use axvm::config::{
    AxVMConfig, AxVMCrateConfig, PassThroughDeviceConfig, VmMemConfig, VmMemMappingType,
};
use fdt_parser::{Fdt, FdtHeader, PciRange, PciSpace};

use crate::vmm::fdt::crate_guest_fdt_with_cache;
#[cfg(target_arch = "aarch64")]
use crate::vmm::fdt::create::update_cpu_node;

const PAGE_SIZE_4K: usize = 0x1000;

pub fn try_get_host_fdt() -> Option<&'static [u8]> {
    const FDT_VALID_MAGIC: u32 = 0xd00d_feed;

    #[cfg(axvisor_host_riscv64)]
    {
        if let Some(bytes) = axvisor_api::arch::host_fdt_bytes() {
            let header = FdtHeader::from_bytes(bytes.get(..core::mem::size_of::<FdtHeader>())?)
                .map_err(|e| error!("Failed to parse host FDT header: {e:#?}"))
                .ok()?;
            if header.magic.get() != FDT_VALID_MAGIC {
                error!(
                    "FDT magic is invalid, expected {:#x}, got {:#x}",
                    FDT_VALID_MAGIC,
                    header.magic.get()
                );
                return None;
            }
            let total_size = header.total_size();
            if bytes.len() < total_size {
                error!(
                    "FDT byte provider is truncated, expected {} bytes, got {} bytes",
                    total_size,
                    bytes.len()
                );
                return None;
            }
            return Some(&bytes[..total_size]);
        }
        warn!("Linux RISC-V host did not provide host FDT bytes");
        None
    }

    #[cfg(not(axvisor_host_riscv64))]
    {
        let bootarg = axvisor_api::arch::host_fdt_paddr();
        let Some(bootarg) = bootarg else {
            warn!("Boot argument does not contain a host FDT pointer");
            return None;
        };

        let fdt_vaddr = axvisor_api::memory::phys_to_virt(bootarg);
        let header = unsafe {
            core::slice::from_raw_parts(fdt_vaddr.as_ptr(), core::mem::size_of::<FdtHeader>())
        };
        let fdt_header = match FdtHeader::from_bytes(header) {
            Ok(header) => header,
            Err(e) => {
                error!("Failed to parse host FDT header: {e:#?}");
                return None;
            }
        };

        if fdt_header.magic.get() != FDT_VALID_MAGIC {
            error!(
                "FDT magic is invalid, expected {:#x}, got {:#x}",
                FDT_VALID_MAGIC,
                fdt_header.magic.get()
            );
            return None;
        }

        Some(unsafe { core::slice::from_raw_parts(fdt_vaddr.as_ptr(), fdt_header.total_size()) })
    }
}

pub fn setup_guest_fdt_from_vmm(
    fdt_bytes: &[u8],
    vm_cfg: &mut AxVMConfig,
    crate_config: &AxVMCrateConfig,
) -> AxResult {
    let fdt = Fdt::from_bytes(fdt_bytes)
        .map_err(|e| ax_err_type!(InvalidData, format!("Failed to parse host FDT: {e:#?}")))?;

    // Call the modified function and get the returned device name list
    let passthrough_device_names = super::device::find_all_passthrough_devices(vm_cfg, &fdt);

    let dtb_data = super::create::crate_guest_fdt(&fdt, &passthrough_device_names, crate_config)?;
    crate_guest_fdt_with_cache(dtb_data, crate_config);
    Ok(())
}

fn is_reserved_memory_path(node_path: &str) -> bool {
    node_path == "/reserved-memory" || node_path.starts_with("/reserved-memory/")
}

fn overlaps_memory_region(lhs_gpa: usize, lhs_size: usize, rhs: &VmMemConfig) -> bool {
    let lhs_end = lhs_gpa.saturating_add(lhs_size);
    let rhs_end = rhs.gpa.saturating_add(rhs.size);
    lhs_gpa < rhs_end && rhs.gpa < lhs_end
}

fn align_down_4k(value: usize) -> usize {
    value & !(PAGE_SIZE_4K - 1)
}

fn align_up_4k(value: usize) -> usize {
    value
        .saturating_add(PAGE_SIZE_4K - 1)
        .checked_div(PAGE_SIZE_4K)
        .unwrap_or(usize::MAX / PAGE_SIZE_4K)
        .saturating_mul(PAGE_SIZE_4K)
}

fn align_reserved_region_4k(gpa: usize, size: usize) -> Option<(usize, usize)> {
    if size == 0 {
        return None;
    }

    let aligned_gpa = align_down_4k(gpa);
    let end = gpa.saturating_add(size);
    let aligned_end = align_up_4k(end);
    let aligned_size = aligned_end.saturating_sub(aligned_gpa);

    (aligned_size > 0).then_some((aligned_gpa, aligned_size))
}

fn subtract_memory_region_overlap(
    start: usize,
    size: usize,
    existing_regions: &[VmMemConfig],
) -> Vec<(usize, usize)> {
    let mut remaining = vec![(start, start.saturating_add(size))];
    let mut overlaps = existing_regions.to_vec();
    overlaps.sort_by_key(|region| region.gpa);

    for region in overlaps {
        let overlap_start = region.gpa;
        let overlap_end = region.gpa.saturating_add(region.size);
        let mut next_remaining = Vec::new();

        for (seg_start, seg_end) in remaining {
            if overlap_end <= seg_start || overlap_start >= seg_end {
                next_remaining.push((seg_start, seg_end));
                continue;
            }

            if seg_start < overlap_start {
                next_remaining.push((seg_start, overlap_start.min(seg_end)));
            }
            if overlap_end < seg_end {
                next_remaining.push((overlap_end.max(seg_start), seg_end));
            }
        }

        remaining = next_remaining;
        if remaining.is_empty() {
            break;
        }
    }

    remaining
        .into_iter()
        .filter_map(|(seg_start, seg_end)| {
            let seg_size = seg_end.saturating_sub(seg_start);
            (seg_size > 0).then_some((seg_start, seg_size))
        })
        .collect()
}

fn reserved_memory_regions(crate_cfg: &AxVMCrateConfig) -> impl Iterator<Item = &VmMemConfig> {
    crate_cfg
        .kernel
        .memory_regions
        .iter()
        .filter(|region| region.map_type == VmMemMappingType::MapReserved)
}

fn fdt_reserved_memory_covers(dtb: &[u8], start: usize, size: usize) -> AxResult<bool> {
    let end = start
        .checked_add(size)
        .ok_or_else(|| ax_err_type!(InvalidInput, "memory region range overflows"))?;
    let fdt = Fdt::from_bytes(dtb).map_err(|e| {
        ax_err_type!(
            InvalidData,
            format!("Failed to parse DTB image while validating reserved memory: {e:#?}")
        )
    })?;
    let all_nodes: Vec<_> = fdt.all_nodes().collect();
    let all_paths = super::build_all_node_paths(&all_nodes);

    for (index, node) in all_nodes.iter().enumerate() {
        if !is_reserved_memory_path(&all_paths[index]) {
            continue;
        }
        if let Some(reg_iter) = node.reg() {
            for reg in reg_iter {
                let region_start = reg.address as usize;
                let region_size = reg.size.unwrap_or(0);
                let Some((region_start, region_size)) =
                    align_reserved_region_4k(region_start, region_size)
                else {
                    continue;
                };
                let region_end = region_start.saturating_add(region_size);
                if start >= region_start && end <= region_end {
                    return Ok(true);
                }
            }
        }
    }

    Ok(false)
}

#[cfg(axvisor_host_riscv64)]
pub fn validate_linux_host_guest_ram_reserved(
    crate_cfg: &AxVMCrateConfig,
    host_dtb: &[u8],
) -> AxResult {
    for memory in crate_cfg.kernel.memory_regions.iter().take(
        crate_cfg
            .kernel
            .configured_memory_region_count
            .min(crate_cfg.kernel.memory_regions.len()),
    ) {
        if memory.map_type != VmMemMappingType::MapIdentical {
            continue;
        }
        if !fdt_reserved_memory_covers(host_dtb, memory.gpa, memory.size)? {
            return Err(ax_err_type!(
                InvalidData,
                format!(
                    "Linux-host MapIdentical guest RAM [{:#x}~{:#x}] is not covered by host DTB reserved-memory/no-map",
                    memory.gpa,
                    memory.gpa.saturating_add(memory.size)
                )
            ));
        }
    }

    Ok(())
}

#[cfg(not(axvisor_host_riscv64))]
pub fn validate_linux_host_guest_ram_reserved(
    _crate_cfg: &AxVMCrateConfig,
    _host_dtb: &[u8],
) -> AxResult {
    Ok(())
}

fn is_memory_like_compatible(node: &fdt_parser::Node<'_>) -> bool {
    node.compatibles().any(|compat| {
        compat == "mmio-sram"
            || compat.contains("shared-memory")
            || compat.contains("shmem")
            || compat.contains("sram")
    })
}

fn is_partition_like_node(node: &fdt_parser::Node<'_>, node_path: &str) -> bool {
    if node
        .compatibles()
        .any(|compat| compat == "fixed-partitions")
    {
        return true;
    }

    node_path.contains("/partitions/")
}

fn should_skip_passthrough_node(
    node: &fdt_parser::Node<'_>,
    node_path: &str,
    reserved_regions: &[VmMemConfig],
) -> bool {
    if !is_memory_like_compatible(node) {
        return false;
    }

    let Some(reg_iter) = node.reg() else {
        return false;
    };

    for reg in reg_iter {
        let gpa = reg.address as usize;
        let size = reg.size.unwrap_or(0);
        if size == 0 {
            continue;
        }

        if let Some(region) = reserved_regions
            .iter()
            .find(|region| overlaps_memory_region(gpa, size, region))
        {
            debug!(
                "Skipping passthrough node {} [{:#x}~{:#x}] because memory-like compatible \
                 overlaps reserved region [{:#x}~{:#x}]",
                node_path,
                gpa,
                gpa + size,
                region.gpa,
                region.gpa + region.size
            );
            return true;
        }
    }

    false
}

pub fn parse_reserved_memory_regions(crate_cfg: &mut AxVMCrateConfig, dtb: &[u8]) -> AxResult {
    let fdt = Fdt::from_bytes(dtb).map_err(|e| {
        ax_err_type!(
            InvalidData,
            format!("Failed to parse DTB image while reading reserved memory: {e:#?}")
        )
    })?;
    let all_nodes: Vec<_> = fdt.all_nodes().collect();
    let all_paths = super::build_all_node_paths(&all_nodes);
    let default_flags = (MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE).bits();

    let mut added_count = 0usize;
    for (index, node) in all_nodes.iter().enumerate() {
        let node_path = &all_paths[index];
        if !is_reserved_memory_path(node_path) {
            continue;
        }

        if let Some(reg_iter) = node.reg() {
            for reg in reg_iter {
                let original_gpa = reg.address as usize;
                let original_size = reg.size.unwrap_or(0);
                let Some((gpa, size)) = align_reserved_region_4k(original_gpa, original_size)
                else {
                    continue;
                };

                if gpa != original_gpa || size != original_size {
                    debug!(
                        "Aligning reserved-memory {} from [{:#x}~{:#x}] to [{:#x}~{:#x}]",
                        node_path,
                        original_gpa,
                        original_gpa.saturating_add(original_size),
                        gpa,
                        gpa.saturating_add(size)
                    );
                }

                let remaining_segments =
                    subtract_memory_region_overlap(gpa, size, &crate_cfg.kernel.memory_regions);

                if remaining_segments.is_empty() {
                    debug!(
                        "Skipping reserved-memory {} [{:#x}~{:#x}] because it is fully covered by \
                         existing memory_regions",
                        node_path,
                        gpa,
                        gpa + size
                    );
                    continue;
                }

                if remaining_segments.len() != 1 || remaining_segments[0] != (gpa, size) {
                    debug!(
                        "Cropping reserved-memory {} [{:#x}~{:#x}] into {:?} to avoid overlaps",
                        node_path,
                        gpa,
                        gpa + size,
                        remaining_segments
                    );
                }

                for (seg_gpa, seg_size) in remaining_segments {
                    crate_cfg.kernel.memory_regions.push(VmMemConfig {
                        gpa: seg_gpa,
                        size: seg_size,
                        flags: default_flags,
                        map_type: VmMemMappingType::MapReserved,
                    });
                    added_count += 1;
                }
            }
        }
    }

    if added_count > 0 {
        debug!(
            "Added {} reserved-memory region(s) from DTB into VM kernel memory_regions",
            added_count
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::align_reserved_region_4k;

    #[test]
    fn align_reserved_region_keeps_aligned_range() {
        assert_eq!(
            align_reserved_region_4k(0x1000, 0x2000),
            Some((0x1000, 0x2000))
        );
    }

    #[test]
    fn align_reserved_region_expands_to_cover_unaligned_bounds() {
        assert_eq!(
            align_reserved_region_4k(0x1100, 0x2500),
            Some((0x1000, 0x3000))
        );
    }

    #[test]
    fn align_reserved_region_rejects_zero_sized_range() {
        assert_eq!(align_reserved_region_4k(0x1000, 0), None);
    }

    #[test]
    fn subtract_memory_region_overlap_keeps_non_overlapping_range() {
        let existing = vec![VmMemConfig {
            gpa: 0x4000,
            size: 0x1000,
            flags: 0,
            map_type: VmMemMappingType::MapReserved,
        }];

        assert_eq!(
            subtract_memory_region_overlap(0x1000, 0x1000, &existing),
            vec![(0x1000, 0x1000)]
        );
    }

    #[test]
    fn subtract_memory_region_overlap_splits_range_around_overlap() {
        let existing = vec![VmMemConfig {
            gpa: 0x3000,
            size: 0x2000,
            flags: 0,
            map_type: VmMemMappingType::MapReserved,
        }];

        assert_eq!(
            subtract_memory_region_overlap(0x1000, 0x6000, &existing),
            vec![(0x1000, 0x2000), (0x5000, 0x2000)]
        );
    }

    #[test]
    fn subtract_memory_region_overlap_drops_fully_covered_range() {
        let existing = vec![VmMemConfig {
            gpa: 0x1000,
            size: 0x4000,
            flags: 0,
            map_type: VmMemMappingType::MapReserved,
        }];

        assert!(subtract_memory_region_overlap(0x2000, 0x1000, &existing).is_empty());
    }
}

pub fn set_phys_cpu_sets(
    vm_cfg: &mut AxVMConfig,
    fdt: &Fdt,
    crate_config: &AxVMCrateConfig,
) -> AxResult {
    // Find and parse CPU information from host DTB
    let host_cpus: Vec<_> = fdt.find_nodes("/cpus/cpu").collect();
    info!("Found {} host CPU nodes", &host_cpus.len());

    let phys_cpu_ids = crate_config
        .base
        .phys_cpu_ids
        .as_ref()
        .ok_or_else(|| ax_err_type!(InvalidInput, "phys_cpu_ids is missing"))?;

    let cpu_nodes_info: Vec<_> = host_cpus
        .iter()
        .filter_map(|cpu_node| {
            let node_id = cpu_node
                .name()
                .strip_prefix("cpu@")
                .and_then(|id| usize::from_str_radix(id, 16).ok())?;
            let cpu_reg = cpu_node.reg().and_then(|mut reg| reg.next())?;
            let guest_cpu_id = cpu_reg.address as usize;
            info!(
                "CPU node: {}, node_id: 0x{:x}, guest_cpu_id: 0x{:x}",
                cpu_node.name(),
                node_id,
                guest_cpu_id
            );
            Some((node_id, guest_cpu_id))
        })
        .collect();

    let mut new_phys_cpu_sets = Vec::new();
    let mut guest_phys_cpu_ids = Vec::new();
    for phys_cpu_id in phys_cpu_ids {
        if let Some((cpu_index, (_, guest_cpu_id))) = cpu_nodes_info
            .iter()
            .enumerate()
            .find(|(_, (node_id, _))| node_id == phys_cpu_id)
        {
            let cpu_mask = 1usize << cpu_index;
            new_phys_cpu_sets.push(cpu_mask);
            guest_phys_cpu_ids.push(*guest_cpu_id);
            debug!(
                "vCPU {} with phys_cpu_id 0x{:x} mapped to CPU index {} (mask: 0x{:x}), guest CPU \
                 ID 0x{:x}",
                vm_cfg.id(),
                phys_cpu_id,
                cpu_index,
                cpu_mask,
                guest_cpu_id
            );
        } else {
            error!(
                "vCPU {} with phys_cpu_id 0x{:x} not found in device tree!",
                vm_cfg.id(),
                phys_cpu_id
            );
        }
    }

    info!("Calculated phys_cpu_sets: {new_phys_cpu_sets:?}");
    info!("Calculated guest phys_cpu_ids: {guest_phys_cpu_ids:?}");

    let phys_cpu_ls = vm_cfg.phys_cpu_ls_mut();
    phys_cpu_ls.set_guest_cpu_sets(new_phys_cpu_sets);
    phys_cpu_ls.set_guest_phys_cpu_ids(guest_phys_cpu_ids);

    debug!(
        "vcpu_mappings: {:?}",
        vm_cfg.phys_cpu_ls_mut().get_vcpu_affinities_pcpu_ids()
    );
    Ok(())
}

/// Add address mapping configuration for a device
fn add_device_address_config(
    vm_cfg: &AxVMConfig,
    out: &mut Vec<PassThroughDeviceConfig>,
    node_name: &str,
    base_address: usize,
    host_address: usize,
    size: usize,
    irq_id: usize,
    index: usize,
    prefix: Option<&str>,
) {
    // Only process devices with address information
    if size == 0 {
        return;
    }

    let addr_end = base_address.saturating_add(size);
    // Runtime DT parsing may discover the same device range that is already
    // owned by an emulated device (for example the guest PLIC window). Do not
    // add a passthrough mapping for such a range, otherwise the guest can
    // bypass the emulation layer entirely.
    if let Some(emu_dev) = vm_cfg.emu_devices().iter().find(|emu_dev| {
        let emu_start = emu_dev.base_gpa;
        let emu_end = emu_dev.base_gpa.saturating_add(emu_dev.length);
        base_address < emu_end && emu_start < addr_end
    }) {
        debug!(
            "Skipping passthrough mapping for node {} [{:#x}~{:#x}] because it overlaps emulated \
             device {} [{:#x}~{:#x}]",
            node_name,
            base_address,
            addr_end,
            emu_dev.name,
            emu_dev.base_gpa,
            emu_dev.base_gpa.saturating_add(emu_dev.length),
        );
        return;
    }

    // Create a device configuration for each address segment
    let device_name = if index == 0 {
        match prefix {
            Some(p) => format!("{node_name}-{p}"),
            None => node_name.to_string(),
        }
    } else {
        match prefix {
            Some(p) => format!("{node_name}-{p}-region{index}"),
            None => format!("{node_name}-region{index}"),
        }
    };

    // Add new device configuration
    let pt_dev = PassThroughDeviceConfig {
        name: device_name,
        base_gpa: base_address,
        base_hpa: host_address,
        length: size,
        irq_id,
    };
    out.push(pt_dev);
}

/// Add ranges property configuration for PCIe devices
fn add_pci_ranges_config(
    vm_cfg: &AxVMConfig,
    out: &mut Vec<PassThroughDeviceConfig>,
    node_name: &str,
    range: &PciRange,
    irq_id: usize,
    index: usize,
) {
    let base_address = range.cpu_address as usize;
    let size = range.size as usize;

    // Only process devices with address information
    if size == 0 {
        return;
    }

    // Create a device configuration for each address segment
    let prefix = match range.space {
        PciSpace::Configuration => "config",
        PciSpace::IO => "io",
        PciSpace::Memory32 => "mem32",
        PciSpace::Memory64 => "mem64",
    };

    let device_name = if index == 0 {
        format!("{node_name}-{prefix}")
    } else {
        format!("{node_name}-{prefix}-region{index}")
    };

    // Add new device configuration
    let pt_dev = PassThroughDeviceConfig {
        name: device_name,
        base_gpa: base_address,
        base_hpa: base_address,
        length: size,
        irq_id,
    };
    out.push(pt_dev);

    trace!(
        "Added PCIe passthrough device {}: base=0x{:x}, size=0x{:x}, space={:?}",
        node_name, base_address, size, range.space
    );
}

#[cfg(target_arch = "riscv64")]
fn node_irq_id(node: &fdt_parser::Node<'_>) -> usize {
    let Some(interrupts) = node.interrupts() else {
        return 0;
    };

    for interrupt in interrupts {
        let mut cells = interrupt;
        if let Some(irq_id) = cells.next() {
            return irq_id as usize;
        }
    }

    0
}

#[cfg(not(target_arch = "riscv64"))]
fn node_irq_id(_node: &fdt_parser::Node<'_>) -> usize {
    0
}

fn find_node_irq_id_by_path(all_nodes: &[fdt_parser::Node<'_>], all_paths: &[String], path: &str) -> usize {
    all_nodes
        .iter()
        .zip(all_paths.iter())
        .find(|(_, node_path)| node_path.as_str() == path)
        .map(|(node, _)| node_irq_id(node))
        .unwrap_or(0)
}

pub fn parse_passthrough_devices_address(
    vm_cfg: &mut AxVMConfig,
    crate_cfg: &AxVMCrateConfig,
    dtb: &[u8],
) -> AxResult {
    let devices = vm_cfg.pass_through_devices().to_vec();
    let fdt = Fdt::from_bytes(dtb).map_err(|e| {
        ax_err_type!(
            InvalidData,
            format!("Failed to parse DTB image while reading passthrough devices: {e:#?}")
        )
    })?;

    let all_nodes: Vec<_> = fdt.all_nodes().collect();
    let all_paths = super::build_all_node_paths(&all_nodes);
    let reserved_regions: Vec<VmMemConfig> = reserved_memory_regions(crate_cfg).cloned().collect();

    let has_path_based_device = devices
        .iter()
        .any(|device| device.length == 0 && device.name.starts_with('/'));
    let has_explicit_region = devices.iter().any(|device| device.length != 0);

    let mut resolved_devices = Vec::new();

    if has_explicit_region && !has_path_based_device {
        for device in devices {
            if device.length == 0 {
                continue;
            }

            let irq_id = if device.irq_id != 0 {
                device.irq_id
            } else if device.name.starts_with('/') {
                find_node_irq_id_by_path(&all_nodes, &all_paths, &device.name)
            } else {
                0
            };

            resolved_devices.push(PassThroughDeviceConfig {
                irq_id,
                ..device
            });
        }
    } else {
        // Traverse all device tree nodes in the guest-facing DTB. At this point the
        // DTB already contains only the passthrough-relevant nodes plus the minimum
        // required platform nodes, so rebuilding the final passthrough map from it
        // preserves dependency expansion while letting us backfill RISC-V IRQ IDs
        // from the source DT.
        for (index, node) in all_nodes.iter().enumerate() {
            let node_path = &all_paths[index];

            // Skip root node
            if node.name() == "/"
                || node.name().starts_with("memory")
                || is_reserved_memory_path(node_path)
            {
                continue;
            }

            if is_partition_like_node(node, node_path) {
                debug!(
                    "Skipping partition-like node {} from passthrough parsing",
                    node_path
                );
                continue;
            }

            if should_skip_passthrough_node(node, node_path, &reserved_regions) {
                continue;
            }

            let node_name = node.name().to_string();
            let irq_id = node_irq_id(node);

            // Check if it's a PCIe device node
            if node_name.starts_with("pcie@") || node_name.contains("pci") {
                // Process PCIe device's ranges property
                if let Some(pci) = node.clone().into_pci() {
                    if let Ok(ranges) = pci.ranges() {
                        for (range_index, range) in ranges.enumerate() {
                            add_pci_ranges_config(
                                vm_cfg,
                                &mut resolved_devices,
                                &node_name,
                                &range,
                                irq_id,
                                range_index,
                            );
                        }
                    }
                }

                // Process PCIe device's reg property (ECAM space)
                if let Some(reg_iter) = node.reg() {
                    for (reg_index, reg) in reg_iter.enumerate() {
                        let base_address = reg.address as usize;
                        let size = reg.size.unwrap_or(0);

                        add_device_address_config(
                            vm_cfg,
                            &mut resolved_devices,
                            &node_name,
                            base_address,
                            base_address,
                            size,
                            irq_id,
                            reg_index,
                            Some("ecam"),
                        );
                    }
                }
            } else {
                // Get device's reg property (process regular devices)
                if let Some(reg_iter) = node.reg() {
                    // Process all address segments of the device
                    for (reg_index, reg) in reg_iter.enumerate() {
                        // Get device's address and size information
                        let base_address = reg.address as usize;
                        let size = reg.size.unwrap_or(0);

                        add_device_address_config(
                            vm_cfg,
                            &mut resolved_devices,
                            &node_name,
                            base_address,
                            base_address,
                            size,
                            irq_id,
                            reg_index,
                            None,
                        );
                    }
                }
            }
        }
    }

    vm_cfg.clear_pass_through_devices();
    for device in resolved_devices {
        vm_cfg.add_pass_through_device(device);
    }
    validate_linux_host_passthrough_devices(vm_cfg)?;

    trace!("All passthrough devices: {:#x?}", vm_cfg.pass_through_devices());
    debug!(
        "Finished parsing passthrough devices, total: {}",
        vm_cfg.pass_through_devices().len()
    );
    Ok(())
}

#[cfg(axvisor_host_riscv64)]
fn validate_linux_host_passthrough_devices(vm_cfg: &AxVMConfig) -> AxResult {
    if vm_cfg.pass_through_devices().is_empty() {
        return Ok(());
    }

    for device in vm_cfg.pass_through_devices() {
        if device.length == 0 {
            return Err(ax_err_type!(
                InvalidData,
                format!(
                    "Linux-host passthrough device {} has zero MMIO length after FDT parsing",
                    device.name
                )
            ));
        }
        if device.irq_id == 0 {
            return Err(ax_err_type!(
                InvalidData,
                format!(
                    "Linux-host passthrough device {} [{:#x}~{:#x}] has zero IRQ after FDT parsing",
                    device.name,
                    device.base_hpa,
                    device.base_hpa.saturating_add(device.length)
                )
            ));
        }
    }

    Ok(())
}

#[cfg(not(axvisor_host_riscv64))]
fn validate_linux_host_passthrough_devices(_vm_cfg: &AxVMConfig) -> AxResult {
    Ok(())
}

#[cfg(target_arch = "aarch64")]
pub fn parse_vm_interrupt(vm_cfg: &mut AxVMConfig, dtb: &[u8]) -> AxResult {
    const GIC_PHANDLE: usize = 1;
    let fdt = Fdt::from_bytes(dtb).map_err(|e| {
        ax_err_type!(
            InvalidData,
            format!("Failed to parse DTB image while reading interrupts: {e:#?}")
        )
    })?;

    for node in fdt.all_nodes() {
        let name = node.name();

        if name.starts_with("memory") {
            continue;
        }
        // Skip the interrupt controller, as we will use vGIC
        // TODO: filter with compatible property and parse its phandle from DT; maybe needs a second pass?
        else if name.starts_with("interrupt-controller")
            || name.starts_with("intc")
            || name.starts_with("its")
        {
            debug!("skipping node {name} to use vGIC");
            continue;
        }

        // Collect all GIC_SPI interrupts and add them to vGIC
        if let Some(interrupts) = node.interrupts() {
            // TODO: skip non-GIC interrupt
            if let Some(parent) = node.interrupt_parent() {
                trace!("node: {}, intr parent: {}", name, parent.node.name());
                if let Some(phandle) = parent.node.phandle() {
                    if phandle.as_usize() != GIC_PHANDLE {
                        debug!(
                            "node: {}, intr parent: {}, phandle: 0x{:x} is not GIC!",
                            name,
                            parent.node.name(),
                            phandle.as_usize()
                        );
                    }
                } else {
                    warn!(
                        "node: {}, intr parent: {} no phandle!",
                        name,
                        parent.node.name(),
                    );
                }
            } else {
                warn!("node: {name} no interrupt parent!");
            }

            for interrupt in interrupts {
                // <GIC_SPI/GIC_PPI, IRQn, trigger_mode>
                for (k, v) in interrupt.enumerate() {
                    match k {
                        0 => {
                            if v == 0 {
                                trace!("node: {name}, GIC_SPI");
                            } else {
                                debug!("node: {name}, intr type: {v}, not GIC_SPI, not supported!");
                                break;
                            }
                        }
                        1 => {
                            trace!("node: {name}, interrupt id: 0x{v:x}");
                            vm_cfg.add_pass_through_spi(v);
                        }
                        2 => {
                            trace!("node: {name}, interrupt mode: 0x{v:x}");
                        }
                        _ => {
                            warn!("unknown interrupt property {k}:0x{v:x}")
                        }
                    }
                }
            }
        }
    }

    // vm_cfg.add_pass_through_device(PassThroughDeviceConfig {
    //     name: "Fake Node".to_string(),
    //     base_gpa: 0x0,
    //     base_hpa: 0x0,
    //     length: 0x20_0000,
    //     irq_id: 0,
    // });
    Ok(())
}

pub fn update_provided_fdt(
    provided_dtb: &[u8],
    host_dtb: &[u8],
    crate_config: &AxVMCrateConfig,
) -> AxResult {
#[cfg(any(target_arch = "loongarch64", target_arch = "riscv64"))]
    {
        let _ = host_dtb;
        #[cfg(target_arch = "riscv64")]
        {
            let provided_fdt = Fdt::from_bytes(provided_dtb).map_err(|e| {
                ax_err_type!(
                    InvalidData,
                    format!("Failed to parse provided DTB image: {e:#?}")
                )
            })?;
            let patched = super::create::sanitize_provided_guest_fdt(&provided_fdt, crate_config)?;
            crate_guest_fdt_with_cache(patched, crate_config);
        }
        #[cfg(target_arch = "loongarch64")]
        {
            crate_guest_fdt_with_cache(provided_dtb.to_vec(), crate_config);
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        let provided_fdt = Fdt::from_bytes(provided_dtb).map_err(|e| {
            ax_err_type!(
                InvalidData,
                format!("Failed to parse provided DTB image: {e:#?}")
            )
        })?;
        let host_fdt = Fdt::from_bytes(host_dtb).map_err(|e| {
            ax_err_type!(
                InvalidData,
                format!("Failed to parse host DTB image: {e:#?}")
            )
        })?;
        let provided_dtb_data = update_cpu_node(&provided_fdt, &host_fdt, crate_config)?;
        crate_guest_fdt_with_cache(provided_dtb_data, crate_config);
    }
    Ok(())
}
