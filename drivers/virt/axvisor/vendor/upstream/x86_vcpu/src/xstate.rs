use core::arch::asm;

use raw_cpuid::CpuId;
use x86::controlregs::{Xcr0, xcr0 as xcr0_read, xcr0_write};
use x86_64::registers::control::{Cr4, Cr4Flags};

use crate::{X86KvmFxsave, msr::Msr};

/// Extended processor state switched between host and guest.
#[derive(Debug)]
pub struct XState {
    pub guest_xcr0: u64,

    host_xcr0: u64,
    host_xss: u64,
    guest_xss: u64,
    guest_fxsave: X86KvmFxsave,
    guest_fxsave_valid: bool,
    xsave_available: bool,
    xsaves_available: bool,
}

impl XState {
    pub fn new() -> Self {
        let xsave_available = xsave_available();
        let xsaves_supported = xsave_available && xsaves_available();
        let xcr0 = if xsave_available {
            unsafe { xcr0_read().bits() }
        } else {
            0
        };
        let xss = if xsaves_supported {
            Msr::IA32_XSS.read()
        } else {
            0
        };

        Self {
            host_xcr0: xcr0,
            guest_xcr0: xcr0,
            host_xss: xss,
            guest_xss: xss,
            guest_fxsave: X86KvmFxsave::zeroed(),
            guest_fxsave_valid: false,
            xsave_available,
            xsaves_available: xsaves_supported,
        }
    }

    pub fn set_guest_fxsave(&mut self, fxsave: &X86KvmFxsave) {
        self.guest_fxsave = *fxsave;
        self.guest_fxsave_valid = true;
    }

    pub fn switch_to_guest(&mut self) {
        unsafe {
            if self.xsave_available {
                self.host_xcr0 = xcr0_read().bits();
                xcr0_write(Xcr0::from_bits_unchecked(self.guest_xcr0));

                if self.xsaves_available {
                    self.host_xss = Msr::IA32_XSS.read();
                    Msr::IA32_XSS.write(self.guest_xss);
                }
            }

            if self.guest_fxsave_valid {
                fxrstor64(&self.guest_fxsave);
            }
        }
    }

    pub fn switch_xcrs_to_guest(&mut self) {
        unsafe {
            if self.xsave_available {
                self.host_xcr0 = xcr0_read().bits();
                xcr0_write(Xcr0::from_bits_unchecked(self.guest_xcr0));

                if self.xsaves_available {
                    self.host_xss = Msr::IA32_XSS.read();
                    Msr::IA32_XSS.write(self.guest_xss);
                }
            }
        }
    }

    pub fn switch_xcrs_to_host(&mut self) {
        unsafe {
            if self.xsave_available {
                self.guest_xcr0 = xcr0_read().bits();
                xcr0_write(Xcr0::from_bits_unchecked(self.host_xcr0));

                if self.xsaves_available {
                    self.guest_xss = Msr::IA32_XSS.read();
                    Msr::IA32_XSS.write(self.host_xss);
                }
            }
        }
    }

    pub fn switch_to_host(&mut self) {
        unsafe {
            if self.guest_fxsave_valid {
                fxsave64(&mut self.guest_fxsave);
            }

            if self.xsave_available {
                self.guest_xcr0 = xcr0_read().bits();
                xcr0_write(Xcr0::from_bits_unchecked(self.host_xcr0));

                if self.xsaves_available {
                    self.guest_xss = Msr::IA32_XSS.read();
                    Msr::IA32_XSS.write(self.host_xss);
                }
            }
        }
    }
}

unsafe fn fxsave64(area: &mut X86KvmFxsave) {
    unsafe {
        asm!(
            "fxsave64 [{ptr}]",
            ptr = in(reg) area.bytes.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    }
}

unsafe fn fxrstor64(area: &X86KvmFxsave) {
    unsafe {
        asm!(
            "fxrstor64 [{ptr}]",
            ptr = in(reg) area.bytes.as_ptr(),
            options(nostack, preserves_flags),
        );
    }
}

pub fn xsave_available() -> bool {
    CpuId::new()
        .get_feature_info()
        .map(|features| features.has_xsave())
        .unwrap_or(false)
}

pub fn xsaves_available() -> bool {
    CpuId::new()
        .get_extended_state_info()
        .map(|features| features.has_xsaves_xrstors())
        .unwrap_or(false)
}

pub fn enable_xsave() {
    if xsave_available() {
        unsafe {
            Cr4::write(Cr4::read() | Cr4Flags::OSXSAVE);
        }
    }
}
