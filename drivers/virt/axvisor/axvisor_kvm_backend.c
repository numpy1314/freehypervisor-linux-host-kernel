// SPDX-License-Identifier: GPL-2.0-only
/*
 * Backend registration hooks for the AxVisor KVM ABI provider.
 *
 * The /dev/kvm ABI provider can be loaded before the real AxVisor backend is
 * available. In that state every execution-facing operation returns
 * -EOPNOTSUPP and KVM_RUN reports a deterministic fail-entry to userspace.
 */

#define pr_fmt(fmt) "axvisor_kvm_backend: " fmt

#include <linux/errno.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/printk.h>

#include "axvisor_kvm_backend.h"

static DEFINE_MUTEX(axvisor_kvm_backend_lock);
static const struct axvisor_kvm_backend_ops *axvisor_kvm_backend_ops;

static const struct axvisor_kvm_backend_ops *axvisor_kvm_backend_get(void)
{
	const struct axvisor_kvm_backend_ops *ops;

	mutex_lock(&axvisor_kvm_backend_lock);
	ops = axvisor_kvm_backend_ops;
	if (ops && ops->owner && !try_module_get(ops->owner))
		ops = NULL;
	mutex_unlock(&axvisor_kvm_backend_lock);

	return ops;
}

static void axvisor_kvm_backend_put(const struct axvisor_kvm_backend_ops *ops)
{
	if (ops && ops->owner)
		module_put(ops->owner);
}

int axvisor_kvm_backend_register(const struct axvisor_kvm_backend_ops *ops)
{
	int ret = 0;

	if (!ops)
		return -EINVAL;

	mutex_lock(&axvisor_kvm_backend_lock);
	if (axvisor_kvm_backend_ops)
		ret = -EBUSY;
	else
		axvisor_kvm_backend_ops = ops;
	mutex_unlock(&axvisor_kvm_backend_lock);

	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_register);

void axvisor_kvm_backend_unregister(const struct axvisor_kvm_backend_ops *ops)
{
	mutex_lock(&axvisor_kvm_backend_lock);
	if (axvisor_kvm_backend_ops == ops)
		axvisor_kvm_backend_ops = NULL;
	mutex_unlock(&axvisor_kvm_backend_lock);
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_unregister);

int axvisor_kvm_backend_create_vm(u64 *backend_vm)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	if (backend_vm)
		*backend_vm = 0;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->create_vm) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->create_vm(backend_vm);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_create_vm);

void axvisor_kvm_backend_destroy_vm(u64 backend_vm)
{
	const struct axvisor_kvm_backend_ops *ops;

	ops = axvisor_kvm_backend_get();
	if (ops && ops->destroy_vm)
		ops->destroy_vm(backend_vm);
	axvisor_kvm_backend_put(ops);
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_destroy_vm);

int axvisor_kvm_backend_set_vm_state(
	u64 backend_vm, const struct axkvm_backend_vm_state *state)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->set_vm_state) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->set_vm_state(backend_vm, state);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_set_vm_state);

int axvisor_kvm_backend_map_page(u64 backend_vm, u64 gpa, u64 hpa, u32 flags)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->map_page) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->map_page(backend_vm, gpa, hpa, flags);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_map_page);

int axvisor_kvm_backend_map_page_nolog(u64 backend_vm, u64 gpa, u64 hpa,
				       u32 flags)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->map_page_nolog) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->map_page_nolog(backend_vm, gpa, hpa, flags);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_map_page_nolog);

int axvisor_kvm_backend_unmap_range(u64 backend_vm, u64 gpa, u64 size)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->unmap_range) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->unmap_range(backend_vm, gpa, size);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_unmap_range);

int axvisor_kvm_backend_create_vcpu(u64 backend_vm, u32 vcpu_id,
				    u64 *backend_vcpu)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	if (backend_vcpu)
		*backend_vcpu = 0;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->create_vcpu) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->create_vcpu(backend_vm, vcpu_id, backend_vcpu);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_create_vcpu);

void axvisor_kvm_backend_destroy_vcpu(u64 backend_vcpu)
{
	const struct axvisor_kvm_backend_ops *ops;

	ops = axvisor_kvm_backend_get();
	if (ops && ops->destroy_vcpu)
		ops->destroy_vcpu(backend_vcpu);
	axvisor_kvm_backend_put(ops);
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_destroy_vcpu);

int axvisor_kvm_backend_set_vcpu_state(
	u64 backend_vcpu, const struct axkvm_backend_vcpu_state *state)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->set_vcpu_state) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->set_vcpu_state(backend_vcpu, state);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_set_vcpu_state);

int axvisor_kvm_backend_boot_vm(u64 backend_vm)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->boot_vm) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->boot_vm(backend_vm);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_boot_vm);

int axvisor_kvm_backend_run_vcpu(u64 backend_vcpu,
				 struct axkvm_backend_exit *exit)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	if (exit) {
		exit->reason = AXKVM_BACKEND_EXIT_FAIL_ENTRY;
		exit->hardware_entry_failure_reason = 0;
	}

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->run_vcpu) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->run_vcpu(backend_vcpu, exit);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_run_vcpu);

int axvisor_kvm_backend_complete_mmio_read(u64 backend_vcpu,
					   const void *data, u32 len)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->complete_mmio_read) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->complete_mmio_read(backend_vcpu, data, len);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_complete_mmio_read);

int axvisor_kvm_backend_complete_io_read(u64 backend_vcpu,
					 const void *data, u32 len)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->complete_io_read) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->complete_io_read(backend_vcpu, data, len);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_complete_io_read);

int axvisor_kvm_backend_inject_irq(u64 backend_vm, u32 gsi)
{
	const struct axvisor_kvm_backend_ops *ops;
	int ret;

	ops = axvisor_kvm_backend_get();
	if (!ops || !ops->inject_irq) {
		ret = -EOPNOTSUPP;
		goto out;
	}

	ret = ops->inject_irq(backend_vm, gsi);
out:
	axvisor_kvm_backend_put(ops);
	return ret;
}
EXPORT_SYMBOL_GPL(axvisor_kvm_backend_inject_irq);

__weak int axvisor_kvm_builtin_backend_init(void)
{
	return 0;
}

__weak void axvisor_kvm_builtin_backend_exit(void)
{
}

MODULE_LICENSE("GPL");
