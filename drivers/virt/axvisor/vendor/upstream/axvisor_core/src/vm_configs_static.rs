/// Static VM config strings embedded by Linux Kbuild.
pub fn static_vm_configs() -> Vec<&'static str> {
    vec![r#"[base]
id = 1
name = "linux-x86_64-qemu"
vm_type = 1
cpu_num = 16
phys_cpu_ids = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]

[kernel]
entry_point = 0x8000
boot_protocol = "direct"
image_location = "memory"
kernel_path = "/tmp/axvisor-adapter-x86-build/arch/x86/boot/bzImage"
kernel_load_addr = 0x20_0000
ramdisk_path = ""
ramdisk_load_addr = 0x40_00000
cmdline = "console=ttyS0 earlyprintk=serial root=/dev/vda rw rootwait devtmpfs.mount=1 init=/init reboot=k panic=1 acpi=off pci=conf1 pci=nomsi irqpoll nox2apic tsc=unstable no_timer_check initcall_blacklist=ahci_pci_driver_init,i8042_init"
memory_regions = [
  # Linux-host mode cannot reuse host-owned low RAM for identity DMA. The
  # smoke launcher reserves this host physical range with memmap before insmod.
  [0x40000000, 0x08000000, 0x7, 1],
  [0x0000_0000, 0x0010_0000, 0x7, 0],
]

[devices]
interrupt_mode = "passthrough"
passthrough_devices = [["HPET", 0xfed0_0000, 0xfed0_0000, 0x1000, 0x1]]
passthrough_addresses = [[0x8000_0000, 0x20_0000], [0x8_0000_0000, 0x20_0000], [0xfe00_0000, 0x00c0_0000], [0xfed8_0000, 0x1000], [0x70_0000_0000, 0x10_0000], [0x3800_0000_0000, 0x10_0000]]
passthrough_ports = [[0x6000, 0x80], [0xc000, 0x80]]
emu_devices = [["x86-com1", 0x3f8, 0x8, 0x0, 0x2, []], ["x86-ioapic", 0xfec0_0000, 0x1000, 0x0, 0x23, []], ["x86-pit", 0x40, 0x22, 0x0, 0x24, []]]
"#]
}

/// One guest image data from memory.
pub struct MemoryImage {
    /// VM id in config file.
    pub id: usize,
    /// Kernel image bytes.
    pub kernel: &'static [u8],
    /// Optional DTB image bytes.
    pub dtb: Option<&'static [u8]>,
    /// Optional BIOS image bytes.
    pub bios: Option<&'static [u8]>,
    /// Optional ramdisk image bytes.
    pub ramdisk: Option<&'static [u8]>,
}

/// Guest images embedded by Linux Kbuild.
pub fn get_memory_images() -> &'static [MemoryImage] {
    &[MemoryImage {
        id: 1,
        kernel: include_bytes!(r"/tmp/axvisor-adapter-x86-build/arch/x86/boot/bzImage"),
        dtb: None,
        bios: None,
        ramdisk: None,
    }]
}
