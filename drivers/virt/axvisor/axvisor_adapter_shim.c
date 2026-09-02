// SPDX-License-Identifier: GPL-2.0-only

#include <linux/cpumask.h>
#include <linux/delay.h>
#include <linux/gfp.h>
#include <linux/kthread.h>
#include <linux/kernel.h>
#include <linux/list.h>
#include <linux/mm.h>
#include <linux/vmalloc.h>
#include <linux/fs.h>
#include <linux/kernel_read_file.h>
#include <linux/namei.h>
#include <linux/moduleparam.h>
#include <linux/of_fdt.h>
#include <linux/platform_device.h>
#include <linux/overflow.h>
#include <linux/pci.h>
#include <linux/atomic.h>
#include <linux/sched.h>
#include <linux/smp.h>
#include <linux/sched/task.h>
#include <linux/ktime.h>
#include <linux/reboot.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/string.h>
#include <linux/proc_fs.h>
#include <linux/poll.h>
#include <linux/sched/task_stack.h>
#include <linux/io.h>
#include <linux/interrupt.h>
#include <linux/irq.h>
#include <linux/uaccess.h>
#include <linux/wait.h>
#include <uapi/linux/virtio_mmio.h>

#ifdef CONFIG_RISCV
#include <asm/delay.h>
#endif
#ifdef CONFIG_X86
#include <asm/hw_irq.h>
#endif

enum axvisor_memory_kind {
	AXVISOR_MEM_FRAME = 1,
	AXVISOR_MEM_REMAP = 3,
	AXVISOR_MEM_IOREMAP = 4,
};

struct axvisor_memory_record {
	u64 paddr;
	u64 vaddr;
	u64 raw_vaddr;
	size_t num_frames;
	size_t size_bytes;
	size_t alloc_size_bytes;
	enum axvisor_memory_kind kind;
	struct page *page;
	struct list_head node;
};

struct axvisor_guest_ram_region {
	u64 paddr;
	u64 size;
};

#define AXVISOR_MAX_GUEST_RAM_REGIONS 16
#define AXVISOR_MAX_PASSTHROUGH_IRQS 16

struct axvisor_passthrough_irq_record {
	unsigned int virq;
	unsigned int guest_irq;
	unsigned long hwirq;
	unsigned int host_vector;
	u64 base;
#ifdef CONFIG_X86
	struct pci_dev *pci_dev;
#endif
	bool requested;
	bool host_irq_requested;
	bool host_irq_masked;
#ifdef CONFIG_X86
	bool pci_irq_vectors_allocated;
	bool pci_device_enabled;
#endif
};

static LIST_HEAD(axvisor_memory_records);
static DEFINE_SPINLOCK(axvisor_memory_records_lock);
static struct axvisor_guest_ram_region
	axvisor_guest_ram_regions[AXVISOR_MAX_GUEST_RAM_REGIONS];
static unsigned int axvisor_guest_ram_region_count;
static DEFINE_SPINLOCK(axvisor_guest_ram_regions_lock);
static unsigned long long axvisor_host_fdt_vaddr;
static size_t axvisor_host_fdt_size;
static char *axvisor_host_fdt_path;
#ifdef CONFIG_RISCV
static unsigned long long axvisor_plic_paddr = 0x0c000000ULL;
static unsigned int axvisor_plic_size = 0x600000U;
#else
static unsigned long long axvisor_plic_paddr;
static unsigned int axvisor_plic_size;
#endif
static atomic64_t axvisor_plic_complete_log_count = ATOMIC64_INIT(0);
static struct axvisor_passthrough_irq_record
	axvisor_passthrough_irqs[AXVISOR_MAX_PASSTHROUGH_IRQS];
static DEFINE_SPINLOCK(axvisor_passthrough_irqs_lock);
static atomic64_t axvisor_passthrough_irq_log_count = ATOMIC64_INIT(0);
static atomic64_t axvisor_passthrough_irq_fail_log_count = ATOMIC64_INIT(0);
#ifdef CONFIG_X86
static bool axvisor_x86_register_qemu_blk_intx = true;
static unsigned int axvisor_x86_qemu_blk_guest_gsi = 19;
static unsigned int axvisor_x86_qemu_blk_bus;
static unsigned int axvisor_x86_qemu_blk_dev = 3;
static unsigned int axvisor_x86_qemu_blk_func;
static atomic64_t axvisor_x86_intx_state_log_count = ATOMIC64_INIT(0);
static atomic64_t axvisor_x86_passthrough_complete_log_count = ATOMIC64_INIT(0);
static atomic64_t axvisor_x86_passthrough_vector_log_count = ATOMIC64_INIT(0);
static atomic64_t axvisor_x86_passthrough_poll_log_count = ATOMIC64_INIT(0);
#endif
static atomic64_t axvisor_guest_console_write_byte_log_count = ATOMIC64_INIT(0);
static atomic64_t axvisor_guest_console_read_byte_log_count = ATOMIC64_INIT(0);
static atomic64_t axvisor_guest_console_proc_read_log_count = ATOMIC64_INIT(0);
static unsigned long long axvisor_release_mmio_paddrs[8] = { 0x10008000ULL };
static unsigned int axvisor_release_mmio_paddrs_count = 1;
static bool axvisor_release_registered_passthrough_mmio;
static struct proc_dir_entry *axvisor_shell_proc_entry;
static struct proc_dir_entry *axvisor_guest_console_proc_entry;
static DECLARE_WAIT_QUEUE_HEAD(axvisor_guest_console_input_wait);
static DECLARE_WAIT_QUEUE_HEAD(axvisor_guest_console_output_wait);
static DEFINE_SPINLOCK(axvisor_guest_console_log_lock);
static char axvisor_guest_console_log_line[512];
static size_t axvisor_guest_console_log_len;

static struct axvisor_memory_record *axvisor_memory_record_lookup(u64 paddr);
static struct axvisor_passthrough_irq_record *
axvisor_passthrough_irq_record_for_base_locked(u64 base);
u64 axvisor_adapter_phys_to_virt(u64 paddr);
u64 axvisor_adapter_virt_to_phys(u64 vaddr);
u32 axvisor_adapter_mmio_read32(u64 paddr);
void axvisor_adapter_mmio_write32(u64 paddr, u32 value);

extern size_t axvisor_linux_console_enqueue_bytes(const u8 *bytes, size_t len);
extern bool axvisor_linux_console_shell_ready(void);
extern size_t axvisor_linux_guest_console_enqueue_bytes(const u8 *bytes, size_t len);
extern size_t axvisor_linux_guest_console_drain_bytes(u8 *bytes, size_t len);
extern size_t axvisor_linux_passthrough_device_count(void);
extern u64 axvisor_linux_passthrough_device_base_hpa(size_t index);
extern u64 axvisor_linux_passthrough_device_length(size_t index);
extern size_t axvisor_linux_passthrough_device_irq_id(size_t index);
extern bool axvisor_linux_passthrough_irq_registered(size_t irq_id);
extern bool axvisor_linux_passthrough_irq_inject(size_t irq_id);
extern bool axvisor_linux_passthrough_irq_mark_pending(size_t irq_id);

#define AXVISOR_RUNTIME_ALLOC_MAGIC 0x41585649534f5241ULL
#define AXVISOR_HOST_REMAP_CHUNK_SIZE SZ_2M
#define AXVISOR_GUEST_CONSOLE_INPUT_BUFFER_CAPACITY 4096
#define AXVISOR_GUEST_CONSOLE_OUTPUT_BUFFER_CAPACITY (1024 * 1024)
#define AXVISOR_GUEST_CONSOLE_PROC_IO_CHUNK_SIZE (64 * 1024)

struct axvisor_guest_console_ring {
	u8 *buf;
	size_t cap;
	size_t head;
	size_t tail;
	size_t len;
	size_t dropped;
	size_t zero_writes;
	size_t nonzero_writes;
	spinlock_t lock;
};

struct axvisor_runtime_alloc_header {
	u64 magic;
	void *raw;
	size_t size;
	size_t align;
};

struct axvisor_runtime_alloc_record {
	u64 vaddr;
	size_t size;
	struct list_head node;
};

static LIST_HEAD(axvisor_runtime_alloc_records);
static DEFINE_SPINLOCK(axvisor_runtime_alloc_records_lock);
static u8 axvisor_guest_console_input_storage[AXVISOR_GUEST_CONSOLE_INPUT_BUFFER_CAPACITY];
static u8 axvisor_guest_console_output_storage[AXVISOR_GUEST_CONSOLE_OUTPUT_BUFFER_CAPACITY];
static struct axvisor_guest_console_ring axvisor_guest_console_input = {
	.buf = axvisor_guest_console_input_storage,
	.cap = AXVISOR_GUEST_CONSOLE_INPUT_BUFFER_CAPACITY,
	.lock = __SPIN_LOCK_UNLOCKED(axvisor_guest_console_input.lock),
};
static struct axvisor_guest_console_ring axvisor_guest_console_output = {
	.buf = axvisor_guest_console_output_storage,
	.cap = AXVISOR_GUEST_CONSOLE_OUTPUT_BUFFER_CAPACITY,
	.lock = __SPIN_LOCK_UNLOCKED(axvisor_guest_console_output.lock),
};

static struct axvisor_runtime_alloc_header *
axvisor_runtime_alloc_header_from_ptr(const void *ptr)
{
	struct axvisor_runtime_alloc_header *header;

	if (!ptr)
		return NULL;

	header = (struct axvisor_runtime_alloc_header *)ptr - 1;
	if (header->magic != AXVISOR_RUNTIME_ALLOC_MAGIC)
		return NULL;

	return header;
}

static struct axvisor_runtime_alloc_record *
axvisor_runtime_alloc_record_lookup_locked(u64 vaddr)
{
	struct axvisor_runtime_alloc_record *record;

	list_for_each_entry(record, &axvisor_runtime_alloc_records, node) {
		u64 begin = record->vaddr;
		u64 end = begin + record->size;

		if (vaddr >= begin && vaddr < end)
			return record;
	}

	return NULL;
}

static bool axvisor_runtime_alloc_record_insert(u64 vaddr, size_t size)
{
	struct axvisor_runtime_alloc_record *record;
	unsigned long flags;

	record = kmalloc(sizeof(*record), GFP_KERNEL);
	if (!record)
		return false;

	record->vaddr = vaddr;
	record->size = size;
	INIT_LIST_HEAD(&record->node);

	spin_lock_irqsave(&axvisor_runtime_alloc_records_lock, flags);
	list_add_tail(&record->node, &axvisor_runtime_alloc_records);
	spin_unlock_irqrestore(&axvisor_runtime_alloc_records_lock, flags);
	return true;
}

static void axvisor_runtime_alloc_record_remove(u64 vaddr)
{
	struct axvisor_runtime_alloc_record *record;
	unsigned long flags;

	spin_lock_irqsave(&axvisor_runtime_alloc_records_lock, flags);
	record = axvisor_runtime_alloc_record_lookup_locked(vaddr);
	if (record)
		list_del(&record->node);
	spin_unlock_irqrestore(&axvisor_runtime_alloc_records_lock, flags);

	kfree(record);
}

static bool axvisor_runtime_alloc_contains_ptr(const void *ptr)
{
	struct axvisor_runtime_alloc_record *record;
	unsigned long flags;
	u64 vaddr = (u64)(unsigned long)ptr;
	bool found;

	spin_lock_irqsave(&axvisor_runtime_alloc_records_lock, flags);
	record = axvisor_runtime_alloc_record_lookup_locked(vaddr);
	found = !!record;
	spin_unlock_irqrestore(&axvisor_runtime_alloc_records_lock, flags);

	return found;
}

static size_t axvisor_guest_console_ring_enqueue(struct axvisor_guest_console_ring *ring,
						 const u8 *src, size_t len,
						 bool drop_oldest)
{
	unsigned long flags;
	size_t written = 0;

	if (!ring || !src || !len || !ring->cap)
		return 0;

	spin_lock_irqsave(&ring->lock, flags);
	while (written < len) {
		if (ring->len == ring->cap) {
			if (!drop_oldest)
				break;
			ring->head = (ring->head + 1) % ring->cap;
			ring->len--;
			ring->dropped++;
		}

		{
			u8 byte = src[written++];

			ring->buf[ring->tail] = byte;
			if (byte)
				ring->nonzero_writes++;
			else
				ring->zero_writes++;
		}
		ring->tail = (ring->tail + 1) % ring->cap;
		ring->len++;
	}
	spin_unlock_irqrestore(&ring->lock, flags);

	return written;
}

static size_t axvisor_guest_console_ring_drain(struct axvisor_guest_console_ring *ring,
					       u8 *dst, size_t len)
{
	unsigned long flags;
	size_t read = 0;

	if (!ring || !dst || !len || !ring->cap)
		return 0;

	spin_lock_irqsave(&ring->lock, flags);
	while (read < len && ring->len) {
		dst[read++] = ring->buf[ring->head];
		ring->head = (ring->head + 1) % ring->cap;
		ring->len--;
	}
	spin_unlock_irqrestore(&ring->lock, flags);

	return read;
}

static size_t axvisor_guest_console_ring_len(struct axvisor_guest_console_ring *ring)
{
	unsigned long flags;
	size_t len;

	if (!ring)
		return 0;

	spin_lock_irqsave(&ring->lock, flags);
	len = ring->len;
	spin_unlock_irqrestore(&ring->lock, flags);
	return len;
}

static size_t axvisor_guest_console_ring_space(struct axvisor_guest_console_ring *ring)
{
	unsigned long flags;
	size_t space;

	if (!ring)
		return 0;

	spin_lock_irqsave(&ring->lock, flags);
	space = ring->cap - ring->len;
	spin_unlock_irqrestore(&ring->lock, flags);
	return space;
}

static void axvisor_guest_console_ring_snapshot(struct axvisor_guest_console_ring *ring,
						size_t *len, size_t *dropped,
						size_t *zero_writes,
						size_t *nonzero_writes)
{
	unsigned long flags;

	if (!ring)
		return;

	spin_lock_irqsave(&ring->lock, flags);
	if (len)
		*len = ring->len;
	if (dropped)
		*dropped = ring->dropped;
	if (zero_writes)
		*zero_writes = ring->zero_writes;
	if (nonzero_writes)
		*nonzero_writes = ring->nonzero_writes;
	spin_unlock_irqrestore(&ring->lock, flags);
}

module_param_named(host_fdt_path, axvisor_host_fdt_path, charp, 0600);
MODULE_PARM_DESC(host_fdt_path,
		 "Path to a host DTB blob kept in an AxVisor-owned virtual buffer");
module_param_named(plic_paddr, axvisor_plic_paddr, ullong, 0600);
MODULE_PARM_DESC(plic_paddr,
		 "Host PLIC physical base used by the RISC-V AxVisor bridge");
module_param_named(plic_size, axvisor_plic_size, uint, 0600);
MODULE_PARM_DESC(plic_size,
		 "Host PLIC MMIO window size used by the RISC-V AxVisor bridge");
module_param_array_named(release_mmio_paddrs, axvisor_release_mmio_paddrs,
			 ullong, &axvisor_release_mmio_paddrs_count, 0600);
MODULE_PARM_DESC(release_mmio_paddrs,
		 "Comma-separated platform MMIO device bases to unbind before guest passthrough ownership");
module_param_named(release_registered_passthrough_mmio,
		   axvisor_release_registered_passthrough_mmio, bool, 0600);
MODULE_PARM_DESC(release_registered_passthrough_mmio,
		 "Also unbind all FDT-registered passthrough MMIO devices; disabled by default to preserve the host console");
#ifdef CONFIG_X86
module_param_named(x86_register_qemu_blk_intx,
		   axvisor_x86_register_qemu_blk_intx, bool, 0600);
MODULE_PARM_DESC(x86_register_qemu_blk_intx,
		 "Register QEMU q35 virtio-blk-pci INTx forwarding for x86 native Linux guest smoke");
module_param_named(x86_qemu_blk_guest_gsi,
		   axvisor_x86_qemu_blk_guest_gsi, uint, 0600);
MODULE_PARM_DESC(x86_qemu_blk_guest_gsi,
		 "Guest IOAPIC GSI for QEMU q35 virtio-blk-pci INTx, dev 3 pin A maps to GSI 19");
module_param_named(x86_qemu_blk_bus, axvisor_x86_qemu_blk_bus, uint, 0600);
MODULE_PARM_DESC(x86_qemu_blk_bus, "QEMU passthrough virtio-blk PCI bus");
module_param_named(x86_qemu_blk_dev, axvisor_x86_qemu_blk_dev, uint, 0600);
MODULE_PARM_DESC(x86_qemu_blk_dev, "QEMU passthrough virtio-blk PCI device");
module_param_named(x86_qemu_blk_func, axvisor_x86_qemu_blk_func, uint, 0600);
MODULE_PARM_DESC(x86_qemu_blk_func, "QEMU passthrough virtio-blk PCI function");
#endif

static int axvisor_adapter_load_host_fdt_from_path(void)
{
	void *file_buf = NULL;
	size_t file_size = 0;
	int ret = 0;

	if (axvisor_host_fdt_vaddr || !axvisor_host_fdt_path ||
	    !axvisor_host_fdt_path[0])
		return 0;

	ret = kernel_read_file_from_path(axvisor_host_fdt_path, 0, &file_buf, SIZE_MAX,
					 &file_size, READING_FIRMWARE);
	if (ret < 0)
		return ret;

	if (!file_size) {
		ret = -EINVAL;
		goto out_free_file_buf;
	}

	axvisor_host_fdt_vaddr = (unsigned long long)(unsigned long)file_buf;
	axvisor_host_fdt_size = file_size;
	file_buf = NULL;

out_free_file_buf:
	if (file_buf)
		vfree(file_buf);
	return ret;
}

void axvisor_adapter_host_fdt_release(void)
{
	if (axvisor_host_fdt_vaddr) {
		vfree((void *)(unsigned long)axvisor_host_fdt_vaddr);
		axvisor_host_fdt_vaddr = 0;
		axvisor_host_fdt_size = 0;
	}
}

int axvisor_adapter_host_fdt_prepare(void)
{
	int ret;

	ret = axvisor_adapter_load_host_fdt_from_path();
	if (ret < 0)
		pr_err("axvisor_adapter: failed to load host DTB from %s: %d\n",
		       axvisor_host_fdt_path, ret);
	else if (axvisor_host_fdt_vaddr)
		pr_info("axvisor_adapter: host DTB prepared from %s at vaddr=0x%llx size=%zu\n",
			axvisor_host_fdt_path, axvisor_host_fdt_vaddr,
			axvisor_host_fdt_size);

	return ret;
}

static struct axvisor_memory_record *axvisor_memory_record_find_locked(u64 paddr)
{
	struct axvisor_memory_record *record;

	list_for_each_entry(record, &axvisor_memory_records, node) {
		if (record->paddr == paddr)
			return record;
	}

	return NULL;
}

static struct axvisor_memory_record *axvisor_memory_record_lookup_paddr_locked(u64 paddr)
{
	struct axvisor_memory_record *record;

	list_for_each_entry(record, &axvisor_memory_records, node) {
		u64 begin = record->paddr;
		u64 end = begin + record->size_bytes;

		if (paddr >= begin && paddr < end)
			return record;
	}

	return NULL;
}

static struct axvisor_memory_record *
axvisor_memory_record_lookup_paddr_kind_locked(u64 paddr,
					       enum axvisor_memory_kind kind)
{
	struct axvisor_memory_record *record;

	list_for_each_entry(record, &axvisor_memory_records, node) {
		u64 begin = record->paddr;
		u64 end = begin + record->size_bytes;

		if (record->kind == kind && paddr >= begin && paddr < end)
			return record;
	}

	return NULL;
}

static struct axvisor_memory_record *
axvisor_memory_record_lookup_paddr_range_kind_locked(u64 paddr, u64 size,
						     enum axvisor_memory_kind kind)
{
	struct axvisor_memory_record *record;
	u64 end;

	if (!size || check_add_overflow(paddr, size, &end))
		return NULL;

	list_for_each_entry(record, &axvisor_memory_records, node) {
		u64 begin = record->paddr;
		u64 record_end;

		if (record->kind != kind ||
		    check_add_overflow(begin, (u64)record->size_bytes, &record_end))
			continue;
		if (paddr >= begin && end <= record_end)
			return record;
	}

	return NULL;
}

static struct axvisor_memory_record *axvisor_memory_record_lookup_vaddr_locked(u64 vaddr)
{
	struct axvisor_memory_record *record;

	list_for_each_entry(record, &axvisor_memory_records, node) {
		size_t size = record->size_bytes;
		u64 begin = record->vaddr;
		u64 end = begin + size;

		if (vaddr >= begin && vaddr < end)
			return record;
	}

	return NULL;
}

static bool axvisor_memory_record_insert(struct axvisor_memory_record *record)
{
	unsigned long flags;

	spin_lock_irqsave(&axvisor_memory_records_lock, flags);
	if (axvisor_memory_record_find_locked(record->paddr)) {
		spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
		return false;
	}
	list_add_tail(&record->node, &axvisor_memory_records);
	spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
	return true;
}

static struct axvisor_memory_record *axvisor_memory_record_remove(u64 paddr)
{
	struct axvisor_memory_record *record;
	unsigned long flags;

	spin_lock_irqsave(&axvisor_memory_records_lock, flags);
	record = axvisor_memory_record_find_locked(paddr);
	if (record)
		list_del(&record->node);
	spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
	return record;
}

static bool axvisor_guest_ram_contains(u64 paddr)
{
	unsigned int i;
	unsigned long flags;

	spin_lock_irqsave(&axvisor_guest_ram_regions_lock, flags);
	for (i = 0; i < axvisor_guest_ram_region_count; i++) {
		u64 begin = axvisor_guest_ram_regions[i].paddr;
		u64 size = axvisor_guest_ram_regions[i].size;
		u64 end;

		if (!size || check_add_overflow(begin, size, &end))
			continue;
		if (paddr >= begin && paddr < end) {
			spin_unlock_irqrestore(&axvisor_guest_ram_regions_lock,
					       flags);
			return true;
		}
	}
	spin_unlock_irqrestore(&axvisor_guest_ram_regions_lock, flags);
	return false;
}

static bool axvisor_guest_ram_contains_range(u64 paddr, u64 size)
{
	u64 end;
	unsigned int i;
	unsigned long flags;

	if (!size || check_add_overflow(paddr, size, &end))
		return false;

	spin_lock_irqsave(&axvisor_guest_ram_regions_lock, flags);
	for (i = 0; i < axvisor_guest_ram_region_count; i++) {
		u64 begin = axvisor_guest_ram_regions[i].paddr;
		u64 region_size = axvisor_guest_ram_regions[i].size;
		u64 region_end;

		if (!region_size ||
		    check_add_overflow(begin, region_size, &region_end))
			continue;
		if (paddr >= begin && end <= region_end) {
			spin_unlock_irqrestore(&axvisor_guest_ram_regions_lock,
					       flags);
			return true;
		}
	}
	spin_unlock_irqrestore(&axvisor_guest_ram_regions_lock, flags);
	return false;
}

static bool axvisor_adapter_memremap_guest_ram_range(u64 paddr, u64 size)
{
	struct axvisor_memory_record *record;
	unsigned long flags;
	void *mapped;
	u64 base, offset, map_size_u64;
	size_t map_size;

	if (!paddr || !size || !axvisor_guest_ram_contains_range(paddr, size))
		return false;

	base = ALIGN_DOWN(paddr, PAGE_SIZE);
	offset = paddr - base;
	if (check_add_overflow(offset, size, &map_size_u64))
		return false;
	map_size_u64 = PAGE_ALIGN(map_size_u64);
	if (map_size_u64 > SIZE_MAX)
		return false;
	map_size = (size_t)map_size_u64;

	spin_lock_irqsave(&axvisor_memory_records_lock, flags);
	record = axvisor_memory_record_lookup_paddr_range_kind_locked(
		paddr, size, AXVISOR_MEM_REMAP);
	if (record) {
		spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
		return true;
	}
	spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);

	mapped = memremap((phys_addr_t)base, map_size, MEMREMAP_WB);
	if (!mapped) {
		pr_err("axvisor_adapter: guest RAM memremap failed pa=0x%llx base=0x%llx size=0x%zx\n",
		       paddr, base, map_size);
		return false;
	}

	record = kmalloc(sizeof(*record), GFP_KERNEL);
	if (!record) {
		memunmap(mapped);
		return false;
	}

	record->paddr = base;
	record->vaddr = (u64)(unsigned long)mapped;
	record->raw_vaddr = record->vaddr;
	record->num_frames = map_size / PAGE_SIZE;
	record->size_bytes = map_size;
	record->alloc_size_bytes = map_size;
	record->kind = AXVISOR_MEM_REMAP;
	record->page = NULL;
	INIT_LIST_HEAD(&record->node);

	if (!axvisor_memory_record_insert(record)) {
		kfree(record);
		memunmap(mapped);

		spin_lock_irqsave(&axvisor_memory_records_lock, flags);
		record = axvisor_memory_record_lookup_paddr_range_kind_locked(
			paddr, size, AXVISOR_MEM_REMAP);
		spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
		return !!record;
	}

	pr_info("axvisor_adapter: memremap guest RAM pa=0x%llx base=0x%llx size=0x%zx -> va=%px\n",
		paddr, base, map_size, mapped);
	return true;
}

bool axvisor_adapter_register_guest_ram(u64 paddr, u64 size)
{
	unsigned int i;
	unsigned long flags;
	u64 end;

	if (!paddr || !size || check_add_overflow(paddr, size, &end))
		return false;

	spin_lock_irqsave(&axvisor_guest_ram_regions_lock, flags);
	for (i = 0; i < axvisor_guest_ram_region_count; i++) {
		struct axvisor_guest_ram_region *region =
			&axvisor_guest_ram_regions[i];
		u64 region_end;

		if (check_add_overflow(region->paddr, region->size, &region_end))
			continue;
		if (region->paddr == paddr && region->size == size) {
			spin_unlock_irqrestore(&axvisor_guest_ram_regions_lock,
					       flags);
			return axvisor_adapter_memremap_guest_ram_range(paddr, size);
		}
		if (paddr >= region->paddr && end <= region_end) {
			spin_unlock_irqrestore(&axvisor_guest_ram_regions_lock,
					       flags);
			return axvisor_adapter_memremap_guest_ram_range(paddr, size);
		}
	}

	if (axvisor_guest_ram_region_count >= AXVISOR_MAX_GUEST_RAM_REGIONS) {
		spin_unlock_irqrestore(&axvisor_guest_ram_regions_lock, flags);
		pr_err("axvisor_adapter: guest RAM registry full pa=0x%llx size=0x%llx\n",
		       paddr, size);
		return false;
	}

	axvisor_guest_ram_regions[axvisor_guest_ram_region_count].paddr = paddr;
	axvisor_guest_ram_regions[axvisor_guest_ram_region_count].size = size;
	axvisor_guest_ram_region_count++;
	spin_unlock_irqrestore(&axvisor_guest_ram_regions_lock, flags);

	pr_info("axvisor_adapter: registered guest RAM pa=0x%llx size=0x%llx\n",
		paddr, size);
	return axvisor_adapter_memremap_guest_ram_range(paddr, size);
}

static struct axvisor_memory_record *axvisor_memory_record_lookup(u64 paddr)
{
	struct axvisor_memory_record *record;
	unsigned long flags;

	spin_lock_irqsave(&axvisor_memory_records_lock, flags);
	record = axvisor_memory_record_find_locked(paddr);
	spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
	return record;
}

static void *axvisor_memory_record_kernel_ptr_from_paddr_locked(
	struct axvisor_memory_record *record, u64 paddr)
{
	u64 offset;

	if (!record)
		return NULL;

	offset = paddr - record->paddr;
	if (offset >= record->size_bytes)
		return NULL;

	return (void *)(unsigned long)(record->raw_vaddr + offset);
}

static void *axvisor_adapter_paddr_to_kernel_ptr(u64 paddr)
{
	struct axvisor_memory_record *record;
	unsigned long flags;
	u64 vaddr;

	if (!paddr)
		return NULL;

	spin_lock_irqsave(&axvisor_memory_records_lock, flags);
	record = axvisor_memory_record_lookup_paddr_locked(paddr);
	if (record) {
		void *ptr = axvisor_memory_record_kernel_ptr_from_paddr_locked(record, paddr);

		spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
		return ptr;
	}
	spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);

	vaddr = axvisor_adapter_phys_to_virt(paddr);
	if (!vaddr)
		return NULL;

	return (void *)(unsigned long)vaddr;
}

static bool axvisor_paddr_is_host_plic(u64 paddr)
{
	u64 end;

	if (!IS_ENABLED(CONFIG_RISCV))
		return false;

	if (!axvisor_plic_paddr || !axvisor_plic_size)
		return false;
	if (check_add_overflow((u64)axvisor_plic_paddr,
			       (u64)axvisor_plic_size, &end))
		return false;
	return paddr >= axvisor_plic_paddr && paddr < end;
}

static bool axvisor_paddr_is_passthrough_mmio(u64 paddr)
{
	size_t count, i;

	count = axvisor_linux_passthrough_device_count();
	for (i = 0; i < count; i++) {
		u64 base = axvisor_linux_passthrough_device_base_hpa(i);
		u64 length = 0;
		u64 end;

		if (!base)
			continue;

		/*
		 * Length comes from the Rust passthrough registry populated after
		 * host FDT parsing. Fall back to one page only for older entries.
		 */
		length = axvisor_linux_passthrough_device_length(i);
		if (!length)
			length = PAGE_SIZE;
		if (check_add_overflow(base, length, &end))
			continue;
		if (paddr >= base && paddr < end)
			return true;
	}

	return false;
}

static bool axvisor_log_count_visible(s64 count)
{
	return count <= 64 || is_power_of_2((unsigned long)count);
}

static int axvisor_passthrough_device_index_for_base(u64 base)
{
	size_t count, i;

	if (!base)
		return -ENOENT;

	count = axvisor_linux_passthrough_device_count();
	for (i = 0; i < count; i++) {
		if (axvisor_linux_passthrough_device_base_hpa(i) == base)
			return i;
	}

	return -ENOENT;
}

static struct axvisor_passthrough_irq_record *
axvisor_passthrough_irq_record_for_base_locked(u64 base)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(axvisor_passthrough_irqs); i++) {
		if (axvisor_passthrough_irqs[i].requested &&
		    axvisor_passthrough_irqs[i].base == base)
			return &axvisor_passthrough_irqs[i];
	}

	return NULL;
}

static struct axvisor_passthrough_irq_record *
axvisor_passthrough_irq_record_for_irq_locked(unsigned int virq,
					     unsigned int guest_irq)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(axvisor_passthrough_irqs); i++) {
		if (!axvisor_passthrough_irqs[i].requested)
			continue;
		if (axvisor_passthrough_irqs[i].virq == virq ||
		    axvisor_passthrough_irqs[i].guest_irq == guest_irq)
			return &axvisor_passthrough_irqs[i];
	}

	return NULL;
}

static struct axvisor_passthrough_irq_record *
axvisor_passthrough_irq_record_for_guest_irq_locked(unsigned int guest_irq)
{
	unsigned int i;

	if (!guest_irq)
		return NULL;

	for (i = 0; i < ARRAY_SIZE(axvisor_passthrough_irqs); i++) {
		if (!axvisor_passthrough_irqs[i].requested)
			continue;
		if (axvisor_passthrough_irqs[i].guest_irq == guest_irq)
			return &axvisor_passthrough_irqs[i];
	}

	return NULL;
}

static struct axvisor_passthrough_irq_record *
axvisor_passthrough_irq_alloc_record_locked(void)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(axvisor_passthrough_irqs); i++) {
		if (!axvisor_passthrough_irqs[i].requested)
			return &axvisor_passthrough_irqs[i];
	}

	return NULL;
}

#ifdef CONFIG_X86
static void axvisor_x86_log_pci_intx_state(struct pci_dev *pdev, const char *where,
					   unsigned int virq,
					   unsigned int guest_irq)
{
	u16 command = 0;
	u16 status = 0;
	s64 count;

	if (!pdev)
		return;

	count = atomic64_inc_return(&axvisor_x86_intx_state_log_count);
	if (!axvisor_log_count_visible(count))
		return;

	pci_read_config_word(pdev, PCI_COMMAND, &command);
	pci_read_config_word(pdev, PCI_STATUS, &status);
	pr_info("axvisor_adapter: x86 INTx state %s pci=%s virq=%u guest_gsi=%u command=0x%04x status=0x%04x intx_disabled=%u interrupt_pending=%u msi=%u msix=%u count=%lld\n",
		where, pci_name(pdev), virq, guest_irq, command, status,
		!!(command & PCI_COMMAND_INTX_DISABLE),
		!!(status & PCI_STATUS_INTERRUPT),
		pdev->msi_enabled, pdev->msix_enabled, count);
}
#endif

static irqreturn_t axvisor_passthrough_irq_handler(int virq, void *dev_id)
{
	struct axvisor_passthrough_irq_record *record = dev_id;
	bool pending = false;
	bool host_irq_masked = false;
	s64 log_count;

	if (!record || !record->guest_irq)
		return IRQ_NONE;

#ifdef CONFIG_X86
	if (record->pci_dev) {
		bool intx_pending;
		unsigned long flags;

		axvisor_x86_log_pci_intx_state(record->pci_dev, "handler-enter",
					       (unsigned int)virq,
					       record->guest_irq);
		intx_pending = pci_check_and_mask_intx(record->pci_dev);
		if (!intx_pending) {
			axvisor_x86_log_pci_intx_state(record->pci_dev,
						       "handler-not-pending",
						       (unsigned int)virq,
						       record->guest_irq);
			return IRQ_NONE;
		}
		host_irq_masked = true;
		spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
		if (record->requested)
			record->host_irq_masked = true;
		spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);
	}
#endif

	if (!host_irq_masked) {
		unsigned long flags;

		disable_irq_nosync((unsigned int)virq);
		host_irq_masked = true;
		spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
		if (record->requested)
			record->host_irq_masked = true;
		spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);
	}

	pending = axvisor_linux_passthrough_irq_mark_pending(record->guest_irq);

	log_count = atomic64_inc_return(pending ?
					&axvisor_passthrough_irq_log_count :
					&axvisor_passthrough_irq_fail_log_count);
	if (axvisor_log_count_visible(log_count)) {
		pr_info("axvisor_adapter: passthrough irq handler virq=%d hwirq=%lu guest_irq=%u base=0x%llx pending=%d masked_now=%d irq_count=%lld\n",
			virq, record->hwirq, record->guest_irq, record->base,
			pending, host_irq_masked, log_count);
	}

	/*
	 * The Linux irqchip layer has already claimed the physical interrupt
	 * before invoking this handler. Treat the line as handled even if
	 * AxVisor injection failed, otherwise the host can disable the IRQ as
	 * spurious and hide the real failure mode.
	 */
	return IRQ_HANDLED;
}

#ifdef CONFIG_X86
static bool axvisor_x86_passthrough_irq_record_matches_vector(
	struct axvisor_passthrough_irq_record *record, u32 vector)
{
	struct irq_data *irq_data;
	struct irq_cfg *cfg;

	if (!record || !record->requested || !record->host_irq_requested ||
	    !record->guest_irq)
		return false;

	if (record->host_vector && record->host_vector == vector)
		return true;

	irq_data = irq_get_irq_data(record->virq);
	cfg = irqd_cfg(irq_data);
	if (!cfg)
		return false;

	return cfg->vector == vector;
}

bool axvisor_adapter_x86_passthrough_irq_handle_vector(u32 vector)
{
	struct axvisor_passthrough_irq_record *record = NULL;
	unsigned long flags;
	unsigned int virq = 0;
	unsigned int guest_irq = 0;
	unsigned long hwirq = 0;
	irqreturn_t ret;
	s64 log_count;
	int i;

	spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
	for (i = 0; i < ARRAY_SIZE(axvisor_passthrough_irqs); i++) {
		if (axvisor_x86_passthrough_irq_record_matches_vector(
			    &axvisor_passthrough_irqs[i], vector)) {
			record = &axvisor_passthrough_irqs[i];
			virq = record->virq;
			guest_irq = record->guest_irq;
			hwirq = record->hwirq;
			break;
		}
	}
	spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);

	log_count = atomic64_inc_return(&axvisor_x86_passthrough_vector_log_count);
	if (!record) {
		if (axvisor_log_count_visible(log_count))
			pr_info("axvisor_adapter: x86 passthrough vector miss vector=%u count=%lld\n",
				vector, log_count);
		return false;
	}

	ret = axvisor_passthrough_irq_handler((int)virq, record);
	if (axvisor_log_count_visible(log_count)) {
		pr_info("axvisor_adapter: x86 passthrough vector hit vector=%u virq=%u hwirq=%lu guest_gsi=%u handled=%d count=%lld\n",
			vector, virq, hwirq, guest_irq, ret == IRQ_HANDLED,
			log_count);
	}

	return ret == IRQ_HANDLED;
}

bool axvisor_adapter_x86_passthrough_irq_poll(u32 irq_id)
{
	struct axvisor_passthrough_irq_record *record;
	struct pci_dev *pci_dev = NULL;
	unsigned long flags;
	unsigned int virq = 0;
	unsigned long hwirq = 0;
	u64 base = 0;
	bool registered = false;
	bool pending = false;
	bool masked = false;
	s64 log_count;

	if (!irq_id)
		return false;

	spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
	record = axvisor_passthrough_irq_record_for_guest_irq_locked(irq_id);
	if (record && record->requested && record->host_irq_requested) {
		registered = true;
		virq = record->virq;
		hwirq = record->hwirq;
		base = record->base;
		pci_dev = record->pci_dev;
	}
	spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);

	if (!registered || !pci_dev)
		return false;

	pending = pci_check_and_mask_intx(pci_dev);
	if (pending) {
		spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
		record = axvisor_passthrough_irq_record_for_guest_irq_locked(irq_id);
		if (record && record->requested) {
			record->host_irq_masked = true;
			masked = true;
		}
		spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);
	}

	log_count = atomic64_inc_return(&axvisor_x86_passthrough_poll_log_count);
	if (pending || axvisor_log_count_visible(log_count)) {
		pr_info("axvisor_adapter: x86 passthrough irq poll irq=%u virq=%u hwirq=%lu base=0x%llx pending=%d masked=%d count=%lld\n",
			irq_id, virq, hwirq, base, pending, masked, log_count);
		if (pending)
			axvisor_x86_log_pci_intx_state(pci_dev, "poll-pending",
						       virq, irq_id);
	}

	return pending;
}
#else
bool axvisor_adapter_x86_passthrough_irq_handle_vector(u32 vector)
{
	return false;
}

bool axvisor_adapter_x86_passthrough_irq_poll(u32 irq_id)
{
	return false;
}
#endif

static int axvisor_adapter_request_passthrough_irq(struct platform_device *pdev,
						  u64 base)
{
	struct axvisor_passthrough_irq_record *record;
	unsigned long flags;
	struct irq_data *irq_data;
	unsigned long hwirq = 0;
	unsigned int guest_irq;
	int index;
	int virq;

	index = axvisor_passthrough_device_index_for_base(base);
	if (index < 0) {
		pr_info("axvisor_adapter: skip passthrough irq request base=0x%llx no registered device\n",
			base);
		return 0;
	}

	guest_irq = (unsigned int)axvisor_linux_passthrough_device_irq_id(index);
	if (!guest_irq) {
		pr_info("axvisor_adapter: skip passthrough irq request base=0x%llx no guest irq\n",
			base);
		return 0;
	}

	virq = platform_get_irq(pdev, 0);
	if (virq < 0) {
		pr_err("axvisor_adapter: platform_get_irq failed base=0x%llx ret=%d\n",
		       base, virq);
		return virq;
	}

	irq_data = irq_get_irq_data((unsigned int)virq);
	if (irq_data)
		hwirq = irqd_to_hwirq(irq_data);

	spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
	record = axvisor_passthrough_irq_record_for_base_locked(base);
	if (record) {
		bool already_ready;

		already_ready = record->host_irq_requested;
		spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);
		pr_info("axvisor_adapter: passthrough irq already prepared base=0x%llx virq=%u guest_irq=%u hwirq=%lu ready=%d\n",
			base, record->virq, record->guest_irq, record->hwirq,
			already_ready);
		return 0;
	}
	record = axvisor_passthrough_irq_record_for_irq_locked((unsigned int)virq,
						       guest_irq);
	if (record) {
		spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);
		pr_info("axvisor_adapter: passthrough irq already prepared virq=%d guest_irq=%u existing_base=0x%llx\n",
			virq, guest_irq, record->base);
		return 0;
	}
	record = axvisor_passthrough_irq_alloc_record_locked();
	if (!record) {
		spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);
		pr_err("axvisor_adapter: passthrough irq registry full base=0x%llx virq=%d guest_irq=%u\n",
		       base, virq, guest_irq);
		return -ENOSPC;
	}
	record->virq = (unsigned int)virq;
	record->guest_irq = guest_irq;
	record->hwirq = hwirq;
	record->base = base;
	record->requested = true;
	record->host_irq_requested = false;
	record->host_irq_masked = false;
	spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);

	{
		int ret;

		ret = request_irq((unsigned int)virq, axvisor_passthrough_irq_handler,
				  IRQF_SHARED, "axvisor-passthrough", record);
		if (ret < 0) {
			spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
			memset(record, 0, sizeof(*record));
			spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);
			pr_err("axvisor_adapter: request_irq failed base=0x%llx virq=%d hwirq=%lu guest_irq=%u ret=%d\n",
			       base, virq, hwirq, guest_irq, ret);
			return ret;
		}

		spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
		record = axvisor_passthrough_irq_record_for_base_locked(base);
		if (record)
			record->host_irq_requested = true;
		spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);

		pr_info("axvisor_adapter: requested passthrough irq base=0x%llx virq=%d hwirq=%lu guest_irq=%u%s\n",
			base, virq, hwirq, guest_irq,
			IS_ENABLED(CONFIG_RISCV) ? " on riscv via Linux IRQ core" : "");
		return 0;
	}
}

#ifdef CONFIG_X86
int axvisor_adapter_request_x86_qemu_blk_intx(void)
{
	struct axvisor_passthrough_irq_record *record;
	struct pci_dev *pdev = NULL;
	struct irq_data *irq_data;
	unsigned long flags;
	unsigned long hwirq = 0;
	unsigned int virq;
	u64 synthetic_base;
	int ret;

	if (!axvisor_x86_register_qemu_blk_intx) {
		pr_info("axvisor_adapter: x86 qemu blk INTx registration disabled\n");
		return 0;
	}

	if (!axvisor_x86_qemu_blk_guest_gsi) {
		pr_info("axvisor_adapter: x86 qemu blk INTx skipped guest_gsi=0\n");
		return 0;
	}

	if (axvisor_x86_qemu_blk_bus > 255 ||
	    axvisor_x86_qemu_blk_dev > 31 ||
	    axvisor_x86_qemu_blk_func > 7)
		return -EINVAL;

	synthetic_base = 0x5850494300000000ULL |
			 ((u64)axvisor_x86_qemu_blk_bus << 16) |
			 ((u64)axvisor_x86_qemu_blk_dev << 8) |
			 (u64)axvisor_x86_qemu_blk_func;

	pdev = pci_get_domain_bus_and_slot(0, axvisor_x86_qemu_blk_bus,
					   PCI_DEVFN(axvisor_x86_qemu_blk_dev,
						     axvisor_x86_qemu_blk_func));
	if (!pdev) {
		pr_warn("axvisor_adapter: x86 qemu blk INTx PCI device 0000:%02x:%02x.%u not found\n",
			axvisor_x86_qemu_blk_bus, axvisor_x86_qemu_blk_dev,
			axvisor_x86_qemu_blk_func);
		return -ENODEV;
	}

	ret = pci_enable_device(pdev);
	if (ret < 0) {
		pr_err("axvisor_adapter: x86 qemu blk INTx pci_enable_device failed pci=%s ret=%d\n",
		       pci_name(pdev), ret);
		goto out_put;
	}

	pci_set_master(pdev);
	pci_intx(pdev, 0);
	axvisor_x86_log_pci_intx_state(pdev, "prepare-disabled", 0,
				       axvisor_x86_qemu_blk_guest_gsi);

	ret = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_INTX);
	if (ret < 0) {
		pr_err("axvisor_adapter: x86 qemu blk INTx pci_alloc_irq_vectors failed pci=%s irq=%u ret=%d\n",
		       pci_name(pdev), pdev->irq, ret);
		goto out_disable;
	}

	ret = pci_irq_vector(pdev, 0);
	if (ret < 0) {
		pr_err("axvisor_adapter: x86 qemu blk INTx pci_irq_vector failed pci=%s ret=%d\n",
		       pci_name(pdev), ret);
		goto out_free_vectors;
	}
	virq = (unsigned int)ret;

	irq_data = irq_get_irq_data(virq);
	if (irq_data)
		hwirq = irqd_to_hwirq(irq_data);

	spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
	record = axvisor_passthrough_irq_record_for_guest_irq_locked(
		axvisor_x86_qemu_blk_guest_gsi);
	if (record) {
		spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);
		pr_info("axvisor_adapter: x86 qemu blk INTx already prepared pci=%s virq=%u guest_gsi=%u base=0x%llx\n",
			pci_name(pdev), record->virq, record->guest_irq,
			record->base);
		ret = 0;
		goto out_free_vectors;
	}

	record = axvisor_passthrough_irq_alloc_record_locked();
	if (!record) {
		spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);
		pr_err("axvisor_adapter: x86 qemu blk INTx registry full pci=%s virq=%u guest_gsi=%u\n",
		       pci_name(pdev), virq, axvisor_x86_qemu_blk_guest_gsi);
		ret = -ENOSPC;
		goto out_free_vectors;
	}

	record->virq = virq;
	record->guest_irq = axvisor_x86_qemu_blk_guest_gsi;
	record->hwirq = hwirq;
	record->host_vector = 0;
	record->base = synthetic_base;
#ifdef CONFIG_X86
	record->pci_dev = pdev;
	record->pci_irq_vectors_allocated = true;
	record->pci_device_enabled = true;
#endif
	record->requested = true;
	record->host_irq_requested = false;
	record->host_irq_masked = false;
	spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);

	ret = request_irq(virq, axvisor_passthrough_irq_handler, IRQF_SHARED,
			  "axvisor-x86-qemu-blk-intx", record);
	if (ret < 0) {
		spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
		memset(record, 0, sizeof(*record));
		spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);
		pr_err("axvisor_adapter: x86 qemu blk INTx request_irq failed pci=%s virq=%u hwirq=%lu guest_gsi=%u ret=%d\n",
		       pci_name(pdev), virq, hwirq,
		       axvisor_x86_qemu_blk_guest_gsi, ret);
		goto out_free_vectors;
	}

	spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
	record = axvisor_passthrough_irq_record_for_base_locked(synthetic_base);
	if (record) {
		struct irq_cfg *cfg = irqd_cfg(irq_get_irq_data(virq));

		record->host_irq_requested = true;
		if (cfg)
			record->host_vector = cfg->vector;
	}
	spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);

	pci_intx(pdev, 0);
	pr_info("axvisor_adapter: requested x86 qemu blk INTx pci=%s virq=%u hwirq=%lu host_vector=%u guest_gsi=%u base=0x%llx\n",
		pci_name(pdev), virq, hwirq,
		record ? record->host_vector : 0,
		axvisor_x86_qemu_blk_guest_gsi, synthetic_base);
	axvisor_x86_log_pci_intx_state(pdev, "request-success-disabled", virq,
				       axvisor_x86_qemu_blk_guest_gsi);
	ret = 0;
	goto out_keep_pci;

out_free_vectors:
	pci_free_irq_vectors(pdev);
out_disable:
	pci_disable_device(pdev);
out_put:
	pci_dev_put(pdev);
out_keep_pci:
	return ret;
}
#else
int axvisor_adapter_request_x86_qemu_blk_intx(void)
{
	return 0;
}
#endif

#ifdef CONFIG_X86
bool axvisor_adapter_x86_passthrough_irq_unmask(u32 irq_id)
{
	struct axvisor_passthrough_irq_record *record;
	struct pci_dev *pci_dev = NULL;
	unsigned long flags;
	unsigned int virq = 0;
	unsigned long hwirq = 0;
	u64 base = 0;
	bool registered = false;
	bool host_irq_requested = false;
	bool host_irq_masked_before = false;
	bool reenable_irq = false;
	bool unmasked = true;
	s64 log_count;

	if (!irq_id)
		return false;

	spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
	record = axvisor_passthrough_irq_record_for_guest_irq_locked(irq_id);
	if (record) {
		registered = true;
		virq = record->virq;
		hwirq = record->hwirq;
		base = record->base;
		pci_dev = record->pci_dev;
		host_irq_requested = record->host_irq_requested;
		host_irq_masked_before = record->host_irq_masked;
	}
	spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);

	if (!registered) {
		pr_info("axvisor_adapter: x86 passthrough irq unmask ignored non-passthrough irq=%u\n",
			irq_id);
		return false;
	}

	if (pci_dev) {
		unmasked = pci_check_and_unmask_intx(pci_dev);
		axvisor_x86_log_pci_intx_state(pci_dev,
					       unmasked ? "unmask-ready" :
							  "unmask-still-pending",
					       virq, irq_id);
	}

	spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
	record = axvisor_passthrough_irq_record_for_guest_irq_locked(irq_id);
	if (record && record->host_irq_requested && record->host_irq_masked &&
	    unmasked) {
		record->host_irq_masked = false;
		/*
		 * PCI INTx forwarding masks the device line with
		 * pci_check_and_mask_intx(); it does not disable the Linux IRQ
		 * descriptor. Only pair enable_irq() with the non-PCI path that
		 * used disable_irq_nosync().
		 */
		reenable_irq = !pci_dev;
	}
	spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);

	if (reenable_irq)
		enable_irq(virq);

	log_count = atomic64_inc_return(&axvisor_x86_passthrough_complete_log_count);
	if (axvisor_log_count_visible(log_count)) {
		pr_info("axvisor_adapter: x86 passthrough irq unmask irq=%u virq=%u hwirq=%lu base=0x%llx host_requested=%d masked_before=%d unmasked=%d reenable=%d count=%lld\n",
			irq_id, virq, hwirq, base, host_irq_requested,
			host_irq_masked_before, unmasked, reenable_irq,
			log_count);
	}

	return unmasked;
}
#else
bool axvisor_adapter_x86_passthrough_irq_unmask(u32 irq_id)
{
	return false;
}
#endif

void axvisor_adapter_release_passthrough_irqs(void)
{
	struct axvisor_passthrough_irq_record records[AXVISOR_MAX_PASSTHROUGH_IRQS];
	struct axvisor_passthrough_irq_record *dev_ids[AXVISOR_MAX_PASSTHROUGH_IRQS];
	unsigned long flags;
	unsigned int i;

	spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
	memcpy(records, axvisor_passthrough_irqs, sizeof(records));
	for (i = 0; i < ARRAY_SIZE(dev_ids); i++)
		dev_ids[i] = &axvisor_passthrough_irqs[i];
	memset(axvisor_passthrough_irqs, 0, sizeof(axvisor_passthrough_irqs));
	spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);

	for (i = 0; i < ARRAY_SIZE(records); i++) {
		if (!records[i].requested)
			continue;
		if (records[i].host_irq_requested && records[i].host_irq_masked) {
#ifdef CONFIG_X86
			if (records[i].pci_dev)
				pci_check_and_unmask_intx(records[i].pci_dev);
			else
#endif
				enable_irq(records[i].virq);
		}
		if (records[i].host_irq_requested) {
			free_irq(records[i].virq, dev_ids[i]);
			pr_info("axvisor_adapter: freed passthrough irq base=0x%llx virq=%u guest_irq=%u\n",
				records[i].base, records[i].virq, records[i].guest_irq);
		}
#ifdef CONFIG_X86
		if (records[i].pci_dev && records[i].pci_irq_vectors_allocated)
			pci_free_irq_vectors(records[i].pci_dev);
		if (records[i].pci_dev && records[i].pci_device_enabled)
			pci_disable_device(records[i].pci_dev);
		if (records[i].pci_dev) {
			pr_info("axvisor_adapter: released passthrough pci device %s\n",
				pci_name(records[i].pci_dev));
			pci_dev_put(records[i].pci_dev);
		}
#endif
	}
}

static u64 axvisor_adapter_ioremap_range(u64 paddr, size_t size)
{
	struct axvisor_memory_record *record;
	unsigned long flags;
	u64 base, offset, map_size;
	void __iomem *mapped;

	if (!paddr || !size)
		return 0;

	base = ALIGN_DOWN(paddr, PAGE_SIZE);
	offset = paddr - base;
	map_size = PAGE_ALIGN(offset + size);

	spin_lock_irqsave(&axvisor_memory_records_lock, flags);
	record = axvisor_memory_record_lookup_paddr_kind_locked(paddr,
							       AXVISOR_MEM_IOREMAP);
	if (record && record->kind == AXVISOR_MEM_IOREMAP &&
	    paddr >= record->paddr &&
	    size <= record->size_bytes - (paddr - record->paddr)) {
		u64 vaddr = record->vaddr + (paddr - record->paddr);

		spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
		return vaddr;
	}
	spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);

	mapped = ioremap((phys_addr_t)base, map_size);
	if (!mapped) {
		pr_err("axvisor_adapter: ioremap failed pa=0x%llx base=0x%llx size=0x%llx\n",
		       paddr, base, map_size);
		return 0;
	}

	record = kmalloc(sizeof(*record), GFP_KERNEL);
	if (!record) {
		iounmap(mapped);
		return 0;
	}

	record->paddr = base;
	record->vaddr = (u64)(unsigned long)mapped;
	record->raw_vaddr = record->vaddr;
	record->num_frames = map_size / PAGE_SIZE;
	record->size_bytes = map_size;
	record->alloc_size_bytes = map_size;
	record->kind = AXVISOR_MEM_IOREMAP;
	record->page = NULL;
	INIT_LIST_HEAD(&record->node);

	if (!axvisor_memory_record_insert(record)) {
		kfree(record);
		iounmap(mapped);

		spin_lock_irqsave(&axvisor_memory_records_lock, flags);
		record = axvisor_memory_record_lookup_paddr_kind_locked(paddr,
								       AXVISOR_MEM_IOREMAP);
		if (record && record->kind == AXVISOR_MEM_IOREMAP &&
		    paddr >= record->paddr &&
		    size <= record->size_bytes - (paddr - record->paddr)) {
			u64 vaddr = record->vaddr + (paddr - record->paddr);

			spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
			return vaddr;
		}
		spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
		return 0;
	}

	pr_info("axvisor_adapter: ioremap host MMIO pa=0x%llx base=0x%llx size=0x%llx -> va=%px\n",
		paddr, base, map_size, mapped);
	return record->vaddr + offset;
}

static u64 axvisor_adapter_map_host_page(u64 paddr)
{
	struct axvisor_memory_record *record;
	unsigned long flags;
	u64 base, offset, map_size;
	void *mapped;

	if (!paddr)
		return 0;

	if (!axvisor_guest_ram_contains(paddr)) {
		pr_err("axvisor_adapter: refuse memremap for unregistered host pa=0x%llx\n",
		       paddr);
		return 0;
	}

	base = ALIGN_DOWN(paddr, PAGE_SIZE);
	offset = paddr - base;
	map_size = AXVISOR_HOST_REMAP_CHUNK_SIZE;

	if (IS_ALIGNED(base, AXVISOR_HOST_REMAP_CHUNK_SIZE))
		base = ALIGN_DOWN(base, AXVISOR_HOST_REMAP_CHUNK_SIZE);
	else
		map_size = PAGE_SIZE;

	offset = paddr - base;

	spin_lock_irqsave(&axvisor_memory_records_lock, flags);
	record = axvisor_memory_record_lookup_paddr_locked(base);
	if (record) {
		u64 vaddr = record->vaddr + offset;

		spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
		pr_debug("axvisor_adapter: map_host_page cache pa=0x%llx -> va=0x%llx kind=%d\n",
			 paddr, vaddr, record->kind);
		return vaddr;
	}
	spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);

	mapped = memremap((phys_addr_t)base, map_size, MEMREMAP_WB);
	if (!mapped)
		return 0;

	record = kmalloc(sizeof(*record), GFP_KERNEL);
	if (!record) {
		memunmap(mapped);
		return 0;
	}

	record->paddr = base;
	record->vaddr = (u64)(unsigned long)mapped;
	record->raw_vaddr = record->vaddr;
	record->num_frames = map_size / PAGE_SIZE;
	record->size_bytes = map_size;
	record->alloc_size_bytes = map_size;
	record->kind = AXVISOR_MEM_REMAP;
	record->page = NULL;
	INIT_LIST_HEAD(&record->node);

	if (!axvisor_memory_record_insert(record)) {
		kfree(record);
		memunmap(mapped);

		spin_lock_irqsave(&axvisor_memory_records_lock, flags);
		record = axvisor_memory_record_lookup_paddr_locked(base);
		if (record) {
			u64 vaddr = record->vaddr + offset;

			spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
			pr_debug("axvisor_adapter: map_host_page raced pa=0x%llx -> va=0x%llx kind=%d\n",
				 paddr, vaddr, record->kind);
			return vaddr;
		}
		spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
		return 0;
	}

	pr_debug("axvisor_adapter: memremap host range pa=0x%llx base=0x%llx size=0x%llx -> va=%px\n",
		 paddr, base, map_size, mapped);
	return ((u64)(unsigned long)mapped) + offset;
}

static const void *axvisor_adapter_runtime_buf_to_kernel_const_ptr(const u8 *buf)
{
	u64 paddr;
	const void *src;
	unsigned long addr;

	if (!buf)
		return NULL;

	addr = (unsigned long)buf;

	if (virt_addr_valid((void *)addr) || is_vmalloc_addr(buf) ||
	    ((long)addr < 0)) {
		pr_debug("axvisor_adapter: const buf host va=%px\n", buf);
		return buf;
	}

	if (object_is_on_stack(buf)) {
		pr_debug("axvisor_adapter: const buf stack va=%px\n", buf);
		return buf;
	}

	if (axvisor_runtime_alloc_contains_ptr(buf)) {
		pr_debug("axvisor_adapter: const buf runtime va=%px\n", buf);
		return buf;
	}

	paddr = axvisor_adapter_virt_to_phys((u64)(unsigned long)buf);
	if (!paddr) {
		pr_err("axvisor_adapter: const buf translation failed va=%px\n", buf);
		return NULL;
	}

	src = axvisor_adapter_paddr_to_kernel_ptr(paddr);
	pr_debug("axvisor_adapter: const buf translate va=%px -> pa=0x%llx -> src=%px\n",
		 buf, paddr, src);
	return src;
}

struct task_struct *axvisor_adapter_kthread_create(int (*threadfn)(void *data),
						   void *data,
						   const char *name)
{
	return kthread_create(threadfn, data, "%s", name);
}

void axvisor_adapter_kthread_bind(struct task_struct *task, unsigned int cpu)
{
	kthread_bind(task, cpu);
}

int axvisor_adapter_set_cpus_allowed_ptr(struct task_struct *task,
					 const struct cpumask *mask)
{
	return set_cpus_allowed_ptr(task, mask);
}

void axvisor_adapter_wake_up_process(struct task_struct *task)
{
	wake_up_process(task);
	/*
	 * A freshly kthread_bind()'d vCPU task may target a host CPU that is
	 * sitting in NOHZ tickless idle. Under nested virtualization the plain
	 * wake_up_process() reschedule IPI can be delayed long enough that the
	 * guest's AP-bringup (INIT-SIPI-SIPI) times out ("CPUx failed to report
	 * alive state"). kick_process() forces a reschedule IPI to the CPU the
	 * task now resides on, pulling it out of idle so the vCPU task actually
	 * gets to run promptly.
	 */
	kick_process(task);
}

int axvisor_adapter_kthread_stop(struct task_struct *task)
{
	return kthread_stop(task);
}

void axvisor_adapter_yield(void)
{
	cond_resched();
}

size_t axvisor_adapter_host_cpu_num(void)
{
	return num_online_cpus();
}

size_t axvisor_adapter_current_cpu_id(void)
{
	return smp_processor_id();
}

void axvisor_adapter_console_write(const u8 *buf, size_t len)
{
	const char *src;
	size_t remaining;
	char chunk[128];
	size_t copied = 0;
	long not_copied;

	if (!buf || !len)
		return;

	src = axvisor_adapter_runtime_buf_to_kernel_const_ptr(buf);
	if (!src) {
		pr_err("axvisor_adapter: console_write failed to translate buf=%px len=%zu\n",
		       buf, len);
		return;
	}

	remaining = len;
	while (remaining) {
		size_t step = min(remaining, sizeof(chunk));

		not_copied = copy_from_kernel_nofault(chunk, src, step);
		if (not_copied) {
			pr_err("axvisor_adapter: console_write nofault copy failed buf=%px src=%px len=%zu copied=%zu remaining=%zu not_copied=%ld\n",
			       buf, src, len, copied, remaining, not_copied);
			return;
		}
		printk(KERN_INFO "%.*s", (int)step, chunk);
		src += step;
		copied += step;
		remaining -= step;
	}
}

static void axvisor_adapter_guest_console_log_flush_locked(void)
{
	if (!axvisor_guest_console_log_len)
		return;

	printk(KERN_INFO "[axvisor-guest] %.*s\n",
	       (int)axvisor_guest_console_log_len,
	       axvisor_guest_console_log_line);
	axvisor_guest_console_log_len = 0;
}

static void axvisor_adapter_guest_console_log_bytes(const u8 *buf, size_t len)
{
	unsigned long flags;
	size_t i;

	if (!buf || !len)
		return;

	spin_lock_irqsave(&axvisor_guest_console_log_lock, flags);
	for (i = 0; i < len; i++) {
		u8 ch = buf[i];

		if (ch == '\n' || ch == '\r') {
			axvisor_adapter_guest_console_log_flush_locked();
			continue;
		}

		if (ch == '\t') {
			ch = ' ';
		} else if (ch < 0x20 || ch > 0x7e) {
			continue;
		}

		if (axvisor_guest_console_log_len >=
		    sizeof(axvisor_guest_console_log_line) - 1)
			axvisor_adapter_guest_console_log_flush_locked();

		axvisor_guest_console_log_line[axvisor_guest_console_log_len++] =
			ch;
	}
	spin_unlock_irqrestore(&axvisor_guest_console_log_lock, flags);
}

void axvisor_adapter_guest_console_write(const u8 *buf, size_t len)
{
	size_t written;

	if (!buf || !len)
		return;

	axvisor_adapter_guest_console_log_bytes(buf, len);

	written = axvisor_guest_console_ring_enqueue(&axvisor_guest_console_output,
						    buf, len, true);
	if (written != len)
		pr_warn("axvisor_adapter: guest_console_write truncated=%zu/%zu buffered=%zu\n",
			written, len,
			axvisor_guest_console_ring_len(&axvisor_guest_console_output));
	if (written)
		wake_up_interruptible(&axvisor_guest_console_output_wait);
}

size_t axvisor_adapter_guest_console_read(u8 *buf, size_t len)
{
	size_t read;

	if (!buf || !len)
		return 0;

	read = axvisor_guest_console_ring_drain(&axvisor_guest_console_input,
					       buf, len);
	if (read)
		wake_up_interruptible(&axvisor_guest_console_input_wait);

	return read;
}

void axvisor_adapter_guest_console_write_byte(unsigned long byte)
{
	u8 narrowed = (u8)(byte & 0xff);
	s64 log_idx = atomic64_fetch_inc(&axvisor_guest_console_write_byte_log_count);
	size_t written;

	if (log_idx < 8)
		pr_debug("axvisor_adapter: guest_console_write_byte[%lld] raw=0x%lx byte=0x%02x '%c'\n",
			 log_idx, byte, narrowed,
			 (narrowed >= 0x20 && narrowed <= 0x7e) ? narrowed : '.');

	written = axvisor_guest_console_ring_enqueue(&axvisor_guest_console_output,
						     &narrowed, 1, true);
	axvisor_adapter_guest_console_log_bytes(&narrowed, 1);
	if (written) {
		wake_up_interruptible(&axvisor_guest_console_output_wait);
	} else {
		pr_warn_ratelimited("axvisor_adapter: guest_console_write_byte enqueue failed raw=0x%lx\n",
				    byte);
	}
}

int axvisor_adapter_guest_console_read_byte(void)
{
	u8 byte;
	s64 log_idx;

	if (axvisor_guest_console_ring_drain(&axvisor_guest_console_input,
					     &byte, 1) != 1)
		return -1;
	wake_up_interruptible(&axvisor_guest_console_input_wait);

	log_idx = atomic64_fetch_inc(&axvisor_guest_console_read_byte_log_count);
	if (log_idx < 8)
		pr_debug("axvisor_adapter: guest_console_read_byte[%lld] byte=0x%02x '%c'\n",
			 log_idx, byte,
			 (byte >= 0x20 && byte <= 0x7e) ? byte : '.');

	return byte;
}

static int axvisor_match_platform_mmio_base(struct device *dev, const void *data)
{
	const u64 base = *(const u64 *)data;
	struct platform_device *pdev = to_platform_device(dev);
	struct resource *res;
	int i;

	for (i = 0; i < pdev->num_resources; i++) {
		res = &pdev->resource[i];
		if (resource_type(res) != IORESOURCE_MEM)
			continue;
		if ((u64)res->start == base)
			return 1;
	}

	return 0;
}

static int axvisor_adapter_release_platform_mmio_device(u64 base)
{
	struct device *dev;
	struct platform_device *pdev;
	bool is_virtio_mmio = false;
	int irq_ret = 0;

	if (!base)
		return 0;

	dev = bus_find_device(&platform_bus_type, NULL, &base,
			      axvisor_match_platform_mmio_base);
	if (!dev) {
		pr_info("axvisor_adapter: no platform MMIO device found at pa=0x%llx for release\n",
			base);
		return 0;
	}

	pdev = to_platform_device(dev);
	is_virtio_mmio =
		(dev->driver && !strcmp(dev->driver->name, "virtio-mmio")) ||
		!strcmp(pdev->name, "virtio-mmio") ||
		strstr(dev_name(dev), ".virtio_mmio") != NULL;

	if (!dev->driver) {
		pr_info("axvisor_adapter: platform MMIO device %s pa=0x%llx has no bound driver\n",
			dev_name(dev), base);
		irq_ret = axvisor_adapter_request_passthrough_irq(pdev, base);
		if (irq_ret < 0)
			pr_warn("axvisor_adapter: passthrough irq request failed for %s pa=0x%llx ret=%d\n",
				dev_name(dev), base, irq_ret);
		put_device(dev);
		return irq_ret < 0 ? irq_ret : 0;
	}

	pr_info("axvisor_adapter: releasing platform MMIO device %s driver=%s pa=0x%llx\n",
		dev_name(dev), dev->driver->name, base);
	device_release_driver(dev);

	if (is_virtio_mmio) {
		struct resource *res;
		void __iomem *mmio;
		u32 version = 0;
		u32 status_before = 0;
		u32 status_after = 0;
		u32 irq_status = 0;
		int irq;
		unsigned int tries;

		res = platform_get_resource(pdev, IORESOURCE_MEM, 0);
		if (!res) {
			pr_warn("axvisor_adapter: virtio-mmio release missing MEM resource base=0x%llx\n",
				base);
			goto out_put_dev;
		}

		mmio = ioremap(res->start, resource_size(res));
		if (!mmio) {
			pr_warn("axvisor_adapter: virtio-mmio release ioremap failed base=0x%llx size=0x%llx\n",
				base, (unsigned long long)resource_size(res));
			goto out_put_dev;
		}

		version = readl(mmio + VIRTIO_MMIO_VERSION);
		status_before = readl(mmio + VIRTIO_MMIO_STATUS) & 0xff;
		irq_status = readl(mmio + VIRTIO_MMIO_INTERRUPT_STATUS);
		if (irq_status)
			writel(irq_status, mmio + VIRTIO_MMIO_INTERRUPT_ACK);

		/*
		 * Linux virtio core leaves the transport in ACKNOWLEDGE state
		 * after driver removal. Reset it back to a clean status=0 so
		 * the guest always observes a fresh virtio-mmio handoff.
		 */
		writel(0, mmio + VIRTIO_MMIO_STATUS);
		for (tries = 0; tries < 100; tries++) {
			status_after = readl(mmio + VIRTIO_MMIO_STATUS) & 0xff;
			if (!status_after)
				break;
			udelay(10);
		}

		iounmap(mmio);

		irq = platform_get_irq(pdev, 0);
		if (irq > 0)
			synchronize_irq(irq);

		pr_info("axvisor_adapter: virtio-mmio handoff reset base=0x%llx version=%u status_before=0x%x irq_status=0x%x status_after=0x%x tries=%u\n",
			base, version, status_before, irq_status, status_after, tries);
	}

	irq_ret = axvisor_adapter_request_passthrough_irq(pdev, base);
	if (irq_ret < 0)
		pr_warn("axvisor_adapter: passthrough irq request failed for %s pa=0x%llx ret=%d\n",
			dev_name(dev), base, irq_ret);

out_put_dev:
	put_device(dev);
	return irq_ret < 0 ? irq_ret : 0;
}

static bool axvisor_adapter_release_base_seen(const u64 *bases, size_t count, u64 base)
{
	size_t i;

	for (i = 0; i < count; i++) {
		if (bases[i] == base)
			return true;
	}
	return false;
}

static ssize_t axvisor_shell_proc_write(struct file *file, const char __user *buf,
					size_t len, loff_t *ppos)
{
	u8 *kbuf;
	size_t written;

	pr_emerg("axvisor_adapter: shell_write enter len=%zu ppos=%lld\n",
		 len, ppos ? *ppos : -1LL);
	pr_info("axvisor_adapter: /proc/axvisor_shell write enter len=%zu ppos=%lld\n",
		len, ppos ? *ppos : -1LL);

	if (!len)
		return 0;

	if (!axvisor_linux_console_shell_ready()) {
		pr_info("axvisor_adapter: /proc/axvisor_shell write rejected before shell ready len=%zu\n",
			len);
		return -EAGAIN;
	}

	kbuf = memdup_user_nul(buf, len);
	if (IS_ERR(kbuf))
		return PTR_ERR(kbuf);

	pr_emerg("axvisor_adapter: shell_write cmd=%.*s\n", (int)len, kbuf);
	pr_info("axvisor_adapter: /proc/axvisor_shell write len=%zu cmd=%.*s\n",
		len, (int)len, kbuf);

	written = axvisor_linux_console_enqueue_bytes(kbuf, len);
	pr_emerg("axvisor_adapter: shell_write enqueued=%zu/%zu\n", written, len);
	pr_info("axvisor_adapter: /proc/axvisor_shell enqueued=%zu/%zu\n",
		written, len);
	if (written != len)
		pr_warn("axvisor_adapter: /proc/axvisor_shell partial enqueue=%zu/%zu\n",
			written, len);
	kfree(kbuf);

	if (written == 0)
		return -ENOMEM;

	*ppos += written;
	pr_emerg("axvisor_adapter: shell_write leave written=%zu new_ppos=%lld\n",
		 written, ppos ? *ppos : -1LL);
	pr_info("axvisor_adapter: /proc/axvisor_shell write leave written=%zu new_ppos=%lld\n",
		written, ppos ? *ppos : -1LL);
	return written;
}

static const struct proc_ops axvisor_shell_proc_ops = {
	.proc_write = axvisor_shell_proc_write,
	.proc_lseek = default_llseek,
};

static ssize_t axvisor_guest_console_proc_write(struct file *file,
						const char __user *buf,
						size_t len, loff_t *ppos)
{
	u8 *kbuf;
	size_t written = 0;
	size_t chunk_size;
	ssize_t ret = 0;

	if (!len)
		return 0;

	chunk_size = min_t(size_t, len, AXVISOR_GUEST_CONSOLE_PROC_IO_CHUNK_SIZE);
	kbuf = kmalloc(chunk_size, GFP_KERNEL);
	if (!kbuf)
		return -ENOMEM;

	while (written < len) {
		size_t copied = 0;
		size_t step = min_t(size_t, len - written, chunk_size);

		if (copy_from_user(kbuf, buf + written, step)) {
			ret = written ? written : -EFAULT;
			break;
		}

		while (copied < step) {
			size_t enqueued;

			if (!axvisor_guest_console_ring_space(&axvisor_guest_console_input)) {
				if (file->f_flags & O_NONBLOCK) {
					ret = written ? written : -EAGAIN;
					goto out;
				}

				if (wait_event_interruptible(
					    axvisor_guest_console_input_wait,
					    axvisor_guest_console_ring_space(&axvisor_guest_console_input))) {
					ret = written ? written : -ERESTARTSYS;
					goto out;
				}
			}

			enqueued = axvisor_guest_console_ring_enqueue(
				&axvisor_guest_console_input, kbuf + copied,
				step - copied, false);
			if (!enqueued) {
				if (file->f_flags & O_NONBLOCK) {
					ret = written ? written : -EAGAIN;
					goto out;
				}

				continue;
			}
			copied += enqueued;
			written += enqueued;
		}
		if (ret)
			break;
	}

out:
	kfree(kbuf);

	if (!ret)
		ret = written;

	if (ret > 0)
		*ppos += ret;
	return ret;
}

static ssize_t axvisor_guest_console_proc_read(struct file *file,
					       char __user *buf,
					       size_t len, loff_t *ppos)
{
	u8 *kbuf;
	size_t chunk_size;
	size_t read = 0;
	loff_t tmp_pos = 0;
	ssize_t ret;

	if (!len)
		return 0;

	chunk_size = min_t(size_t, len, AXVISOR_GUEST_CONSOLE_PROC_IO_CHUNK_SIZE);
	kbuf = kmalloc(chunk_size, GFP_KERNEL);
	if (!kbuf)
		return -ENOMEM;

	for (;;) {
		if (!axvisor_guest_console_ring_len(&axvisor_guest_console_output)) {
			if (file->f_flags & O_NONBLOCK) {
				ret = -EAGAIN;
				goto out;
			}

			ret = wait_event_interruptible(
				axvisor_guest_console_output_wait,
				axvisor_guest_console_ring_len(&axvisor_guest_console_output));
			if (ret)
				goto out;
		}

		read = axvisor_guest_console_ring_drain(&axvisor_guest_console_output,
						       kbuf, chunk_size);
		if (read)
			break;
	}

	if (read) {
		s64 log_idx = atomic64_fetch_inc(&axvisor_guest_console_proc_read_log_count);
		size_t buffered = 0;
		size_t dropped = 0;
		size_t zero_writes = 0;
		size_t nonzero_writes = 0;

		axvisor_guest_console_ring_snapshot(&axvisor_guest_console_output,
						    &buffered, &dropped,
						    &zero_writes,
						    &nonzero_writes);
		if (log_idx < 8) {
			size_t preview_len = min_t(size_t, read, 16);

			print_hex_dump(KERN_DEBUG,
				       "axvisor_adapter: /proc/axvisor_guest_console preview ",
				       DUMP_PREFIX_NONE, 16, 1, kbuf,
				       preview_len, true);
		}
		pr_debug("axvisor_adapter: /proc/axvisor_guest_console read=%zu requested=%zu buffered_after=%zu dropped=%zu zero_writes=%zu nonzero_writes=%zu\n",
			 read, len, buffered, dropped, zero_writes,
			 nonzero_writes);
	}
	ret = simple_read_from_buffer(buf, len, &tmp_pos, kbuf, read);
out:
	kfree(kbuf);
	if (ret > 0)
		*ppos += ret;
	return ret;
}

static __poll_t axvisor_guest_console_proc_poll(struct file *file,
						poll_table *wait)
{
	__poll_t mask = EPOLLOUT | EPOLLWRNORM;

	poll_wait(file, &axvisor_guest_console_output_wait, wait);
	poll_wait(file, &axvisor_guest_console_input_wait, wait);

	if (axvisor_guest_console_ring_len(&axvisor_guest_console_output))
		mask |= EPOLLIN | EPOLLRDNORM;
	if (!axvisor_guest_console_ring_space(&axvisor_guest_console_input))
		mask &= ~(EPOLLOUT | EPOLLWRNORM);

	return mask;
}

static const struct proc_ops axvisor_guest_console_proc_ops = {
	.proc_read = axvisor_guest_console_proc_read,
	.proc_write = axvisor_guest_console_proc_write,
	.proc_poll = axvisor_guest_console_proc_poll,
	.proc_lseek = default_llseek,
};

bool axvisor_adapter_console_input_install(void)
{
	if (axvisor_shell_proc_entry)
		return true;

	axvisor_shell_proc_entry = proc_create("axvisor_shell", 0200, NULL,
					       &axvisor_shell_proc_ops);
	if (!axvisor_shell_proc_entry)
		return false;

	axvisor_guest_console_proc_entry =
		proc_create("axvisor_guest_console", 0600, NULL,
			    &axvisor_guest_console_proc_ops);
	if (!axvisor_guest_console_proc_entry) {
		proc_remove(axvisor_shell_proc_entry);
		axvisor_shell_proc_entry = NULL;
		return false;
	}

	return true;
}

void axvisor_adapter_console_input_remove(void)
{
	if (axvisor_guest_console_proc_entry) {
		proc_remove(axvisor_guest_console_proc_entry);
		axvisor_guest_console_proc_entry = NULL;
	}

	if (axvisor_shell_proc_entry) {
		proc_remove(axvisor_shell_proc_entry);
		axvisor_shell_proc_entry = NULL;
	}
}

/* 9. TimeIf::current_time_nanos */
u64 axvisor_adapter_current_time_nanos(void)
{
	return ktime_get_ns();
}

/* 23. MemoryIf::alloc_frame */
u64 axvisor_adapter_alloc_frame(void)
{
	struct axvisor_memory_record *record;
	struct page *page = alloc_pages(GFP_KERNEL | __GFP_ZERO, 0);

	if (!page)
		return 0;

	record = kmalloc(sizeof(*record), GFP_KERNEL);
	if (!record) {
		__free_pages(page, 0);
		return 0;
	}

	record->paddr = (u64)page_to_phys(page);
	record->vaddr = (u64)(unsigned long)page_address(page);
	record->raw_vaddr = record->vaddr;
	record->num_frames = 1;
	record->size_bytes = PAGE_SIZE;
	record->alloc_size_bytes = PAGE_SIZE;
	record->kind = AXVISOR_MEM_FRAME;
	record->page = page;
	INIT_LIST_HEAD(&record->node);

	if (!axvisor_memory_record_insert(record)) {
		kfree(record);
		__free_pages(page, 0);
		return 0;
	}

	return record->paddr;
}

/* 25. MemoryIf::dealloc_frame */
bool axvisor_adapter_dealloc_frame(u64 paddr)
{
	struct axvisor_memory_record *record;

	if (!paddr)
		return false;

	record = axvisor_memory_record_remove(paddr);
	if (!record || record->kind != AXVISOR_MEM_FRAME || record->num_frames != 1) {
		kfree(record);
		return false;
	}

	__free_pages(record->page, 0);
	kfree(record);
	return true;
}

void axvisor_adapter_release_dynamic_mappings(void)
{
	struct axvisor_memory_record *record, *tmp;
	LIST_HEAD(to_release);
	unsigned long flags;

	spin_lock_irqsave(&axvisor_memory_records_lock, flags);
	list_for_each_entry_safe(record, tmp, &axvisor_memory_records, node) {
		if (record->kind == AXVISOR_MEM_REMAP ||
		    record->kind == AXVISOR_MEM_IOREMAP) {
			list_move_tail(&record->node, &to_release);
		}
	}
	spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);

	list_for_each_entry_safe(record, tmp, &to_release, node) {
		list_del(&record->node);
		if (record->kind == AXVISOR_MEM_IOREMAP)
			iounmap((void __iomem *)(unsigned long)record->raw_vaddr);
		else
			memunmap((void *)(unsigned long)record->raw_vaddr);
		kfree(record);
	}
}

/* 27. MemoryIf::phys_to_virt */
u64 axvisor_adapter_phys_to_virt(u64 paddr)
{
	struct axvisor_memory_record *record;
	unsigned long flags;
	u64 mapped_vaddr;

	if (!paddr)
		return 0;

	if (axvisor_paddr_is_host_plic(paddr))
		return axvisor_adapter_ioremap_range(paddr, sizeof(u32));
	if (axvisor_paddr_is_passthrough_mmio(paddr))
		return axvisor_adapter_ioremap_range(paddr, PAGE_SIZE);

	spin_lock_irqsave(&axvisor_memory_records_lock, flags);
	record = axvisor_memory_record_lookup_paddr_locked(paddr);
	if (record) {
		u64 offset = paddr - record->paddr;
		u64 vaddr = record->vaddr + offset;
		int kind = record->kind;

		spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
		pr_debug("axvisor_adapter: phys_to_virt record pa=0x%llx -> va=0x%llx kind=%d\n",
			 paddr, vaddr, kind);
		return vaddr;
	}
	spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);

		/*
		 * Only explicitly registered guest RAM may be memremapped here.
		 * This keeps Linux-host semantics narrower than Asterinas' broad
		 * linear mapping and prevents accidental access to arbitrary HPAs.
		 */
		mapped_vaddr = axvisor_adapter_map_host_page(paddr);
		return mapped_vaddr;
	}

void axvisor_adapter_riscv_plic_complete_passthrough_irq(u32 irq_id)
{
	struct axvisor_passthrough_irq_record *record;
	unsigned long flags;
	unsigned int virq = 0;
	unsigned long hwirq = 0;
	u64 base = 0;
	bool should_enable = false;
	bool registered = false;
	bool host_irq_requested = false;
	bool host_irq_masked_before = false;
	s64 log_count;

	if (!irq_id)
		return;

	spin_lock_irqsave(&axvisor_passthrough_irqs_lock, flags);
	record = axvisor_passthrough_irq_record_for_guest_irq_locked(irq_id);
	if (record) {
		registered = true;
		virq = record->virq;
		hwirq = record->hwirq;
		base = record->base;
		host_irq_requested = record->host_irq_requested;
		host_irq_masked_before = record->host_irq_masked;
		if (record->host_irq_requested && record->host_irq_masked) {
			record->host_irq_masked = false;
			should_enable = true;
		}
	}
	spin_unlock_irqrestore(&axvisor_passthrough_irqs_lock, flags);

	if (!registered) {
		pr_info("axvisor_adapter: plic complete ignored non-passthrough irq=%u\n",
			irq_id);
		return;
	}

	log_count = atomic64_inc_return(&axvisor_plic_complete_log_count);
	if (axvisor_log_count_visible(log_count)) {
		pr_info("axvisor_adapter: plic complete passthrough irq=%u virq=%u hwirq=%lu base=0x%llx host_requested=%d masked_before=%d reenable=%d complete_count=%lld\n",
			irq_id, virq, hwirq, base, host_irq_requested,
			host_irq_masked_before, should_enable, log_count);
	}

	if (should_enable)
		enable_irq(virq);
}

u32 axvisor_adapter_mmio_read32(u64 paddr)
{
	void __iomem *addr;

	if (!paddr)
		return 0;

	addr = (void __iomem *)(unsigned long)axvisor_adapter_ioremap_range(paddr,
									    sizeof(u32));
	if (!addr)
		return 0;

	return readl(addr);
}

void axvisor_adapter_mmio_write32(u64 paddr, u32 value)
{
	void __iomem *addr;

	if (!paddr)
		return;

	addr = (void __iomem *)(unsigned long)axvisor_adapter_ioremap_range(paddr,
									    sizeof(u32));
	if (!addr)
		return;

	writel(value, addr);
}

/* 28. MemoryIf::virt_to_phys */
u64 axvisor_adapter_virt_to_phys(u64 vaddr)
{
	struct axvisor_memory_record *record;
	unsigned long flags;
	void *ptr;

	if (!vaddr)
		return 0;

	spin_lock_irqsave(&axvisor_memory_records_lock, flags);
	record = axvisor_memory_record_lookup_vaddr_locked(vaddr);
	if (record) {
		u64 offset = vaddr - record->vaddr;

		spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);
		return record->paddr + offset;
	}
	spin_unlock_irqrestore(&axvisor_memory_records_lock, flags);

	ptr = (void *)(unsigned long)vaddr;
	if (!virt_addr_valid(ptr))
		return 0;

	return (u64)__pa(ptr);

	return 0;
}

int axvisor_adapter_release_host_filesystems(void)
{
	unsigned int i;
	size_t registered_count, released_count = 0;
	u64 released_bases[ARRAY_SIZE(axvisor_release_mmio_paddrs) + 16];
	int ret = 0;

	/*
	 * The AxVisor core calls HostIf::release_host_filesystems() before guest
	 * passthrough devices take ownership. Linux may already have bound the
	 * QEMU virtio-mmio devices used by a guest, so release those platform
	 * drivers before handing their MMIO regions and IRQs to AxVisor.
	 */
	pr_info("axvisor_adapter: release_host_filesystems hook reached release_mmio_paddrs_count=%u\n",
		axvisor_release_mmio_paddrs_count);

	registered_count = axvisor_linux_passthrough_device_count();
	pr_info("axvisor_adapter: release_host_filesystems registered_passthrough_count=%zu release_registered=%d\n",
		registered_count, axvisor_release_registered_passthrough_mmio);

	if (axvisor_release_registered_passthrough_mmio) {
		for (i = 0; i < registered_count &&
		     released_count < ARRAY_SIZE(released_bases); i++) {
			u64 base = axvisor_linux_passthrough_device_base_hpa(i);
			int one_ret;

			if (!base ||
			    axvisor_adapter_release_base_seen(released_bases,
							     released_count,
							     base))
				continue;

			pr_info("axvisor_adapter: release_host_filesystems registered_passthrough[%u] base=0x%llx irq=%zu\n",
				i, base, axvisor_linux_passthrough_device_irq_id(i));
			one_ret = axvisor_adapter_release_platform_mmio_device(base);
			released_bases[released_count++] = base;
			if (one_ret < 0 && !ret)
				ret = one_ret;
		}
	}

	for (i = 0; i < axvisor_release_mmio_paddrs_count; i++) {
		int one_ret;
		u64 base = axvisor_release_mmio_paddrs[i];

		if (!base || axvisor_adapter_release_base_seen(released_bases, released_count, base))
			continue;

		pr_info("axvisor_adapter: release_host_filesystems release_mmio_paddrs[%u]=0x%llx\n",
			i, base);
		one_ret = axvisor_adapter_release_platform_mmio_device(base);
		if (released_count < ARRAY_SIZE(released_bases))
			released_bases[released_count++] = base;
		if (one_ret < 0 && !ret)
			ret = one_ret;
	}

	return ret;
}

/* 6. HostIf::exit */
void axvisor_adapter_host_exit(int exit_code)
{
	pr_emerg("axvisor_adapter: host exit requested, exit_code=%d\n",
		 exit_code);

	if (exit_code == 0)
		kernel_power_off();
	else
		kernel_halt();

	for (;;)
		cpu_relax();
}

unsigned long long axvisor_adapter_host_fdt_vaddr(void)
{
	return axvisor_host_fdt_vaddr;
}

size_t axvisor_adapter_host_fdt_size(void)
{
	return axvisor_host_fdt_size;
}

/* 30. ArchIf::host_tsc_frequency_mhz */
unsigned int axvisor_adapter_host_tsc_frequency_mhz(void)
{
#ifdef CONFIG_RISCV
	return (unsigned int)(riscv_timebase / 1000000UL);
#else
	return 0;
#endif
}

void *axvisor_adapter_runtime_alloc(size_t size, size_t align)
{
	struct axvisor_runtime_alloc_header *header;
	size_t actual_align, total, padding;
	uintptr_t raw_addr, aligned_addr;
	void *raw;

	if (!size)
		return NULL;

	actual_align = align;
	if (actual_align < sizeof(void *))
		actual_align = sizeof(void *);
	if (!is_power_of_2(actual_align))
		return NULL;
	if (size > SIZE_MAX - actual_align - sizeof(*header))
		return NULL;

	total = size + actual_align + sizeof(*header);
	raw = kmalloc(total, GFP_KERNEL);
	if (!raw)
		return NULL;

	raw_addr = (uintptr_t)raw;
	padding = sizeof(*header);
	aligned_addr = ALIGN(raw_addr + padding, actual_align);
	header = (struct axvisor_runtime_alloc_header *)aligned_addr - 1;
	header->magic = AXVISOR_RUNTIME_ALLOC_MAGIC;
	header->raw = raw;
	header->size = size;
	header->align = actual_align;

	if (!axvisor_runtime_alloc_record_insert((u64)(unsigned long)aligned_addr, size)) {
		kfree(raw);
		return NULL;
	}

	return (void *)aligned_addr;
}

void axvisor_adapter_runtime_dealloc(void *ptr, size_t align)
{
	struct axvisor_runtime_alloc_header *header;

	if (!ptr)
		return;

	header = axvisor_runtime_alloc_header_from_ptr(ptr);
	if (!header)
		return;
	if (align && header->align != max_t(size_t, align, sizeof(void *)))
		return;

	axvisor_runtime_alloc_record_remove((u64)(unsigned long)ptr);
	kfree(header->raw);
}

void *axvisor_adapter_runtime_realloc(void *ptr, size_t new_size, size_t align)
{
	struct axvisor_runtime_alloc_header *old_header;
	void *new_ptr;

	if (!ptr)
		return axvisor_adapter_runtime_alloc(new_size, align);
	if (!new_size) {
		axvisor_adapter_runtime_dealloc(ptr, align);
		return NULL;
	}

	old_header = axvisor_runtime_alloc_header_from_ptr(ptr);
	if (!old_header)
		return NULL;

	new_ptr = axvisor_adapter_runtime_alloc(new_size, align);
	if (!new_ptr)
		return NULL;

	memcpy(new_ptr, ptr, min(old_header->size, new_size));
	axvisor_runtime_alloc_record_remove((u64)(unsigned long)ptr);
	kfree(old_header->raw);

	return new_ptr;
}
