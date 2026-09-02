# SPDX-License-Identifier: GPL-2.0-only

obj-$(CONFIG_AXVISOR_ADAPTER) += axvisor_adapter.o
obj-$(CONFIG_AXVISOR_KVM) += axvisor_kvm.o
axvisor_kvm-y := axvisor_kvm_backend.o axvisor_kvm_main.o
axvisor_kvm-$(CONFIG_AXVISOR_KVM_AXVISOR_BACKEND) += axvisor_kvm_axvisor_backend.o
axvisor_kvm-$(CONFIG_AXVISOR_KVM_RUST_BACKEND) += axvisor_kvm_rust_backend.o
axvisor_kvm-$(CONFIG_AXVISOR_KVM_X86_BRIDGE) += axvisor_kvm_x86_bridge_runtime.o axvisor_kvm_x86_bridge.o

axvisor_kvm_x86_rlib_dir := $(obj)/axvisor_kvm_x86
axvisor_kvm_x86_rlib_dir_abs := $(abspath $(axvisor_kvm_x86_rlib_dir))
axvisor_kvm_x86_rlib_prefix := $(axvisor_kvm_x86_rlib_dir)/lib
axvisor_kvm_x86_runtime_core := $(axvisor_kvm_x86_rlib_prefix)core.rlib
axvisor_kvm_x86_runtime_alloc := $(axvisor_kvm_x86_rlib_prefix)alloc.rlib
axvisor_kvm_x86_runtime_compiler_builtins := $(axvisor_kvm_x86_rlib_prefix)compiler_builtins.rlib
axvisor_kvm_x86_target_spec := $(axvisor_kvm_x86_rlib_dir)/x86_64-axvisor-linux-kernel.json
axvisor_kvm_x86_host_proc_macro2 := $(objtree)/rust/libproc_macro2.rlib
axvisor_kvm_x86_host_quote := $(objtree)/rust/libquote.rlib
axvisor_kvm_x86_host_syn := $(objtree)/rust/libsyn.rlib

targets += axvisor_kvm_x86_bridge_raw.o axvisor_kvm_x86_bridge.o

axvisor_kvm_x86_target_sysroot := $(shell $(RUSTC) --print sysroot)
axvisor_kvm_x86_rust_src := $(axvisor_kvm_x86_target_sysroot)/lib/rustlib/src/rust/library
axvisor_kvm_x86_target_rustlib_dir := \
	$(axvisor_kvm_x86_target_sysroot)/lib/rustlib/x86_64-unknown-none/lib
axvisor_kvm_x86_core_rmeta := $(axvisor_kvm_x86_rlib_prefix)core.rmeta
axvisor_kvm_x86_alloc_rmeta := $(axvisor_kvm_x86_rlib_prefix)alloc.rmeta
axvisor_kvm_x86_compiler_builtins_rmeta := $(axvisor_kvm_x86_rlib_prefix)compiler_builtins.rmeta
axvisor_kvm_x86_rustc_codegen_flags := \
	-Cdebuginfo=0 \
	-Cforce-unwind-tables=n \
	-Cllvm-args=--generate-type-units=false

axvisor_kvm_x86_target_flags := \
	--target=$(axvisor_kvm_x86_target_spec) \
	-Cpanic=abort \
	-Crelocation-model=static \
	-Cno-redzone=y \
	-Ccode-model=kernel \
	-Copt-level=2

axvisor_kvm_x86_rust_edition = --edition=2021
axvisor_kvm_x86_runtime_rust_edition = --edition=2024

axvisor_kvm_x86_target_rlibs := \
	$(axvisor_kvm_x86_runtime_core) \
	$(axvisor_kvm_x86_runtime_compiler_builtins) \
	$(axvisor_kvm_x86_runtime_alloc) \
	$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bitflags.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bitflags1.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bit_field.rlib \
	$(axvisor_kvm_x86_rlib_prefix)arrayvec.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bitmaps.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bit.rlib \
	$(axvisor_kvm_x86_rlib_prefix)spin.rlib \
	$(axvisor_kvm_x86_rlib_prefix)log.rlib \
	$(axvisor_kvm_x86_rlib_prefix)numeric_enum_macro.rlib \
	$(axvisor_kvm_x86_rlib_prefix)tock_registers.rlib \
	$(axvisor_kvm_x86_rlib_prefix)volatile.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86_64.rlib \
	$(axvisor_kvm_x86_rlib_prefix)raw_cpuid.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_lazyinit.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_set.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_page_table_multiarch.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_cpumask.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_kernel_guard.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_percpu.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvcpu.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86_vlapic.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86_vcpu.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axdevice.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvm.rlib

axvisor_kvm_x86_dependency_rlibs := \
	$(filter-out \
		$(axvisor_kvm_x86_runtime_core) \
		$(axvisor_kvm_x86_runtime_compiler_builtins) \
		$(axvisor_kvm_x86_runtime_alloc), \
		$(axvisor_kvm_x86_target_rlibs))

axvisor_kvm_x86_host_proc_macros := \
	$(axvisor_kvm_x86_rlib_prefix)const_fn.so \
	$(axvisor_kvm_x86_rlib_prefix)rustversion.so \
	$(axvisor_kvm_x86_rlib_prefix)paste.so \
	$(axvisor_kvm_x86_rlib_prefix)ax_crate_interface.so \
	$(axvisor_kvm_x86_rlib_prefix)ax_percpu_macros.so \
	$(axvisor_kvm_x86_rlib_prefix)axvisor_api_proc.so

always-$(CONFIG_AXVISOR_KVM_X86_BRIDGE) += \
	$(axvisor_kvm_x86_target_rlibs) \
	$(axvisor_kvm_x86_host_proc_macros)

targets += \
	$(axvisor_kvm_x86_target_spec) \
	$(axvisor_kvm_x86_target_rlibs) \
	$(axvisor_kvm_x86_host_proc_macros)

quiet_cmd_axvisor_kvm_x86_mkdir = MKDIR   $(axvisor_kvm_x86_rlib_dir)
      cmd_axvisor_kvm_x86_mkdir = mkdir -p $(axvisor_kvm_x86_rlib_dir)

$(axvisor_kvm_x86_rlib_dir):
	+$(call cmd,axvisor_kvm_x86_mkdir)

quiet_cmd_axvisor_kvm_x86_target_spec = GEN     $@
      cmd_axvisor_kvm_x86_target_spec = \
	mkdir -p $(dir $@) && \
	$(RUSTC) -Z unstable-options --print target-spec-json --target=x86_64-unknown-none > $@.tmp && \
	python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["position-independent-executables"]=False; d["static-position-independent-executables"]=False; d["plt-by-default"]=True; json.dump(d, open(p, "w"), indent=2); open(p, "a").write("\n")' $@.tmp && \
	mv $@.tmp $@

$(axvisor_kvm_x86_target_spec): $(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed,axvisor_kvm_x86_target_spec)

quiet_cmd_axvisor_kvm_x86_rustversion_expr = GEN     $@
      cmd_axvisor_kvm_x86_rustversion_expr = \
	printf 'crate::version::Version { minor: 96, patch: 0, channel: crate::version::Channel::Nightly(crate::date::Date { year: 2026, month: 4, day: 2 }) }\n' > $@

$(axvisor_kvm_x86_rlib_dir)/version.expr: $(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed,axvisor_kvm_x86_rustversion_expr)

quiet_cmd_axvisor_kvm_x86_rustc_host_proc_macro = RUSTC   $@
      cmd_axvisor_kvm_x86_rustc_host_proc_macro = \
	OUT_DIR=$(axvisor_kvm_x86_rlib_dir_abs) $(RUSTC) \
		$(filter-out $(skip_flags) -Dunsafe_op_in_unsafe_fn,$(rust_common_flags) $(rustc_target_flags)) \
		-Clinker-flavor=gcc -Clinker=$(HOSTCC) \
		-Clink-args='$(call escsq,$(KBUILD_PROCMACROLDFLAGS))' \
		--emit=dep-info=$(depfile) --emit=link=$@ \
		--extern proc_macro \
		--crate-type proc-macro \
		-L dependency=$(obj) \
		-L dependency=$(objtree)/rust \
		-L dependency=$(axvisor_kvm_x86_rlib_dir) \
		--crate-name $(patsubst lib%.so,%,$(notdir $@)) \
		$<

quiet_cmd_axvisor_kvm_x86_rustc_core = RUSTC   $@
      cmd_axvisor_kvm_x86_rustc_core = \
	$(RUSTC) \
		$(filter-out $(skip_flags) -Dunsafe_op_in_unsafe_fn --edition=%,$(rust_common_flags) $(rustc_target_flags)) \
		$(axvisor_kvm_x86_runtime_rust_edition) \
		$(axvisor_kvm_x86_target_flags) \
		--cfg no_fp_fmt_parse \
		--emit=dep-info=$(depfile) --emit=link=$@ \
		--emit=metadata=$(dir $@)$(patsubst %.rlib,%.rmeta,$(notdir $@)) \
		--crate-type rlib \
		--out-dir $(axvisor_kvm_x86_rlib_dir) \
		--crate-name $(patsubst lib%.rlib,%,$(notdir $@)) \
		--sysroot=/dev/null \
		-Zunstable-options \
		$<

quiet_cmd_axvisor_kvm_x86_rustc_runtime_library = RUSTC   $@
      cmd_axvisor_kvm_x86_rustc_runtime_library = \
	$(RUSTC) \
		$(filter-out $(skip_flags) -Dunsafe_op_in_unsafe_fn --edition=%,$(rust_common_flags) $(rustc_target_flags)) \
		$(axvisor_kvm_x86_runtime_rust_edition) \
		$(axvisor_kvm_x86_target_flags) \
		--extern core=$(axvisor_kvm_x86_core_rmeta) \
		--extern compiler_builtins=$(axvisor_kvm_x86_compiler_builtins_rmeta) \
		--emit=dep-info=$(depfile) --emit=link=$@ \
		--emit=metadata=$(dir $@)$(patsubst %.rlib,%.rmeta,$(notdir $@)) \
		--crate-type rlib \
		--out-dir $(axvisor_kvm_x86_rlib_dir) \
		-L dependency=$(axvisor_kvm_x86_rlib_dir) \
		--crate-name $(patsubst lib%.rlib,%,$(notdir $@)) \
		--sysroot=/dev/null \
		-Zunstable-options \
		$<

quiet_cmd_axvisor_kvm_x86_rustc_compiler_builtins = RUSTC   $@
      cmd_axvisor_kvm_x86_rustc_compiler_builtins = \
	$(RUSTC) \
		$(filter-out $(skip_flags) -Dunsafe_op_in_unsafe_fn --edition=%,$(rust_common_flags) $(rustc_target_flags)) \
		$(axvisor_kvm_x86_rust_edition) \
		$(axvisor_kvm_x86_target_flags) \
		--extern core=$(axvisor_kvm_x86_core_rmeta) \
		--emit=dep-info=$(depfile) --emit=link=$@ \
		--emit=metadata=$(dir $@)$(patsubst %.rlib,%.rmeta,$(notdir $@)) \
		--crate-type rlib \
		--out-dir $(axvisor_kvm_x86_rlib_dir) \
		-L dependency=$(axvisor_kvm_x86_rlib_dir) \
		--crate-name $(patsubst lib%.rlib,%,$(notdir $@)) \
		--sysroot=/dev/null \
		-Zunstable-options \
		$<

quiet_cmd_axvisor_kvm_x86_rustc_target_library = RUSTC   $@
      cmd_axvisor_kvm_x86_rustc_target_library = \
	$(RUSTC) \
		$(filter-out $(skip_flags) -Dunsafe_op_in_unsafe_fn --edition=%,$(rust_common_flags) $(rustc_target_flags)) \
		$(axvisor_kvm_x86_rust_edition) \
		$(axvisor_kvm_x86_target_flags) \
		--extern core=$(axvisor_kvm_x86_core_rmeta) \
		--extern compiler_builtins=$(axvisor_kvm_x86_compiler_builtins_rmeta) \
		--extern alloc=$(axvisor_kvm_x86_alloc_rmeta) \
		--emit=dep-info=$(depfile) --emit=link=$@ \
		--emit=metadata=$(dir $@)$(patsubst %.rlib,%.rmeta,$(notdir $@)) \
		--crate-type rlib \
		--out-dir $(axvisor_kvm_x86_rlib_dir) \
		-L dependency=$(axvisor_kvm_x86_rlib_dir) \
		-L dependency=$(axvisor_kvm_x86_target_rustlib_dir) \
		--crate-name $(patsubst lib%.rlib,%,$(notdir $@)) \
		--sysroot=/dev/null \
		-Zunstable-options \
		$<

quiet_cmd_axvisor_kvm_x86_rustc_bridge_object = RUSTC   $@
      cmd_axvisor_kvm_x86_rustc_bridge_object = \
	$(RUSTC) \
		$(filter-out $(skip_flags) -Dunsafe_op_in_unsafe_fn --edition=%,$(rust_common_flags)) \
		$(axvisor_kvm_x86_rustc_codegen_flags) \
		$(axvisor_kvm_x86_rust_edition) \
		$(axvisor_kvm_x86_target_flags) \
		-L dependency=$(obj) \
		-L dependency=$(axvisor_kvm_x86_rlib_dir) \
		--extern core=$(axvisor_kvm_x86_core_rmeta) \
		--extern compiler_builtins=$(axvisor_kvm_x86_compiler_builtins_rmeta) \
		--extern alloc=$(axvisor_kvm_x86_alloc_rmeta) \
		--extern ax_errno=$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
		--extern ax_kspin=$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rmeta \
		--extern axaddrspace=$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
		--extern axdevice=$(axvisor_kvm_x86_rlib_prefix)axdevice.rlib \
		--extern axdevice_base=$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rlib \
		--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
		--extern axvisor_api=$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rlib \
		--extern axvm=$(axvisor_kvm_x86_rlib_prefix)axvm.rlib \
		--extern axvcpu=$(axvisor_kvm_x86_rlib_prefix)axvcpu.rlib \
		--extern axvmconfig=$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rlib \
		--extern x86_vcpu=$(axvisor_kvm_x86_rlib_prefix)x86_vcpu.rlib \
		--extern x86_vlapic=$(axvisor_kvm_x86_rlib_prefix)x86_vlapic.rlib \
		--emit=dep-info=$(depfile) --emit=obj=$@ \
		--crate-type lib \
		--crate-name axvisor_kvm_x86_bridge \
		--sysroot=/dev/null \
		-Zunstable-options \
		$<

quiet_cmd_axvisor_kvm_x86_ld_object = LD      $@
      cmd_axvisor_kvm_x86_ld_object = \
	$(LD) -r -o $@ \
		$(obj)/axvisor_kvm_x86_bridge_raw.o \
		$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
		$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
		$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
		$(axvisor_kvm_x86_rlib_prefix)axdevice.rlib \
		$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rlib \
		$(axvisor_kvm_x86_rlib_prefix)axvm.rlib \
		$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rlib \
		$(axvisor_kvm_x86_rlib_prefix)axvcpu.rlib \
		$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rlib \
		$(axvisor_kvm_x86_rlib_prefix)x86_vcpu.rlib \
		$(axvisor_kvm_x86_rlib_prefix)x86_vlapic.rlib \
		$(axvisor_kvm_x86_rlib_prefix)log.rlib \
		$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
		$(axvisor_kvm_x86_rlib_prefix)spin.rlib \
		$(axvisor_kvm_x86_rlib_prefix)ax_cpumask.rlib \
		$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rlib \
		$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rlib \
		$(axvisor_kvm_x86_rlib_prefix)ax_page_table_multiarch.rlib \
		$(axvisor_kvm_x86_rlib_prefix)ax_percpu.rlib \
		$(axvisor_kvm_x86_rlib_prefix)x86_64.rlib \
		$(axvisor_kvm_x86_rlib_prefix)x86.rlib \
		$(axvisor_kvm_x86_rlib_prefix)raw_cpuid.rlib \
		$(axvisor_kvm_x86_rlib_prefix)numeric_enum_macro.rlib \
		$(axvisor_kvm_x86_rlib_prefix)tock_registers.rlib \
		$(axvisor_kvm_x86_rlib_prefix)volatile.rlib \
		$(axvisor_kvm_x86_rlib_prefix)bit_field.rlib \
		$(axvisor_kvm_x86_rlib_prefix)bitflags.rlib \
		$(axvisor_kvm_x86_rlib_prefix)bitflags1.rlib \
		$(axvisor_kvm_x86_rlib_prefix)arrayvec.rlib \
		$(axvisor_kvm_x86_rlib_prefix)bitmaps.rlib \
		$(axvisor_kvm_x86_rlib_prefix)bit.rlib \
		$(axvisor_kvm_x86_runtime_core) \
		$(axvisor_kvm_x86_runtime_alloc) \
		$(axvisor_kvm_x86_runtime_compiler_builtins)

$(obj)/axvisor_kvm_x86_bridge_raw.o: private skip_clippy = 1
$(obj)/axvisor_kvm_x86_bridge.o: private skip_clippy = 1
$(axvisor_kvm_x86_runtime_core): private skip_clippy = 1
$(axvisor_kvm_x86_runtime_core): \
	$(axvisor_kvm_x86_rust_src)/core/src/lib.rs \
	$(axvisor_kvm_x86_target_spec) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_core)

$(axvisor_kvm_x86_runtime_compiler_builtins): private skip_clippy = 1
$(axvisor_kvm_x86_runtime_compiler_builtins): \
	$(srctree)/rust/compiler_builtins.rs \
	$(axvisor_kvm_x86_runtime_core) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_compiler_builtins)

$(axvisor_kvm_x86_runtime_alloc): private skip_clippy = 1
$(axvisor_kvm_x86_runtime_alloc): \
	$(axvisor_kvm_x86_rust_src)/alloc/src/lib.rs \
	$(axvisor_kvm_x86_runtime_core) \
	$(axvisor_kvm_x86_runtime_compiler_builtins) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_runtime_library)

$(axvisor_kvm_x86_dependency_rlibs): $(axvisor_kvm_x86_runtime_alloc)

$(axvisor_kvm_x86_rlib_prefix)const_fn.so: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)const_fn.so: \
	$(src)/vendor/registry/const_fn/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_host_proc_macro)

$(axvisor_kvm_x86_rlib_prefix)rustversion.so: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)rustversion.so: \
	$(src)/vendor/registry/rustversion/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir)/version.expr FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_host_proc_macro)

$(axvisor_kvm_x86_rlib_prefix)paste.so: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)paste.so: \
	$(src)/vendor/registry/paste/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_host_proc_macro)

$(axvisor_kvm_x86_rlib_prefix)ax_crate_interface.so: private rustc_target_flags = \
	--extern proc_macro2=$(axvisor_kvm_x86_host_proc_macro2) \
	--extern quote=$(axvisor_kvm_x86_host_quote) \
	--extern syn=$(axvisor_kvm_x86_host_syn)
$(axvisor_kvm_x86_rlib_prefix)ax_crate_interface.so: \
	$(src)/vendor/upstream/ax-crate-interface/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_host_proc_macro)

$(axvisor_kvm_x86_rlib_prefix)ax_percpu_macros.so: private rustc_target_flags = \
	--extern proc_macro2=$(axvisor_kvm_x86_host_proc_macro2) \
	--extern quote=$(axvisor_kvm_x86_host_quote) \
	--extern syn=$(axvisor_kvm_x86_host_syn) \
	--cfg 'feature="sp-naive"'
$(axvisor_kvm_x86_rlib_prefix)ax_percpu_macros.so: \
	$(src)/vendor/upstream/ax-percpu-macros/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_host_proc_macro)

$(axvisor_kvm_x86_rlib_prefix)axvisor_api_proc.so: private rustc_target_flags = \
	--extern proc_macro2=$(axvisor_kvm_x86_host_proc_macro2) \
	--extern quote=$(axvisor_kvm_x86_host_quote) \
	--extern syn=$(axvisor_kvm_x86_host_syn)
$(axvisor_kvm_x86_rlib_prefix)axvisor_api_proc.so: \
	$(src)/vendor/upstream/axvisor_api_proc/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_host_proc_macro)

$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib: \
	$(src)/vendor/registry/cfg-if/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)bitflags.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)bitflags.rlib: \
	$(src)/vendor/registry/bitflags/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)bitflags1.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)bitflags1.rlib: \
	$(src)/vendor/registry/bitflags-1/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)bit_field.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)bit_field.rlib: \
	$(src)/vendor/registry/bit_field/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)arrayvec.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)arrayvec.rlib: \
	$(src)/vendor/registry/arrayvec/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)bitmaps.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)bitmaps.rlib: \
	$(src)/vendor/registry/bitmaps/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)bit.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)bit.rlib: \
	$(src)/vendor/registry/bit/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)spin.rlib: private rustc_target_flags = \
	--cfg 'feature="once"' \
	--cfg 'feature="mutex"' \
	--cfg 'feature="spin_mutex"'
$(axvisor_kvm_x86_rlib_prefix)spin.rlib: \
	$(src)/vendor/registry/spin/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)log.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)log.rlib: \
	$(src)/vendor/registry/log/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)numeric_enum_macro.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)numeric_enum_macro.rlib: \
	$(src)/vendor/registry/numeric-enum-macro/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)tock_registers.rlib: private rustc_target_flags = \
	--cfg 'feature="register_types"'
$(axvisor_kvm_x86_rlib_prefix)tock_registers.rlib: \
	$(src)/vendor/registry/tock-registers/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)volatile.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)volatile.rlib: \
	$(src)/vendor/registry/volatile/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)x86_64.rlib: private rustc_target_flags = \
	--extern bit_field=$(axvisor_kvm_x86_rlib_prefix)bit_field.rmeta \
	--extern bitflags=$(axvisor_kvm_x86_rlib_prefix)bitflags.rmeta \
	--extern dep_const_fn=$(axvisor_kvm_x86_rlib_prefix)const_fn.so \
	--extern rustversion=$(axvisor_kvm_x86_rlib_prefix)rustversion.so \
	--extern volatile=$(axvisor_kvm_x86_rlib_prefix)volatile.rmeta \
	--cfg 'feature="instructions"'
$(axvisor_kvm_x86_rlib_prefix)x86_64.rlib: \
	$(src)/vendor/registry/x86_64/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)bit_field.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bitflags.rlib \
	$(axvisor_kvm_x86_rlib_prefix)const_fn.so \
	$(axvisor_kvm_x86_rlib_prefix)rustversion.so \
	$(axvisor_kvm_x86_rlib_prefix)volatile.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)raw_cpuid.rlib: private rustc_target_flags = \
	--extern bitflags=$(axvisor_kvm_x86_rlib_prefix)bitflags.rmeta
$(axvisor_kvm_x86_rlib_prefix)raw_cpuid.rlib: \
	$(src)/vendor/registry/raw-cpuid/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)bitflags.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)x86.rlib: private rustc_target_flags = \
	--extern bit_field=$(axvisor_kvm_x86_rlib_prefix)bit_field.rmeta \
	--extern bitflags=$(axvisor_kvm_x86_rlib_prefix)bitflags1.rmeta \
	--extern raw_cpuid=$(axvisor_kvm_x86_rlib_prefix)raw_cpuid.rmeta
$(axvisor_kvm_x86_rlib_prefix)x86.rlib: \
	$(src)/vendor/registry/x86/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)bit_field.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bitflags1.rlib \
	$(axvisor_kvm_x86_rlib_prefix)raw_cpuid.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)ax_lazyinit.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)ax_lazyinit.rlib: \
	$(src)/vendor/upstream/ax-lazyinit/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib: private rustc_target_flags =
$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib: \
	$(src)/vendor/upstream/ax-memory-addr/src/lib.rs \
	$(axvisor_kvm_x86_rlib_dir) FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib: private rustc_target_flags = \
	--extern log=$(axvisor_kvm_x86_rlib_prefix)log.rmeta
$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib: \
	$(src)/vendor/upstream/ax-errno/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)log.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rlib: private rustc_target_flags = \
	--extern bitflags=$(axvisor_kvm_x86_rlib_prefix)bitflags.rmeta \
	--extern x86_64=$(axvisor_kvm_x86_rlib_prefix)x86_64.rmeta \
	--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rmeta
$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rlib: \
	$(src)/vendor/upstream/ax-page-table-entry/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)bitflags.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86_64.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)ax_memory_set.rlib: private rustc_target_flags = \
	--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rmeta
$(axvisor_kvm_x86_rlib_prefix)ax_memory_set.rlib: \
	$(src)/vendor/upstream/ax-memory-set/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)ax_page_table_multiarch.rlib: private rustc_target_flags = \
	--extern arrayvec=$(axvisor_kvm_x86_rlib_prefix)arrayvec.rmeta \
	--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rmeta \
	--extern ax_page_table_entry=$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rmeta \
	--extern log=$(axvisor_kvm_x86_rlib_prefix)log.rmeta \
	--extern x86=$(axvisor_kvm_x86_rlib_prefix)x86.rmeta
$(axvisor_kvm_x86_rlib_prefix)ax_page_table_multiarch.rlib: \
	$(src)/vendor/upstream/ax-page-table-multiarch/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)arrayvec.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rlib \
	$(axvisor_kvm_x86_rlib_prefix)log.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib: private rustc_target_flags = \
	--extern cfg_if=$(axvisor_kvm_x86_rlib_prefix)cfg_if.rmeta \
	--extern log=$(axvisor_kvm_x86_rlib_prefix)log.rmeta \
	--extern bit_field=$(axvisor_kvm_x86_rlib_prefix)bit_field.rmeta \
	--extern bitflags=$(axvisor_kvm_x86_rlib_prefix)bitflags.rmeta \
	--extern numeric_enum_macro=$(axvisor_kvm_x86_rlib_prefix)numeric_enum_macro.rmeta \
	--extern x86=$(axvisor_kvm_x86_rlib_prefix)x86.rmeta \
	--extern ax_errno=$(axvisor_kvm_x86_rlib_prefix)ax_errno.rmeta \
	--extern ax_lazyinit=$(axvisor_kvm_x86_rlib_prefix)ax_lazyinit.rmeta \
	--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rmeta \
	--extern ax_memory_set=$(axvisor_kvm_x86_rlib_prefix)ax_memory_set.rmeta \
	--extern ax_page_table_entry=$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rmeta \
	--extern ax_page_table_multiarch=$(axvisor_kvm_x86_rlib_prefix)ax_page_table_multiarch.rmeta \
	--cfg 'feature="vmx"'
$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib: \
	$(src)/vendor/upstream/axaddrspace/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	$(axvisor_kvm_x86_rlib_prefix)log.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bit_field.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bitflags.rlib \
	$(axvisor_kvm_x86_rlib_prefix)numeric_enum_macro.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_lazyinit.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_set.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_page_table_multiarch.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)ax_cpumask.rlib: private rustc_target_flags = \
	--extern bitmaps=$(axvisor_kvm_x86_rlib_prefix)bitmaps.rmeta
$(axvisor_kvm_x86_rlib_prefix)ax_cpumask.rlib: \
	$(src)/vendor/upstream/ax-cpumask/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)bitmaps.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)ax_kernel_guard.rlib: private rustc_target_flags = \
	--extern cfg_if=$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	--extern ax_crate_interface=$(axvisor_kvm_x86_rlib_prefix)ax_crate_interface.so
$(axvisor_kvm_x86_rlib_prefix)ax_kernel_guard.rlib: \
	$(src)/vendor/upstream/ax-kernel-guard/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_crate_interface.so FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rlib: private rustc_target_flags = \
	--extern cfg_if=$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	--extern ax_kernel_guard=$(axvisor_kvm_x86_rlib_prefix)ax_kernel_guard.rmeta \
	--cfg 'feature="smp"'
$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rlib: \
	$(src)/vendor/upstream/ax-kspin/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_kernel_guard.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)ax_percpu.rlib: private rustc_target_flags = \
	--extern cfg_if=$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	--extern x86=$(axvisor_kvm_x86_rlib_prefix)x86.rmeta \
	--extern ax_percpu_macros=$(axvisor_kvm_x86_rlib_prefix)ax_percpu_macros.so \
	--cfg 'feature="sp-naive"'
$(axvisor_kvm_x86_rlib_prefix)ax_percpu.rlib: \
	$(src)/vendor/upstream/ax-percpu/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_percpu_macros.so FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rlib: private rustc_target_flags = \
	--cfg 'feature="shell"' \
	--extern axvisor_api_proc=$(axvisor_kvm_x86_rlib_prefix)axvisor_api_proc.so \
	--extern ax_errno=$(axvisor_kvm_x86_rlib_prefix)ax_errno.rmeta \
	--extern axaddrspace=$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rmeta \
	--extern ax_crate_interface=$(axvisor_kvm_x86_rlib_prefix)ax_crate_interface.so \
	--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rmeta \
	--extern ax_cpumask=$(axvisor_kvm_x86_rlib_prefix)ax_cpumask.rmeta
$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rlib: \
	$(src)/vendor/upstream/axvisor_api/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)axvisor_api_proc.so \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_crate_interface.so \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_cpumask.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)axvcpu.rlib: private rustc_target_flags = \
	--extern ax_errno=$(axvisor_kvm_x86_rlib_prefix)ax_errno.rmeta \
	--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rmeta \
	--extern ax_percpu=$(axvisor_kvm_x86_rlib_prefix)ax_percpu.rmeta \
	--extern axaddrspace=$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rmeta \
	--extern axvisor_api=$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rmeta \
	--extern ax_kspin=$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rmeta
$(axvisor_kvm_x86_rlib_prefix)axvcpu.rlib: \
	$(src)/vendor/upstream/axvcpu/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_percpu.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rlib: private rustc_target_flags = \
	--extern ax_errno=$(axvisor_kvm_x86_rlib_prefix)ax_errno.rmeta \
	--extern log=$(axvisor_kvm_x86_rlib_prefix)log.rmeta
$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rlib: \
	$(src)/vendor/upstream/axvmconfig/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)log.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rlib: private rustc_target_flags = \
	--extern ax_errno=$(axvisor_kvm_x86_rlib_prefix)ax_errno.rmeta \
	--extern axaddrspace=$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rmeta \
	--extern axvmconfig=$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rmeta
$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rlib: \
	$(src)/vendor/upstream/axdevice_base/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)x86_vlapic.rlib: private rustc_target_flags = \
	--extern log=$(axvisor_kvm_x86_rlib_prefix)log.rmeta \
	--extern paste=$(axvisor_kvm_x86_rlib_prefix)paste.so \
	--extern tock_registers=$(axvisor_kvm_x86_rlib_prefix)tock_registers.rmeta \
	--extern bit=$(axvisor_kvm_x86_rlib_prefix)bit.rmeta \
	--extern ax_errno=$(axvisor_kvm_x86_rlib_prefix)ax_errno.rmeta \
	--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rmeta \
	--extern axaddrspace=$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rmeta \
	--extern axdevice_base=$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rmeta \
	--extern axvisor_api=$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rmeta \
	--extern ax_kspin=$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rmeta
$(axvisor_kvm_x86_rlib_prefix)x86_vlapic.rlib: \
	$(src)/vendor/upstream/x86_vlapic/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)log.rlib \
	$(axvisor_kvm_x86_rlib_prefix)paste.so \
	$(axvisor_kvm_x86_rlib_prefix)tock_registers.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bit.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)x86_vcpu.rlib: private axvisor_kvm_x86_rust_edition = --edition=2024
$(axvisor_kvm_x86_rlib_prefix)x86_vcpu.rlib: private rustc_target_flags = \
	--extern log=$(axvisor_kvm_x86_rlib_prefix)log.rmeta \
	--extern cfg_if=$(axvisor_kvm_x86_rlib_prefix)cfg_if.rmeta \
	--extern bitflags=$(axvisor_kvm_x86_rlib_prefix)bitflags.rmeta \
	--extern bit_field=$(axvisor_kvm_x86_rlib_prefix)bit_field.rmeta \
	--extern paste=$(axvisor_kvm_x86_rlib_prefix)paste.so \
	--extern x86=$(axvisor_kvm_x86_rlib_prefix)x86.rmeta \
	--extern x86_64=$(axvisor_kvm_x86_rlib_prefix)x86_64.rmeta \
	--extern raw_cpuid=$(axvisor_kvm_x86_rlib_prefix)raw_cpuid.rmeta \
	--extern numeric_enum_macro=$(axvisor_kvm_x86_rlib_prefix)numeric_enum_macro.rmeta \
	--extern tock_registers=$(axvisor_kvm_x86_rlib_prefix)tock_registers.rmeta \
	--extern ax_errno=$(axvisor_kvm_x86_rlib_prefix)ax_errno.rmeta \
	--extern ax_page_table_entry=$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rmeta \
	--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rmeta \
	--extern ax_crate_interface=$(axvisor_kvm_x86_rlib_prefix)ax_crate_interface.so \
	--extern axaddrspace=$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rmeta \
	--extern axvcpu=$(axvisor_kvm_x86_rlib_prefix)axvcpu.rmeta \
	--extern x86_vlapic=$(axvisor_kvm_x86_rlib_prefix)x86_vlapic.rmeta \
	--extern axdevice_base=$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rmeta \
	--extern axvisor_api=$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rmeta \
	--extern ax_kspin=$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rmeta \
	--cfg 'feature="vmx"'
$(axvisor_kvm_x86_rlib_prefix)x86_vcpu.rlib: \
	$(src)/vendor/upstream/x86_vcpu/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)log.rlib \
	$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bitflags.rlib \
	$(axvisor_kvm_x86_rlib_prefix)bit_field.rlib \
	$(axvisor_kvm_x86_rlib_prefix)paste.so \
	$(axvisor_kvm_x86_rlib_prefix)x86.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86_64.rlib \
	$(axvisor_kvm_x86_rlib_prefix)raw_cpuid.rlib \
	$(axvisor_kvm_x86_rlib_prefix)numeric_enum_macro.rlib \
	$(axvisor_kvm_x86_rlib_prefix)tock_registers.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_crate_interface.so \
	$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvcpu.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86_vlapic.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)axdevice.rlib: private rustc_target_flags = \
	--extern log=$(axvisor_kvm_x86_rlib_prefix)log.rmeta \
	--extern cfg_if=$(axvisor_kvm_x86_rlib_prefix)cfg_if.rmeta \
	--extern ax_kspin=$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rmeta \
	--extern ax_errno=$(axvisor_kvm_x86_rlib_prefix)ax_errno.rmeta \
	--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rmeta \
	--extern axvmconfig=$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rmeta \
	--extern axaddrspace=$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rmeta \
	--extern axdevice_base=$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rmeta \
	--extern x86_vlapic=$(axvisor_kvm_x86_rlib_prefix)x86_vlapic.rmeta
$(axvisor_kvm_x86_rlib_prefix)axdevice.rlib: \
	$(src)/vendor/upstream/axdevice/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)log.rlib \
	$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86_vlapic.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(axvisor_kvm_x86_rlib_prefix)axvm.rlib: private rustc_target_flags = \
	--extern log=$(axvisor_kvm_x86_rlib_prefix)log.rmeta \
	--extern cfg_if=$(axvisor_kvm_x86_rlib_prefix)cfg_if.rmeta \
	--extern spin=$(axvisor_kvm_x86_rlib_prefix)spin.rmeta \
	--extern ax_errno=$(axvisor_kvm_x86_rlib_prefix)ax_errno.rmeta \
	--extern ax_cpumask=$(axvisor_kvm_x86_rlib_prefix)ax_cpumask.rmeta \
	--extern ax_kspin=$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rmeta \
	--extern ax_memory_addr=$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rmeta \
	--extern ax_page_table_entry=$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rmeta \
	--extern ax_page_table_multiarch=$(axvisor_kvm_x86_rlib_prefix)ax_page_table_multiarch.rmeta \
	--extern ax_percpu=$(axvisor_kvm_x86_rlib_prefix)ax_percpu.rmeta \
	--extern axvcpu=$(axvisor_kvm_x86_rlib_prefix)axvcpu.rmeta \
	--extern axaddrspace=$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rmeta \
	--extern axvisor_api=$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rmeta \
	--extern axdevice=$(axvisor_kvm_x86_rlib_prefix)axdevice.rmeta \
	--extern axdevice_base=$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rmeta \
	--extern axvmconfig=$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rmeta \
	--extern x86_vcpu=$(axvisor_kvm_x86_rlib_prefix)x86_vcpu.rmeta \
	--cfg 'feature="vmx"'
$(axvisor_kvm_x86_rlib_prefix)axvm.rlib: \
	$(src)/vendor/upstream/axvm/src/lib.rs \
	$(axvisor_kvm_x86_rlib_prefix)log.rlib \
	$(axvisor_kvm_x86_rlib_prefix)cfg_if.rlib \
	$(axvisor_kvm_x86_rlib_prefix)spin.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_cpumask.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_page_table_entry.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_page_table_multiarch.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_percpu.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvcpu.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axdevice.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86_vcpu.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_target_library)

$(obj)/axvisor_kvm_x86_bridge_raw.o: \
	$(src)/axvisor_kvm_x86_bridge.rs \
	$(axvisor_kvm_x86_rlib_prefix)ax_errno.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_kspin.rlib \
	$(axvisor_kvm_x86_rlib_prefix)ax_memory_addr.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axaddrspace.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axdevice.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axdevice_base.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvm.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvmconfig.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvcpu.rlib \
	$(axvisor_kvm_x86_rlib_prefix)axvisor_api.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86_vcpu.rlib \
	$(axvisor_kvm_x86_rlib_prefix)x86_vlapic.rlib FORCE
	+$(call if_changed_dep,axvisor_kvm_x86_rustc_bridge_object)

$(obj)/axvisor_kvm_x86_bridge.o: \
	$(obj)/axvisor_kvm_x86_bridge_raw.o \
	$(axvisor_kvm_x86_target_rlibs) FORCE
	+$(call if_changed,axvisor_kvm_x86_ld_object)

ifneq ($(CONFIG_AXVISOR_ADAPTER),)
ifdef KBUILD_EXTMOD
obj-m += axvisor_adapter.o
endif

axvisor_adapter-y := \
	axvisor_percpu_start.o \
	axvisor_linux_bridge.o \
	axvisor_percpu_stop.o \
	axvisor_adapter_main.o \
	axvisor_adapter_shim.o

CFLAGS_axvisor_adapter_shim.o += -Wno-missing-prototypes -Wno-unused-function

ccflags-remove-y += -ftrivial-auto-var-init=zero
ccflags-remove-y += $(CC_AUTO_VAR_INIT_ZERO_ENABLER)
ccflags-remove-y += -march=rv64imac_zacas_zabha

ifeq ($(CONFIG_RISCV),y)
$(obj)/axvisor_adapter_shim.o: KBUILD_CFLAGS := \
	$(filter-out -march=%,$(KBUILD_CFLAGS)) -march=rv64imac
$(obj)/axvisor_adapter.mod.o: KBUILD_CFLAGS := \
	$(filter-out -march=%,$(KBUILD_CFLAGS)) -march=rv64imac
endif

$(obj)/axvisor_adapter_main.o: private rust_flags += \
	-L $(obj)
$(obj)/axvisor_adapter_main.o: $(obj)/libaxvisor_core.rlib $(obj)/libax_percpu.rlib
$(obj)/axvisor_linux_bridge.o: $(obj)/libaxvisor_linux_bridge.a

axvisor_repo_root := $(abspath $(srctree)/..)
AXVISOR_LINUX_VM_CONFIG ?= $(axvisor_repo_root)/tools/riscv64-linux-host-vm.toml
AXVISOR_LINUX_GUEST_KERNEL ?= $(axvisor_repo_root)/ivans-asterinas-axvisor-host/target/axvisor/images/qemu_riscv64_linux/qemu-riscv64
AXVISOR_LINUX_GUEST_INITRAMFS ?= $(axvisor_repo_root)/ivans-asterinas-axvisor-host/target/axvisor/images/qemu_riscv64_linux/initramfs.cpio.gz
AXVISOR_LINUX_GUEST_DTB ?= $(axvisor_repo_root)/ivans-asterinas-axvisor-host/test/axvisor/riscv64/linux/linux-riscv64-qemu-smp1.dtb
AXVISOR_LINUX_EMBED_INITRAMFS ?= 0

axvisor_static_vm_configs_rs := $(src)/vendor/upstream/axvisor_core/src/vm_configs_static.rs
axvisor_static_generated_dir := $(src)/generated
axvisor_linux_guest_dtb_dep := $(wildcard $(AXVISOR_LINUX_GUEST_DTB))

$(axvisor_static_vm_configs_rs): \
	$(axvisor_repo_root)/tools/gen-axvisor-linux-vm-configs-static.py \
	$(AXVISOR_LINUX_VM_CONFIG) \
	$(AXVISOR_LINUX_GUEST_KERNEL) \
	$(AXVISOR_LINUX_GUEST_INITRAMFS) \
	$(axvisor_linux_guest_dtb_dep) FORCE
	$(Q)python3 $(axvisor_repo_root)/tools/gen-axvisor-linux-vm-configs-static.py \
		--config $(AXVISOR_LINUX_VM_CONFIG) \
		--kernel $(AXVISOR_LINUX_GUEST_KERNEL) \
		$(if $(axvisor_linux_guest_dtb_dep),--dtb $(AXVISOR_LINUX_GUEST_DTB),) \
		--initramfs $(AXVISOR_LINUX_GUEST_INITRAMFS) \
		$(if $(filter 1,$(AXVISOR_LINUX_EMBED_INITRAMFS)),--embed-initramfs,) \
		--generated-dir $(axvisor_static_generated_dir) \
		--output $@

# AxVisor proc-macro staging, phase 1:
# keep the adapter module object list separate from the host-side Rust crate
# artifacts that will be wired in later.

# Provided by rust/Makefile. AxVisor depends on these but does not redefine them.
axvisor_rust_proc_macro_common := \
	libproc_macro2.rlib \
	libquote.rlib \
	libsyn.rlib

ifeq ($(CONFIG_X86_64),y)
axvisor_target_triple := x86_64-unknown-none
axvisor_target_flags := \
	--target=$(axvisor_kvm_x86_target_spec) \
	-Cpanic=abort \
	-Crelocation-model=static \
	-Cno-redzone=y \
	-Ccode-model=kernel
axvisor_runtime_arch_flags := \
	--cfg axvisor_host_x86_64
else
axvisor_target_triple := riscv64imac-unknown-none-elf
axvisor_target_flags := \
	--target=$(axvisor_target_triple) \
	-Cpanic=abort
axvisor_runtime_arch_flags := \
	--cfg axvisor_host_riscv64
endif

axvisor_target_sysroot := $(shell $(RUSTC) --print sysroot)
ifeq ($(CONFIG_X86_64),y)
axvisor_target_core_rlib := $(axvisor_kvm_x86_runtime_core)
axvisor_target_core_rmeta := $(axvisor_kvm_x86_core_rmeta)
axvisor_target_compiler_builtins_rlib := $(axvisor_kvm_x86_runtime_compiler_builtins)
axvisor_target_compiler_builtins_rmeta := $(axvisor_kvm_x86_compiler_builtins_rmeta)
axvisor_target_alloc_rlib := $(axvisor_kvm_x86_runtime_alloc)
axvisor_target_alloc_rmeta := $(axvisor_kvm_x86_alloc_rmeta)
axvisor_target_runtime_deps := \
	$(axvisor_target_core_rlib) \
	$(axvisor_target_compiler_builtins_rlib) \
	$(axvisor_target_alloc_rlib)
axvisor_target_object_sysroot := /dev/null
else
axvisor_target_core_rlib := \
	$(firstword $(wildcard $(axvisor_target_sysroot)/lib/rustlib/$(axvisor_target_triple)/lib/libcore-*.rlib))
axvisor_target_core_rmeta := \
	$(firstword $(wildcard $(axvisor_target_sysroot)/lib/rustlib/$(axvisor_target_triple)/lib/libcore-*.rmeta))
axvisor_target_compiler_builtins_rlib := \
	$(firstword $(wildcard $(axvisor_target_sysroot)/lib/rustlib/$(axvisor_target_triple)/lib/libcompiler_builtins-*.rlib))
axvisor_target_compiler_builtins_rmeta := \
	$(firstword $(wildcard $(axvisor_target_sysroot)/lib/rustlib/$(axvisor_target_triple)/lib/libcompiler_builtins-*.rmeta))
axvisor_target_alloc_rlib := \
	$(firstword $(wildcard $(axvisor_target_sysroot)/lib/rustlib/$(axvisor_target_triple)/lib/liballoc-*.rlib))
axvisor_target_alloc_rmeta := \
	$(firstword $(wildcard $(axvisor_target_sysroot)/lib/rustlib/$(axvisor_target_triple)/lib/liballoc-*.rmeta))
axvisor_target_runtime_deps :=
axvisor_target_object_sysroot := $(axvisor_target_sysroot)
endif
axvisor_target_rustlib_dir := \
	$(dir $(axvisor_target_core_rmeta))
axvisor_target_rust_edition := --edition=2021

axvisor_proc_macro2_flags := \
	--cap-lints=allow \
	--cfg 'feature="proc-macro"' \
	--cfg wrap_proc_macro

axvisor_quote_skip_flags := \
	--edition=2021

axvisor_quote_flags := \
	--edition=2018 \
	--cap-lints=allow \
	--extern proc_macro2 \
	--cfg 'feature="proc-macro"'

axvisor_syn_flags := \
	--cap-lints=allow \
	--extern proc_macro2 \
	--extern quote \
	--cfg 'feature="clone-impls"' \
	--cfg 'feature="derive"' \
	--cfg 'feature="full"' \
	--cfg 'feature="parsing"' \
	--cfg 'feature="printing"' \
	--cfg 'feature="proc-macro"' \
	--cfg 'feature="visit-mut"'

# First-stage AxVisor-specific host-side rlibs for proc-macro support.
axvisor_rust_proc_macro_rlibs := \
	libcfg_if.rlib \
	libbitflags.rlib \
	liblog.rlib \
	libarrayvec.rlib \
	libspin.rlib \
	libbare_metal.rlib \
	libbit_field.rlib \
	libbitmaps.rlib \
	libriscv.rlib \
	libriscv_decode.rlib \
	libsbi_spec.rlib \
	libsbi_rt.rlib \
	libtock_registers.rlib \
	libmemoffset.rlib \
	librustsbi.rlib \
	libequivalent.rlib \
	libmemchr.rlib \
	libhashbrown.rlib \
	libindexmap.rlib \
	libwinnow.rlib

# First-stage AxVisor-specific proc-macro dynamic libraries.
axvisor_rust_proc_macros := \
	libconst_fn.so \
	librustversion.so \
	libax_crate_interface.so \
	libax_percpu_macros.so \
	libaxvisor_api_proc.so \
	librustsbi_macros.so

# First-stage runtime base rlibs: start only from the lightest crates that do
# not immediately expand into the larger log/derive/synchronization chains.
axvisor_rust_runtime_base_rlibs := \
	libax_lazyinit.rlib \
	libax_cpumask.rlib \
	libax_kernel_guard.rlib \
	libax_kspin.rlib \
	libax_timer_list.rlib \
	libax_memory_addr.rlib \
	libvolatile.rlib \
	libx86_64.rlib \
	libax_errno.rlib \
	libax_page_table_entry.rlib \
	libax_memory_set.rlib \
	libax_page_table_multiarch.rlib \
	libaxaddrspace.rlib \
	libax_percpu.rlib \
	libaxvisor_api.rlib \
	libaxvcpu.rlib \
	libaxvmconfig.rlib \
	libaxdevice_base.rlib \
	libriscv_h.rlib \
	libriscv_vplic.rlib \
	libaxdevice.rlib \
	libriscv_vcpu.rlib \
	libaxvm.rlib \
	libaxhvc.rlib \
	libfdt_parser.rlib \
	libaxvisor_core.rlib

ifeq ($(CONFIG_X86_64),y)
axvisor_rust_proc_macro_rlibs := \
	$(filter-out \
		libriscv.rlib \
		libriscv_decode.rlib \
		libsbi_spec.rlib \
		libsbi_rt.rlib \
		librustsbi.rlib, \
		$(axvisor_rust_proc_macro_rlibs)) \
	libbitflags1.rlib \
	libbit.rlib \
	libnumeric_enum_macro.rlib \
	libraw_cpuid.rlib \
	libx86.rlib

axvisor_rust_proc_macros := \
	$(filter-out librustsbi_macros.so,$(axvisor_rust_proc_macros)) \
	libpaste.so

axvisor_rust_runtime_base_rlibs := \
	$(filter-out \
		libriscv_h.rlib \
		libriscv_vplic.rlib \
		libriscv_vcpu.rlib, \
		$(axvisor_rust_runtime_base_rlibs)) \
	libx86_vlapic.rlib \
	libx86_vcpu.rlib
endif

axvisor_rust_runtime_bridge_archives := \
	libaxvisor_linux_bridge.a

# Phase-1 contract:
# - declare the Rust crate artifacts that AxVisor will need
# - keep them as always-built side artifacts instead of mixing them into the
#   adapter module object list
# - defer the actual rustc rule wiring to the next step
always-$(CONFIG_AXVISOR_ADAPTER) += \
	$(axvisor_target_runtime_deps) \
	$(axvisor_rust_proc_macro_rlibs) \
	$(axvisor_rust_proc_macros) \
	$(axvisor_rust_runtime_base_rlibs) \
	$(axvisor_rust_runtime_bridge_archives)

targets += \
	$(axvisor_target_runtime_deps) \
	$(axvisor_rust_proc_macro_rlibs) \
	$(axvisor_rust_proc_macros) \
	$(axvisor_rust_runtime_base_rlibs) \
	$(axvisor_rust_runtime_bridge_archives)

$(addprefix $(obj)/,$(axvisor_rust_proc_macro_rlibs) $(axvisor_rust_runtime_base_rlibs) $(axvisor_rust_runtime_bridge_archives)): $(axvisor_target_runtime_deps)

# Phase-1 dependency skeleton:
# encode the dependency order first, before wiring the actual rustc rules.

libaxvcpu.rlib: \
	libax_errno.rlib \
	libax_memory_addr.rlib \
	libax_percpu.rlib \
	libaxaddrspace.rlib \
	libaxvisor_api.rlib \
	libax_kspin.rlib

libaxdevice.rlib: \
	liblog.rlib \
	libcfg_if.rlib \
	libax_kspin.rlib \
	libax_errno.rlib \
	libax_memory_addr.rlib \
	libaxvmconfig.rlib \
	libaxaddrspace.rlib \
	libaxdevice_base.rlib \
	libriscv_vplic.rlib

libriscv_vcpu.rlib: \
	liblog.rlib \
	libcfg_if.rlib \
	libbitflags.rlib \
	libbit_field.rlib \
	libax_crate_interface.so \
	libriscv.rlib \
	libriscv_h.rlib \
	libriscv_decode.rlib \
	librustsbi.rlib \
	libsbi_rt.rlib \
	libsbi_spec.rlib \
	libtock_registers.rlib \
	libmemoffset.rlib \
	libax_errno.rlib \
	libax_page_table_entry.rlib \
	libax_memory_addr.rlib \
	libaxaddrspace.rlib \
	libaxvcpu.rlib \
	libaxvisor_api.rlib

libaxvm.rlib: \
	liblog.rlib \
	libcfg_if.rlib \
	libspin.rlib \
	libax_errno.rlib \
	libax_cpumask.rlib \
	libax_kspin.rlib \
	libax_memory_addr.rlib \
	libax_page_table_entry.rlib \
	libax_page_table_multiarch.rlib \
	libax_percpu.rlib \
	libaxvcpu.rlib \
	libaxaddrspace.rlib \
	libaxvisor_api.rlib \
	libaxdevice.rlib \
	libaxdevice_base.rlib \
	libaxvmconfig.rlib \
	libriscv_vcpu.rlib

libaxvisor_core.rlib: \
	libspin.rlib \
	liblog.rlib \
	libhashbrown.rlib \
	libax_cpumask.rlib \
	libax_kernel_guard.rlib \
	libax_kspin.rlib \
	libax_lazyinit.rlib \
	libax_percpu.rlib \
	libax_timer_list.rlib \
	libax_errno.rlib \
	libax_memory_addr.rlib \
	libax_page_table_multiarch.rlib \
	libaxaddrspace.rlib \
	libaxhvc.rlib \
	libaxvcpu.rlib \
	libaxvm.rlib \
	libaxvisor_api.rlib \
	libfdt_parser.rlib \
	libriscv_vplic.rlib \
	libriscv_vcpu.rlib

libaxhvc.rlib: libax_errno.rlib

libax_cpumask.rlib: libbitmaps.rlib

librustsbi.rlib: \
	librustsbi_macros.so \
	libsbi_rt.rlib \
	libsbi_spec.rlib

libsbi_rt.rlib: libsbi_spec.rlib

libsbi_spec.rlib: libbitflags.rlib

librustsbi_macros.so: \
	libproc_macro2.rlib \
	libquote.rlib \
	libsyn.rlib

libriscv_vplic.rlib: \
	libax_kspin.rlib \
	libaxaddrspace.rlib \
	libaxdevice_base.rlib \
	libaxvisor_api.rlib \
	libax_errno.rlib \
	libbitmaps.rlib \
	liblog.rlib \
	libriscv_h.rlib

libriscv_h.rlib: \
	libriscv.rlib \
	libbare_metal.rlib \
	libbitflags.rlib \
	libbit_field.rlib \
	liblog.rlib

libaxdevice_base.rlib: \
	libaxaddrspace.rlib \
	libax_errno.rlib \
	libaxvmconfig.rlib

libaxvmconfig.rlib: \
	libax_errno.rlib \
	liblog.rlib

libriscv.rlib: \
	libbare_metal.rlib \
	libbitflags.rlib \
	libbit_field.rlib \
	liblog.rlib

libax_percpu.rlib: \
	libcfg_if.rlib \
	libspin.rlib \
	libax_percpu_macros.so

libaxvisor_api.rlib: \
	libaxvisor_api_proc.so \
	libax_errno.rlib \
	libaxaddrspace.rlib \
	libax_crate_interface.so \
	libax_memory_addr.rlib \
	libax_cpumask.rlib

libaxaddrspace.rlib: \
	libcfg_if.rlib \
	liblog.rlib \
	libax_errno.rlib \
	libax_lazyinit.rlib \
	libax_memory_addr.rlib \
	libax_memory_set.rlib \
	libax_page_table_entry.rlib \
	libax_page_table_multiarch.rlib

libax_page_table_entry.rlib: \
	libbitflags.rlib \
	libx86_64.rlib \
	libax_memory_addr.rlib

libx86_64.rlib: \
	libbit_field.rlib \
	libbitflags.rlib \
	libconst_fn.so \
	librustversion.so \
	libvolatile.rlib

libax_errno.rlib: liblog.rlib

libhashbrown.rlib: libequivalent.rlib

libindexmap.rlib: libequivalent.rlib libhashbrown.rlib

libwinnow.rlib: libmemchr.rlib

libax_crate_interface.so: \
	libproc_macro2.rlib \
	libquote.rlib \
	libsyn.rlib

libax_percpu_macros.so: \
	libcfg_if.rlib \
	libproc_macro2.rlib \
	libquote.rlib \
	libsyn.rlib

libaxvisor_api_proc.so: \
	libproc_macro2.rlib \
	libquote.rlib \
	libsyn.rlib

# First real Rust crate rules: start from the lowest-risk host-side rlibs that
# do not need additional --extern wiring.
quiet_cmd_axvisor_rustc_procmacrolibrary = $(RUSTC_OR_CLIPPY_QUIET) PL $@
      cmd_axvisor_rustc_procmacrolibrary = \
	$(if $(skip_clippy),$(RUSTC),$(RUSTC_OR_CLIPPY)) \
		$(filter-out $(skip_flags) -Dunsafe_op_in_unsafe_fn,$(rust_common_flags) $(rustc_target_flags) $(axvisor_runtime_arch_flags)) \
		--emit=dep-info=$(depfile) --emit=link=$@ --crate-type rlib -O \
		--out-dir $(obj) -L$(obj) \
		--crate-name $(patsubst lib%.rlib,%,$(notdir $@)) $<

quiet_cmd_axvisor_rustc_target_library = $(if $(skip_clippy),RUSTC,$(RUSTC_OR_CLIPPY_QUIET)) TL $@
      cmd_axvisor_rustc_target_library = \
	$(if $(skip_clippy),$(RUSTC),$(RUSTC_OR_CLIPPY)) \
		$(filter-out $(skip_flags) -Dunsafe_op_in_unsafe_fn --edition=%,$(rust_common_flags) $(rustc_target_flags)) $(axvisor_target_rust_edition) $(axvisor_target_flags) $(axvisor_runtime_arch_flags) \
		--extern core=$(axvisor_target_core_rmeta) \
		--extern compiler_builtins=$(axvisor_target_compiler_builtins_rmeta) \
		--extern alloc=$(axvisor_target_alloc_rmeta) \
		--emit=dep-info=$(depfile) --emit=link=$@ \
		--emit=metadata=$(dir $@)$(patsubst %.rlib,%.rmeta,$(notdir $@)) \
		--crate-type rlib -O \
		--out-dir $(obj) -L$(obj) \
		-L dependency=$(obj) -L dependency=$(axvisor_target_rustlib_dir) \
		--crate-name $(patsubst lib%.rlib,%,$(notdir $@)) $< \
		--sysroot=/dev/null \
		-Zunstable-options

axvisor_target_rustc_codegen_flags := \
	-Cdebuginfo=0 \
	-Cforce-unwind-tables=n \
	-Cllvm-args=--generate-type-units=false \
	-C link-arg=--no-relax

quiet_cmd_axvisor_rustc_target_object = $(if $(skip_clippy),RUSTC,$(RUSTC_OR_CLIPPY_QUIET)) TO $@
      cmd_axvisor_rustc_target_object = \
	$(if $(skip_clippy),$(RUSTC),$(RUSTC_OR_CLIPPY)) \
		$(filter-out $(skip_flags) -Dunsafe_op_in_unsafe_fn --edition=%,$(rust_common_flags) $(rustc_target_flags)) $(axvisor_target_rust_edition) $(axvisor_target_flags) $(axvisor_runtime_arch_flags) $(axvisor_target_rustc_codegen_flags) \
		--extern core=$(axvisor_target_core_rlib) \
		--extern compiler_builtins=$(axvisor_target_compiler_builtins_rlib) \
		--extern alloc=$(axvisor_target_alloc_rlib) \
		--emit=dep-info=$(depfile) --emit=link=$@ \
		--crate-type staticlib -O \
		--out-dir $(obj) -L$(obj) \
		-L dependency=$(obj) -L dependency=$(axvisor_target_rustlib_dir) \
		--crate-name $(patsubst lib%.a,%,$(notdir $@)) $< \
		--sysroot=$(axvisor_target_object_sysroot) \
		-Zunstable-options

quiet_cmd_axvisor_partial_link_archive = LD      $@
      cmd_axvisor_partial_link_archive = \
	$(LD) -r -o $@ --whole-archive $< --no-whole-archive

quiet_cmd_axvisor_strip_linker_incompatible = OBJCOPY $@
      cmd_axvisor_strip_linker_incompatible = \
	$(OBJCOPY) \
		--remove-section=.eh_frame \
		--remove-section=.rela.eh_frame \
		--remove-section=.debug_aranges \
		--remove-section=.rela.debug_aranges \
		--remove-section=.debug_info \
		--remove-section=.rela.debug_info \
		--remove-section=.debug_abbrev \
		--remove-section=.debug_line \
		--remove-section=.rela.debug_line \
		--remove-section=.debug_frame \
		--remove-section=.rela.debug_frame \
		--remove-section=.debug_str \
		--remove-section=.debug_ranges \
		--remove-section=.rela.debug_ranges \
		--remove-section=.debug_line_str \
		--remove-section=.debug_loclists \
		--remove-section=.rela.debug_loclists \
		--remove-section=.debug_rnglists \
		--remove-section=.rela.debug_rnglists \
		--remove-section=.llvmbc \
		--remove-section=.llvmcmd \
		$@

$(obj)/libcfg_if.rlib: private skip_clippy = 1
$(obj)/libcfg_if.rlib: $(src)/vendor/registry/cfg-if/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libbitflags.rlib: private skip_clippy = 1
$(obj)/libbitflags.rlib: $(src)/vendor/registry/bitflags/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libbitflags1.rlib: private skip_clippy = 1
$(obj)/libbitflags1.rlib: $(src)/vendor/registry/bitflags-1/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/liblog.rlib: private skip_clippy = 1
$(obj)/liblog.rlib: $(src)/vendor/registry/log/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libarrayvec.rlib: private skip_clippy = 1
$(obj)/libarrayvec.rlib: $(src)/vendor/registry/arrayvec/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libspin.rlib: private skip_clippy = 1
$(obj)/libspin.rlib: private rustc_target_flags = \
	--cfg='feature="once"' \
	--cfg='feature="mutex"' \
	--cfg='feature="spin_mutex"'
$(obj)/libspin.rlib: $(src)/vendor/registry/spin/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libbare_metal.rlib: private skip_clippy = 1
$(obj)/libbare_metal.rlib: $(src)/vendor/registry/bare-metal/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libbit_field.rlib: private skip_clippy = 1
$(obj)/libbit_field.rlib: $(src)/vendor/registry/bit_field/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libbitmaps.rlib: private skip_clippy = 1
$(obj)/libbitmaps.rlib: $(src)/vendor/registry/bitmaps/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libbit.rlib: private skip_clippy = 1
$(obj)/libbit.rlib: $(src)/vendor/registry/bit/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libnumeric_enum_macro.rlib: private skip_clippy = 1
$(obj)/libnumeric_enum_macro.rlib: $(src)/vendor/registry/numeric-enum-macro/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libriscv.rlib: private skip_clippy = 1
$(obj)/libriscv.rlib: private skip_flags = --edition=2021
$(obj)/libriscv.rlib: private axvisor_target_rust_edition = --edition=2015
$(obj)/libriscv.rlib: private rustc_target_flags = \
	--extern bare_metal \
	--extern bitflags \
	--extern bit_field \
	--extern log \
	--cfg 'feature="inline-asm"' \
	--cfg riscv \
	--cfg riscv64
$(obj)/libriscv.rlib: \
	$(src)/vendor/registry/riscv/src/lib.rs \
	$(obj)/libbare_metal.rlib $(obj)/libbitflags.rlib \
	$(obj)/libbit_field.rlib $(obj)/liblog.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libriscv_decode.rlib: private skip_clippy = 1
$(obj)/libriscv_decode.rlib: $(src)/vendor/registry/riscv-decode/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libsbi_spec.rlib: private skip_clippy = 1
$(obj)/libsbi_spec.rlib: private rustc_target_flags = \
	--extern bitflags=$(obj)/libbitflags.rmeta \
	--cfg='feature="legacy"'
$(obj)/libsbi_spec.rlib: \
	$(src)/vendor/registry/sbi-spec/src/lib.rs \
	$(obj)/libbitflags.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libsbi_rt.rlib: private skip_clippy = 1
$(obj)/libsbi_rt.rlib: private rustc_target_flags = \
	--extern sbi_spec=$(obj)/libsbi_spec.rmeta \
	--cfg='feature="integer-impls"'
$(obj)/libsbi_rt.rlib: \
	$(src)/vendor/registry/sbi-rt/src/lib.rs \
	$(obj)/libsbi_spec.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libtock_registers.rlib: private skip_clippy = 1
$(obj)/libtock_registers.rlib: private rustc_target_flags = \
	--cfg='feature="register_types"'
$(obj)/libtock_registers.rlib: $(src)/vendor/registry/tock-registers/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libmemoffset.rlib: private skip_clippy = 1
$(obj)/libmemoffset.rlib: private rustc_target_flags = \
	--cfg tuple_ty \
	--cfg allow_clippy \
	--cfg maybe_uninit \
	--cfg raw_ref_macros \
	--cfg stable_const \
	--cfg stable_offset_of
$(obj)/libmemoffset.rlib: $(src)/vendor/registry/memoffset/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/librustsbi.rlib: private skip_clippy = 1
$(obj)/librustsbi.rlib: private rustc_target_flags = \
	--extern rustsbi_macros=$(obj)/librustsbi_macros.so \
	--extern sbi_rt=$(obj)/libsbi_rt.rmeta \
	--extern sbi_spec=$(obj)/libsbi_spec.rmeta \
	--cfg='feature="forward"'
$(obj)/librustsbi.rlib: \
	$(src)/vendor/registry/rustsbi/src/lib.rs \
	$(obj)/librustsbi_macros.so $(obj)/libsbi_rt.rlib \
	$(obj)/libsbi_spec.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libequivalent.rlib: private skip_clippy = 1
$(obj)/libequivalent.rlib: $(src)/vendor/registry/equivalent/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libmemchr.rlib: private skip_clippy = 1
$(obj)/libmemchr.rlib: $(src)/vendor/registry/memchr/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libhashbrown.rlib: private skip_clippy = 1
$(obj)/libhashbrown.rlib: private rustc_target_flags = \
	--extern equivalent
$(obj)/libhashbrown.rlib: $(src)/vendor/registry/hashbrown/src/lib.rs \
	$(obj)/libequivalent.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libindexmap.rlib: private skip_clippy = 1
$(obj)/libindexmap.rlib: private rustc_target_flags = \
	--extern equivalent \
	--extern hashbrown=$(obj)/libhashbrown.rlib
$(obj)/libindexmap.rlib: $(src)/vendor/registry/indexmap/src/lib.rs \
	$(obj)/libequivalent.rlib $(obj)/libhashbrown.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libwinnow.rlib: private skip_clippy = 1
$(obj)/libwinnow.rlib: private rustc_target_flags = \
	--extern memchr
$(obj)/libwinnow.rlib: $(src)/vendor/registry/winnow/src/lib.rs \
	$(obj)/libmemchr.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libconst_fn.so: private skip_clippy = 1
$(obj)/libconst_fn.so: \
	$(src)/vendor/registry/const_fn/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_procmacro)

$(obj)/libpaste.so: private skip_clippy = 1
$(obj)/libpaste.so: \
	$(src)/vendor/registry/paste/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_procmacro)

$(obj)/librustversion.so: private skip_clippy = 1
$(obj)/librustversion.so: \
	$(src)/vendor/registry/rustversion/src/lib.rs \
	$(obj)/version.expr FORCE
	+$(call if_changed_dep,axvisor_rustc_procmacro)

$(obj)/libproc_macro2.rlib: private skip_clippy = 1
$(obj)/libproc_macro2.rlib: private rustc_target_flags = $(axvisor_proc_macro2_flags)
$(obj)/libproc_macro2.rlib: $(srctree)/rust/proc-macro2/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_procmacrolibrary)

$(obj)/libquote.rlib: private skip_clippy = 1
$(obj)/libquote.rlib: private skip_flags = $(axvisor_quote_skip_flags)
$(obj)/libquote.rlib: private rustc_target_flags = $(axvisor_quote_flags)
$(obj)/libquote.rlib: $(srctree)/rust/quote/lib.rs $(obj)/libproc_macro2.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_procmacrolibrary)

$(obj)/libsyn.rlib: private skip_clippy = 1
$(obj)/libsyn.rlib: private rustc_target_flags = $(axvisor_syn_flags)
$(obj)/libsyn.rlib: $(srctree)/rust/syn/lib.rs $(obj)/libquote.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_procmacrolibrary)

quiet_cmd_axvisor_rustc_procmacro = $(RUSTC_OR_CLIPPY_QUIET) P $@
      cmd_axvisor_rustc_procmacro = \
	OUT_DIR=$(abspath $(obj)) $(RUSTC_OR_CLIPPY) $(filter-out -Dunsafe_op_in_unsafe_fn,$(rust_common_flags) $(rustc_target_flags) $(axvisor_runtime_arch_flags)) \
		-Clinker-flavor=gcc -Clinker=$(HOSTCC) \
		-Clink-args='$(call escsq,$(KBUILD_PROCMACROLDFLAGS))' \
		--emit=dep-info=$(depfile) --emit=link=$@ --extern proc_macro \
		--crate-type proc-macro -L$(obj) \
		--crate-name $(patsubst lib%.so,%,$(notdir $@)) \
		@$(objtree)/include/generated/rustc_cfg $<

quiet_cmd_axvisor_rustversion_expr = GEN     $@
      cmd_axvisor_rustversion_expr = \
	printf 'crate::version::Version { minor: 96, patch: 0, channel: crate::version::Channel::Nightly(crate::date::Date { year: 2026, month: 4, day: 2 }) }\n' > $@

$(obj)/version.expr: FORCE
	+$(call if_changed,axvisor_rustversion_expr)

$(obj)/libax_crate_interface.so: private rustc_target_flags = \
	--extern proc_macro2 \
	--extern quote \
	--extern syn
$(obj)/libax_crate_interface.so: \
	$(src)/vendor/upstream/ax-crate-interface/src/lib.rs \
	$(obj)/libproc_macro2.rlib $(obj)/libquote.rlib $(obj)/libsyn.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_procmacro)

ifeq ($(CONFIG_X86_64),y)
axvisor_percpu_feature_flags := --cfg 'feature="custom-base"'
else
axvisor_percpu_feature_flags := --cfg 'feature="sp-naive"'
endif

$(obj)/libax_percpu_macros.so: private rustc_target_flags = \
	--extern cfg_if=$(obj)/libcfg_if.rlib \
	--extern proc_macro2 \
	--extern quote \
	--extern syn \
	$(axvisor_percpu_feature_flags)
$(obj)/libax_percpu_macros.so: \
	$(src)/vendor/upstream/ax-percpu-macros/src/lib.rs \
	$(obj)/libcfg_if.rlib $(obj)/libproc_macro2.rlib \
	$(obj)/libquote.rlib $(obj)/libsyn.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_procmacro)

$(obj)/libaxvisor_api_proc.so: private rustc_target_flags = \
	--extern proc_macro2 \
	--extern quote \
	--extern syn
$(obj)/libaxvisor_api_proc.so: \
	$(src)/vendor/upstream/axvisor_api_proc/src/lib.rs \
	$(obj)/libproc_macro2.rlib $(obj)/libquote.rlib \
	$(obj)/libsyn.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_procmacro)

$(obj)/librustsbi_macros.so: private rustc_target_flags = \
	--extern proc_macro2 \
	--extern quote \
	--extern syn
$(obj)/librustsbi_macros.so: \
	$(src)/vendor/registry/rustsbi-macros/src/lib.rs \
	$(obj)/libproc_macro2.rlib $(obj)/libquote.rlib $(obj)/libsyn.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_procmacro)

# Start the runtime base chain from the two lowest-risk crates.
$(obj)/libax_lazyinit.rlib: private skip_clippy = 1
$(obj)/libax_lazyinit.rlib: $(src)/vendor/upstream/ax-lazyinit/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libax_cpumask.rlib: private skip_clippy = 1
$(obj)/libax_cpumask.rlib: private rustc_target_flags = \
	--extern bitmaps=$(obj)/libbitmaps.rmeta
$(obj)/libax_cpumask.rlib: $(src)/vendor/upstream/ax-cpumask/src/lib.rs \
	$(obj)/libbitmaps.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libax_kernel_guard.rlib: private skip_clippy = 1
$(obj)/libax_kernel_guard.rlib: private rustc_target_flags = \
	--extern cfg_if=$(obj)/libcfg_if.rlib \
	--extern ax_crate_interface=$(obj)/libax_crate_interface.so
$(obj)/libax_kernel_guard.rlib: \
	$(src)/vendor/upstream/ax-kernel-guard/src/lib.rs \
	$(obj)/libcfg_if.rlib $(obj)/libax_crate_interface.so FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libax_kspin.rlib: private skip_clippy = 1
$(obj)/libax_kspin.rlib: private rustc_target_flags = \
	--extern cfg_if=$(obj)/libcfg_if.rlib \
	--extern ax_kernel_guard=$(obj)/libax_kernel_guard.rmeta \
	--cfg 'feature="smp"'
$(obj)/libax_kspin.rlib: $(src)/vendor/upstream/ax-kspin/src/lib.rs \
	$(obj)/libcfg_if.rlib $(obj)/libax_kernel_guard.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libax_timer_list.rlib: private skip_clippy = 1
$(obj)/libax_timer_list.rlib: $(src)/vendor/upstream/ax-timer-list/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libax_memory_addr.rlib: private skip_clippy = 1
$(obj)/libax_memory_addr.rlib: $(src)/vendor/upstream/ax-memory-addr/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libvolatile.rlib: private skip_clippy = 1
$(obj)/libvolatile.rlib: $(src)/vendor/registry/volatile/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libx86_64.rlib: private skip_clippy = 1
$(obj)/libx86_64.rlib: private rustc_target_flags = \
	--extern bit_field=$(obj)/libbit_field.rmeta \
	--extern bitflags=$(obj)/libbitflags.rmeta \
	--extern dep_const_fn=$(obj)/libconst_fn.so \
	--extern rustversion=$(obj)/librustversion.so \
	--extern volatile=$(obj)/libvolatile.rmeta \
	--cfg 'feature="instructions"'
$(obj)/libx86_64.rlib: \
	$(src)/vendor/registry/x86_64/src/lib.rs \
	$(obj)/libbit_field.rlib $(obj)/libbitflags.rlib \
	$(obj)/libconst_fn.so $(obj)/librustversion.so \
	$(obj)/libvolatile.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libraw_cpuid.rlib: private skip_clippy = 1
$(obj)/libraw_cpuid.rlib: private rustc_target_flags = \
	--extern bitflags=$(obj)/libbitflags.rmeta
$(obj)/libraw_cpuid.rlib: \
	$(src)/vendor/registry/raw-cpuid/src/lib.rs \
	$(obj)/libbitflags.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libx86.rlib: private skip_clippy = 1
$(obj)/libx86.rlib: private rustc_target_flags = \
	--extern bit_field=$(obj)/libbit_field.rmeta \
	--extern bitflags=$(obj)/libbitflags1.rmeta \
	--extern raw_cpuid=$(obj)/libraw_cpuid.rmeta
$(obj)/libx86.rlib: \
	$(src)/vendor/registry/x86/src/lib.rs \
	$(obj)/libbit_field.rlib $(obj)/libbitflags1.rlib \
	$(obj)/libraw_cpuid.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libax_errno.rlib: private skip_clippy = 1
$(obj)/libax_errno.rlib: private rustc_target_flags = \
	--extern log=$(obj)/liblog.rmeta
$(obj)/libax_errno.rlib: \
	$(src)/vendor/upstream/ax-errno/src/lib.rs \
	$(obj)/liblog.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libax_page_table_entry.rlib: private skip_clippy = 1
$(obj)/libax_page_table_entry.rlib: private rustc_target_flags = \
	--extern bitflags=$(obj)/libbitflags.rmeta \
	--extern x86_64=$(obj)/libx86_64.rmeta \
	--extern ax_memory_addr=$(obj)/libax_memory_addr.rmeta
$(obj)/libax_page_table_entry.rlib: \
	$(src)/vendor/upstream/ax-page-table-entry/src/lib.rs \
	$(obj)/libbitflags.rlib $(obj)/libx86_64.rlib \
	$(obj)/libax_memory_addr.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libax_memory_set.rlib: private skip_clippy = 1
$(obj)/libax_memory_set.rlib: private rustc_target_flags = \
	--extern ax_memory_addr=$(obj)/libax_memory_addr.rmeta
$(obj)/libax_memory_set.rlib: \
	$(src)/vendor/upstream/ax-memory-set/src/lib.rs \
	$(obj)/libax_memory_addr.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

ifeq ($(CONFIG_X86_64),y)
axvisor_pt_multiarch_arch_flags := \
	--extern x86=$(obj)/libx86.rmeta
axvisor_pt_multiarch_arch_deps := \
	$(obj)/libx86.rlib
axvisor_addrspace_arch_flags := \
	--extern bit_field=$(obj)/libbit_field.rmeta \
	--extern bitflags=$(obj)/libbitflags.rmeta \
	--extern numeric_enum_macro=$(obj)/libnumeric_enum_macro.rmeta \
	--extern x86=$(obj)/libx86.rmeta \
	--cfg 'feature="vmx"'
axvisor_addrspace_arch_deps := \
	$(obj)/libbit_field.rlib \
	$(obj)/libbitflags.rlib \
	$(obj)/libnumeric_enum_macro.rlib \
	$(obj)/libx86.rlib
else
axvisor_pt_multiarch_arch_flags := \
	--extern riscv=$(obj)/libriscv.rmeta
axvisor_pt_multiarch_arch_deps := \
	$(obj)/libriscv.rlib
axvisor_addrspace_arch_flags :=
axvisor_addrspace_arch_deps :=
endif

$(obj)/libax_page_table_multiarch.rlib: private skip_clippy = 1
$(obj)/libax_page_table_multiarch.rlib: private rustc_target_flags = \
	--extern arrayvec=$(obj)/libarrayvec.rmeta \
	--extern ax_memory_addr=$(obj)/libax_memory_addr.rmeta \
	--extern log=$(obj)/liblog.rlib \
	--extern ax_page_table_entry=$(obj)/libax_page_table_entry.rmeta \
	$(axvisor_pt_multiarch_arch_flags)
$(obj)/libax_page_table_multiarch.rlib: \
	$(src)/vendor/upstream/ax-page-table-multiarch/src/lib.rs \
	$(obj)/libarrayvec.rlib $(obj)/libax_memory_addr.rlib \
	$(obj)/libax_page_table_entry.rlib $(obj)/liblog.rlib \
	$(axvisor_pt_multiarch_arch_deps) FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libaxaddrspace.rlib: private skip_clippy = 1
$(obj)/libaxaddrspace.rlib: private rustc_target_flags = \
	--extern cfg_if=$(obj)/libcfg_if.rlib \
	--extern log=$(obj)/liblog.rlib \
	--extern ax_errno=$(obj)/libax_errno.rmeta \
	--extern ax_lazyinit=$(obj)/libax_lazyinit.rmeta \
	--extern ax_memory_addr=$(obj)/libax_memory_addr.rmeta \
	--extern ax_memory_set=$(obj)/libax_memory_set.rmeta \
	--extern ax_page_table_entry=$(obj)/libax_page_table_entry.rmeta \
	--extern ax_page_table_multiarch=$(obj)/libax_page_table_multiarch.rmeta \
	$(axvisor_addrspace_arch_flags)
$(obj)/libaxaddrspace.rlib: \
	$(src)/vendor/upstream/axaddrspace/src/lib.rs \
	$(obj)/libcfg_if.rlib $(obj)/liblog.rlib $(obj)/libax_errno.rlib \
	$(obj)/libax_lazyinit.rlib $(obj)/libax_memory_addr.rlib \
	$(obj)/libax_memory_set.rlib $(obj)/libax_page_table_entry.rlib \
	$(obj)/libax_page_table_multiarch.rlib \
	$(axvisor_addrspace_arch_deps) FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

ifeq ($(CONFIG_X86_64),y)
axvisor_percpu_arch_flags := \
	--extern x86=$(obj)/libx86.rmeta
axvisor_percpu_arch_deps := \
	$(obj)/libx86.rlib
else
axvisor_percpu_arch_flags :=
axvisor_percpu_arch_deps :=
endif

$(obj)/libax_percpu.rlib: private skip_clippy = 1
$(obj)/libax_percpu.rlib: private rustc_target_flags = \
	--extern cfg_if=$(obj)/libcfg_if.rlib \
	--extern spin=$(obj)/libspin.rmeta \
	--extern ax_percpu_macros=$(obj)/libax_percpu_macros.so \
	$(axvisor_percpu_arch_flags) \
	$(axvisor_percpu_feature_flags)
$(obj)/libax_percpu.rlib: \
	$(src)/vendor/upstream/ax-percpu/src/lib.rs \
	$(obj)/libcfg_if.rlib $(obj)/libspin.rlib \
	$(obj)/libax_percpu_macros.so \
	$(axvisor_percpu_arch_deps) FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libaxvisor_api.rlib: private skip_clippy = 1
$(obj)/libaxvisor_api.rlib: private rustc_target_flags = \
	--cfg 'feature="shell"' \
	--extern axvisor_api_proc=$(obj)/libaxvisor_api_proc.so \
	--extern ax_errno=$(obj)/libax_errno.rmeta \
	--extern axaddrspace=$(obj)/libaxaddrspace.rmeta \
	--extern ax_crate_interface=$(obj)/libax_crate_interface.so \
	--extern ax_memory_addr=$(obj)/libax_memory_addr.rmeta \
	--extern ax_cpumask=$(obj)/libax_cpumask.rmeta
$(obj)/libaxvisor_api.rlib: \
	$(src)/vendor/upstream/axvisor_api/src/lib.rs \
	$(obj)/libaxvisor_api_proc.so $(obj)/libax_errno.rlib \
	$(obj)/libaxaddrspace.rlib $(obj)/libax_crate_interface.so \
	$(obj)/libax_memory_addr.rlib $(obj)/libax_cpumask.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libaxvcpu.rlib: private skip_clippy = 1
$(obj)/libaxvcpu.rlib: private rustc_target_flags = \
	--extern ax_errno=$(obj)/libax_errno.rmeta \
	--extern ax_memory_addr=$(obj)/libax_memory_addr.rmeta \
	--extern ax_percpu=$(obj)/libax_percpu.rmeta \
	--extern axaddrspace=$(obj)/libaxaddrspace.rmeta \
	--extern axvisor_api=$(obj)/libaxvisor_api.rmeta \
	--extern ax_kspin=$(obj)/libax_kspin.rmeta
$(obj)/libaxvcpu.rlib: \
	$(src)/vendor/upstream/axvcpu/src/lib.rs \
	$(obj)/libax_errno.rlib $(obj)/libax_memory_addr.rlib \
	$(obj)/libax_percpu.rlib $(obj)/libaxaddrspace.rlib \
	$(obj)/libaxvisor_api.rlib $(obj)/libax_kspin.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libaxvmconfig.rlib: private skip_clippy = 1
$(obj)/libaxvmconfig.rlib: private rustc_target_flags = \
	--extern ax_errno=$(obj)/libax_errno.rmeta \
	--extern log=$(obj)/liblog.rmeta
$(obj)/libaxvmconfig.rlib: \
	$(src)/vendor/upstream/axvmconfig/src/lib.rs \
	$(obj)/libax_errno.rlib $(obj)/liblog.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libaxdevice_base.rlib: private skip_clippy = 1
$(obj)/libaxdevice_base.rlib: private rustc_target_flags = \
	--extern axaddrspace=$(obj)/libaxaddrspace.rmeta \
	--extern ax_errno=$(obj)/libax_errno.rmeta \
	--extern axvmconfig=$(obj)/libaxvmconfig.rmeta
$(obj)/libaxdevice_base.rlib: \
	$(src)/vendor/upstream/axdevice_base/src/lib.rs \
	$(obj)/libaxaddrspace.rlib $(obj)/libax_errno.rlib \
	$(obj)/libaxvmconfig.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libx86_vlapic.rlib: private skip_clippy = 1
$(obj)/libx86_vlapic.rlib: private rustc_target_flags = \
	--extern log=$(obj)/liblog.rmeta \
	--extern paste=$(obj)/libpaste.so \
	--extern tock_registers=$(obj)/libtock_registers.rmeta \
	--extern bit=$(obj)/libbit.rmeta \
	--extern ax_errno=$(obj)/libax_errno.rmeta \
	--extern ax_memory_addr=$(obj)/libax_memory_addr.rmeta \
	--extern axaddrspace=$(obj)/libaxaddrspace.rmeta \
	--extern axdevice_base=$(obj)/libaxdevice_base.rmeta \
	--extern axvisor_api=$(obj)/libaxvisor_api.rmeta \
	--extern ax_kspin=$(obj)/libax_kspin.rmeta
$(obj)/libx86_vlapic.rlib: \
	$(src)/vendor/upstream/x86_vlapic/src/lib.rs \
	$(obj)/liblog.rlib $(obj)/libpaste.so \
	$(obj)/libtock_registers.rlib $(obj)/libbit.rlib \
	$(obj)/libax_errno.rlib $(obj)/libax_memory_addr.rlib \
	$(obj)/libaxaddrspace.rlib $(obj)/libaxdevice_base.rlib \
	$(obj)/libaxvisor_api.rlib $(obj)/libax_kspin.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libx86_vcpu.rlib: private skip_clippy = 1
$(obj)/libx86_vcpu.rlib: private axvisor_target_rust_edition = --edition=2024
$(obj)/libx86_vcpu.rlib: private rustc_target_flags = \
	--extern log=$(obj)/liblog.rmeta \
	--extern cfg_if=$(obj)/libcfg_if.rmeta \
	--extern bitflags=$(obj)/libbitflags.rmeta \
	--extern bit_field=$(obj)/libbit_field.rmeta \
	--extern paste=$(obj)/libpaste.so \
	--extern x86=$(obj)/libx86.rmeta \
	--extern x86_64=$(obj)/libx86_64.rmeta \
	--extern raw_cpuid=$(obj)/libraw_cpuid.rmeta \
	--extern numeric_enum_macro=$(obj)/libnumeric_enum_macro.rmeta \
	--extern tock_registers=$(obj)/libtock_registers.rmeta \
	--extern ax_errno=$(obj)/libax_errno.rmeta \
	--extern ax_page_table_entry=$(obj)/libax_page_table_entry.rmeta \
	--extern ax_memory_addr=$(obj)/libax_memory_addr.rmeta \
	--extern ax_crate_interface=$(obj)/libax_crate_interface.so \
	--extern axaddrspace=$(obj)/libaxaddrspace.rmeta \
	--extern axvcpu=$(obj)/libaxvcpu.rmeta \
	--extern x86_vlapic=$(obj)/libx86_vlapic.rmeta \
	--extern axdevice_base=$(obj)/libaxdevice_base.rmeta \
	--extern axvisor_api=$(obj)/libaxvisor_api.rmeta \
	--extern ax_kspin=$(obj)/libax_kspin.rmeta \
	--cfg 'feature="vmx"'
$(obj)/libx86_vcpu.rlib: \
	$(src)/vendor/upstream/x86_vcpu/src/lib.rs \
	$(obj)/liblog.rlib $(obj)/libcfg_if.rlib \
	$(obj)/libbitflags.rlib $(obj)/libbit_field.rlib \
	$(obj)/libpaste.so $(obj)/libx86.rlib $(obj)/libx86_64.rlib \
	$(obj)/libraw_cpuid.rlib $(obj)/libnumeric_enum_macro.rlib \
	$(obj)/libtock_registers.rlib $(obj)/libax_errno.rlib \
	$(obj)/libax_page_table_entry.rlib $(obj)/libax_memory_addr.rlib \
	$(obj)/libax_crate_interface.so $(obj)/libaxaddrspace.rlib \
	$(obj)/libaxvcpu.rlib $(obj)/libx86_vlapic.rlib \
	$(obj)/libaxdevice_base.rlib $(obj)/libaxvisor_api.rlib \
	$(obj)/libax_kspin.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libriscv_h.rlib: private skip_clippy = 1
$(obj)/libriscv_h.rlib: private rustc_target_flags = \
	--extern riscv=$(obj)/libriscv.rmeta \
	--extern bare_metal=$(obj)/libbare_metal.rmeta \
	--extern bitflags=$(obj)/libbitflags.rmeta \
	--extern bit_field=$(obj)/libbit_field.rmeta \
	--extern log=$(obj)/liblog.rmeta
$(obj)/libriscv_h.rlib: \
	$(src)/vendor/upstream/riscv-h/src/lib.rs \
	$(obj)/libriscv.rlib $(obj)/libbare_metal.rlib \
	$(obj)/libbitflags.rlib $(obj)/libbit_field.rlib \
	$(obj)/liblog.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libriscv_vplic.rlib: private skip_clippy = 1
$(obj)/libriscv_vplic.rlib: private rustc_target_flags = \
	--extern ax_kspin \
	--extern axaddrspace \
	--extern axdevice_base \
	--extern axvisor_api \
	--extern ax_errno \
	--extern bitmaps \
	--extern log \
	--extern riscv_h
$(obj)/libriscv_vplic.rlib: \
	$(src)/vendor/upstream/riscv_vplic/src/lib.rs \
	$(obj)/libax_kspin.rlib $(obj)/libaxaddrspace.rlib \
	$(obj)/libaxdevice_base.rlib $(obj)/libaxvisor_api.rlib \
	$(obj)/libax_errno.rlib $(obj)/libbitmaps.rlib \
	$(obj)/liblog.rlib $(obj)/libriscv_h.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

ifeq ($(CONFIG_X86_64),y)
axvisor_device_arch_flags := \
	--extern x86_vlapic=$(obj)/libx86_vlapic.rmeta
axvisor_device_arch_deps := \
	$(obj)/libx86_vlapic.rlib
axvisor_vm_arch_flags := \
	--extern x86_vcpu=$(obj)/libx86_vcpu.rmeta \
	--cfg 'feature="vmx"'
axvisor_vm_arch_deps := \
	$(obj)/libx86_vcpu.rlib
else
axvisor_device_arch_flags := \
	--extern riscv_vplic
axvisor_device_arch_deps := \
	$(obj)/libriscv_vplic.rlib
axvisor_vm_arch_flags := \
	--extern riscv_vcpu
axvisor_vm_arch_deps := \
	$(obj)/libriscv_vcpu.rlib
endif

$(obj)/libaxdevice.rlib: private skip_clippy = 1
$(obj)/libaxdevice.rlib: private rustc_target_flags = \
	--extern log \
	--extern cfg_if=$(obj)/libcfg_if.rlib \
	--extern ax_kspin \
	--extern ax_errno \
	--extern ax_memory_addr \
	--extern axvmconfig \
	--extern axaddrspace \
	--extern axdevice_base \
	$(axvisor_device_arch_flags)
$(obj)/libaxdevice.rlib: \
	$(src)/vendor/upstream/axdevice/src/lib.rs \
	$(obj)/liblog.rlib $(obj)/libcfg_if.rlib $(obj)/libax_kspin.rlib \
	$(obj)/libax_errno.rlib $(obj)/libax_memory_addr.rlib \
	$(obj)/libaxvmconfig.rlib $(obj)/libaxaddrspace.rlib \
	$(obj)/libaxdevice_base.rlib $(axvisor_device_arch_deps) FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libriscv_vcpu.rlib: private skip_clippy = 1
$(obj)/libriscv_vcpu.rlib: private rustc_target_flags = \
	--extern log \
	--extern cfg_if=$(obj)/libcfg_if.rlib \
	--extern bitflags \
	--extern bit_field \
	--extern ax_crate_interface \
	--extern riscv \
	--extern riscv_h \
	--extern riscv_decode \
	--extern rustsbi \
	--extern sbi_rt \
	--extern sbi_spec \
	--extern tock_registers \
	--extern memoffset \
	--extern ax_errno \
	--extern ax_page_table_entry \
	--extern ax_memory_addr \
	--extern axaddrspace \
	--extern axvcpu \
	--extern axvisor_api \
	--cfg='feature="sstc"'
$(obj)/libriscv_vcpu.rlib: \
	$(src)/vendor/upstream/riscv_vcpu/src/lib.rs \
	$(obj)/liblog.rlib $(obj)/libcfg_if.rlib $(obj)/libbitflags.rlib \
	$(obj)/libbit_field.rlib $(obj)/libax_crate_interface.so \
	$(obj)/libriscv.rlib $(obj)/libriscv_h.rlib \
	$(obj)/libriscv_decode.rlib $(obj)/librustsbi.rlib \
	$(obj)/libsbi_rt.rlib $(obj)/libsbi_spec.rlib \
	$(obj)/libtock_registers.rlib $(obj)/libmemoffset.rlib \
	$(obj)/libax_errno.rlib $(obj)/libax_page_table_entry.rlib \
	$(obj)/libax_memory_addr.rlib $(obj)/libaxaddrspace.rlib \
	$(obj)/libaxvcpu.rlib $(obj)/libaxvisor_api.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libaxvm.rlib: private skip_clippy = 1
$(obj)/libaxvm.rlib: private rustc_target_flags = \
	--extern log \
	--extern cfg_if=$(obj)/libcfg_if.rlib \
	--extern spin \
	--extern ax_errno \
	--extern ax_cpumask \
	--extern ax_kspin \
	--extern ax_memory_addr \
	--extern ax_page_table_entry \
	--extern ax_page_table_multiarch \
	--extern ax_percpu \
	--extern axvcpu \
	--extern axaddrspace \
	--extern axvisor_api \
	--extern axdevice \
	--extern axdevice_base \
	--extern axvmconfig \
	$(axvisor_vm_arch_flags)
$(obj)/libaxvm.rlib: \
	$(src)/vendor/upstream/axvm/src/lib.rs \
	$(obj)/liblog.rlib $(obj)/libcfg_if.rlib $(obj)/libspin.rlib \
	$(obj)/libax_errno.rlib $(obj)/libax_cpumask.rlib \
	$(obj)/libax_kspin.rlib $(obj)/libax_memory_addr.rlib \
	$(obj)/libax_page_table_entry.rlib $(obj)/libax_page_table_multiarch.rlib \
	$(obj)/libax_percpu.rlib $(obj)/libaxvcpu.rlib \
	$(obj)/libaxaddrspace.rlib $(obj)/libaxvisor_api.rlib \
	$(obj)/libaxdevice.rlib $(obj)/libaxdevice_base.rlib \
	$(obj)/libaxvmconfig.rlib $(axvisor_vm_arch_deps) FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libaxhvc.rlib: private skip_clippy = 1
$(obj)/libaxhvc.rlib: private rustc_target_flags = \
	--extern ax_errno
$(obj)/libaxhvc.rlib: \
	$(src)/vendor/upstream/axhvc/src/lib.rs \
	$(obj)/libax_errno.rlib FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libfdt_parser.rlib: private skip_clippy = 1
$(obj)/libfdt_parser.rlib: $(src)/vendor/registry/fdt-parser/src/lib.rs FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

ifeq ($(CONFIG_X86_64),y)
axvisor_core_arch_flags := \
	--extern x86_vlapic=$(obj)/libx86_vlapic.rmeta \
	--extern x86_vcpu=$(obj)/libx86_vcpu.rmeta \
	--cfg 'feature="shell"' \
	--cfg 'feature="vmx"'
axvisor_core_arch_deps := \
	$(obj)/libx86_vlapic.rlib \
	$(obj)/libx86_vcpu.rlib
else
axvisor_core_arch_flags := \
	--extern riscv_vplic \
	--extern riscv_vcpu \
	--cfg 'feature="shell"' \
	--cfg 'feature="sstc"'
axvisor_core_arch_deps := \
	$(obj)/libriscv_vplic.rlib \
	$(obj)/libriscv_vcpu.rlib
endif

$(obj)/libaxvisor_core.rlib: private skip_clippy = 1
$(obj)/libaxvisor_core.rlib: private rustc_target_flags = \
	--extern spin \
	--extern log \
	--extern hashbrown \
	--extern ax_cpumask \
	--extern ax_kernel_guard \
	--extern ax_kspin \
	--extern ax_lazyinit \
	--extern ax_percpu \
	--extern ax_timer_list \
	--extern ax_errno \
	--extern ax_memory_addr \
	--extern ax_page_table_multiarch \
	--extern axaddrspace \
	--extern axhvc \
	--extern axvcpu \
	--extern axvm \
	--extern axvisor_api \
	--extern fdt_parser \
	$(axvisor_core_arch_flags)
$(obj)/libaxvisor_core.rlib: \
	$(src)/vendor/upstream/axvisor_core/src/lib.rs \
	$(axvisor_static_vm_configs_rs) \
	$(obj)/libspin.rlib $(obj)/liblog.rlib $(obj)/libhashbrown.rlib \
	$(obj)/libax_cpumask.rlib $(obj)/libax_kernel_guard.rlib \
	$(obj)/libax_kspin.rlib $(obj)/libax_lazyinit.rlib \
	$(obj)/libax_percpu.rlib $(obj)/libax_timer_list.rlib \
	$(obj)/libax_errno.rlib $(obj)/libax_memory_addr.rlib \
	$(obj)/libax_page_table_multiarch.rlib $(obj)/libaxaddrspace.rlib \
	$(obj)/libaxhvc.rlib $(obj)/libaxvcpu.rlib $(obj)/libaxvm.rlib \
	$(obj)/libaxvisor_api.rlib $(obj)/libfdt_parser.rlib \
	$(axvisor_core_arch_deps) FORCE
	+$(call if_changed_dep,axvisor_rustc_target_library)

$(obj)/libaxvisor_linux_bridge.a: private skip_clippy = 1
$(obj)/libaxvisor_linux_bridge.a: private rustc_target_flags = \
	--extern axvisor_core \
	--extern spin \
	--extern log \
	--extern hashbrown \
	--extern ax_cpumask \
	--extern ax_kernel_guard \
	--extern ax_kspin \
	--extern ax_lazyinit \
	--extern ax_percpu \
	--extern ax_timer_list \
	--extern ax_errno \
	--extern ax_memory_addr \
	--extern ax_page_table_multiarch \
	--extern axaddrspace \
	--extern axhvc \
	--extern axvcpu \
	--extern axvm \
	--extern axvisor_api \
	--extern fdt_parser \
	$(axvisor_core_arch_flags)
$(obj)/libaxvisor_linux_bridge.a: \
	$(src)/vendor/upstream/axvisor_linux_bridge/src/lib.rs \
	$(obj)/libaxvisor_core.rlib \
	$(obj)/libspin.rlib $(obj)/liblog.rlib $(obj)/libhashbrown.rlib \
	$(obj)/libax_cpumask.rlib $(obj)/libax_kernel_guard.rlib \
	$(obj)/libax_kspin.rlib $(obj)/libax_lazyinit.rlib \
	$(obj)/libax_percpu.rlib $(obj)/libax_timer_list.rlib \
	$(obj)/libax_errno.rlib $(obj)/libax_memory_addr.rlib \
	$(obj)/libax_page_table_multiarch.rlib $(obj)/libaxaddrspace.rlib \
	$(obj)/libaxhvc.rlib $(obj)/libaxvcpu.rlib $(obj)/libaxvm.rlib \
	$(obj)/libaxvisor_api.rlib $(obj)/libfdt_parser.rlib \
	$(axvisor_core_arch_deps) FORCE
	+$(call if_changed_dep,axvisor_rustc_target_object)

$(obj)/axvisor_linux_bridge.o: $(obj)/libaxvisor_linux_bridge.a FORCE
	+$(call if_changed,axvisor_partial_link_archive)

endif # CONFIG_AXVISOR_ADAPTER
