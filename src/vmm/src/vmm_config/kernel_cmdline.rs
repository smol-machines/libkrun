// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter, Result};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 \
                                          rootfstype=virtiofs rw quiet no-kvmapf cpuidle_haltpoll.force=1";
#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
pub const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 \
                                          rootfstype=virtiofs rw quiet no-kvmapf";
#[cfg(target_os = "macos")]
pub const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 \
                                          rootfstype=virtiofs rw quiet no-kvmapf";
#[cfg(target_os = "windows")]
pub const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 no_timer_check \
     rootfstype=virtiofs rw quiet no-kvmapf";

/// Strongly typed data structure used to configure the boot source of the
/// microvm.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct KernelCmdlineConfig {
    pub prolog: Option<String>,
    pub krun_env: Option<String>,
    pub epilog: Option<String>,
}

/// Errors associated with actions on `KernelCmdlineConfig`.
#[derive(Debug)]
pub enum KernelCmdlineConfigError {
    /// The kernel command line is invalid.
    InvalidKernelCommandLine(String),
}

impl Display for KernelCmdlineConfigError {
    fn fmt(&self, f: &mut Formatter) -> Result {
        use self::KernelCmdlineConfigError::*;
        match *self {
            InvalidKernelCommandLine(ref e) => {
                write!(f, "The kernel command line is invalid: {}", e.as_str())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_KERNEL_CMDLINE;

    #[test]
    fn guest_haltpoll_is_enabled_only_for_linux_x86_64() {
        let polling_arguments: Vec<_> = DEFAULT_KERNEL_CMDLINE
            .split_whitespace()
            .filter(|argument| argument.starts_with("cpuidle_haltpoll."))
            .collect();
        let expected: &[&str] = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            &["cpuidle_haltpoll.force=1"]
        } else {
            &[]
        };
        assert_eq!(polling_arguments, expected);
        assert!(!DEFAULT_KERNEL_CMDLINE.contains("halt_poll_ns"));
    }
}
