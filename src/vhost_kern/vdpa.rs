// Copyright (C) 2021 Red Hat, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 or BSD-3-Clause

//! Kernel-based vhost-vdpa backend.

use std::fs::{File, OpenOptions};
use std::os::raw::c_int;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};

use vm_memory::GuestAddressSpace;
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::ioctl::{ioctl_with_mut_ref, ioctl_with_ptr, ioctl_with_ref};

use std::alloc::{alloc, dealloc, Layout};
use std::mem;

use super::vhost_binding::*;
use super::{ioctl_result, Error, Result, VhostKernBackend};
use crate::vdpa::*;

/// Handle for running VHOST_VDPA ioctls.
pub struct VhostKernVdpa<AS: GuestAddressSpace> {
    fd: File,
    mem: AS,
}

impl<AS: GuestAddressSpace> VhostKernVdpa<AS> {
    /// Open a handle to a new VHOST-VDPA instance.
    pub fn new(path: &str, mem: AS) -> Result<Self> {
        Ok(VhostKernVdpa {
            fd: OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(path)
                .map_err(Error::VhostOpen)?,
            mem,
        })
    }
}

impl<AS: GuestAddressSpace> VhostVdpa for VhostKernVdpa<AS> {
    fn get_device_id(&self) -> Result<u32> {
        let mut device_id: u32 = 0;
        let ret = unsafe { ioctl_with_mut_ref(self, VHOST_VDPA_GET_DEVICE_ID(), &mut device_id) };
        ioctl_result(ret, device_id)
    }

    fn get_status(&self) -> Result<u8> {
        let mut status: u8 = 0;
        let ret = unsafe { ioctl_with_mut_ref(self, VHOST_VDPA_GET_STATUS(), &mut status) };
        ioctl_result(ret, status)
    }

    fn set_status(&self, status: u8) -> Result<()> {
        let ret = unsafe { ioctl_with_ref(self, VHOST_VDPA_SET_STATUS(), &status) };
        ioctl_result(ret, ())
    }

    fn get_config(&self, offset: u32, buffer: &mut [u8]) -> Result<()> {
        let buffer_len = buffer.len();
        let layout =
            Layout::from_size_align(mem::size_of::<vhost_vdpa_config>() + buffer_len, 1).unwrap();
        let ret: c_int;

        unsafe {
            let ptr = alloc(layout);
            let config = ptr as *mut vhost_vdpa_config;
            (*config).off = offset;
            (*config).len = buffer_len as u32;

            ret = ioctl_with_ptr(self, VHOST_VDPA_GET_CONFIG(), ptr);

            buffer.copy_from_slice((*config).buf.as_slice(buffer_len));

            dealloc(ptr, layout);
        };

        ioctl_result(ret, ())
    }

    fn set_config(&self, offset: u32, buffer: &[u8]) -> Result<()> {
        let buffer_len = buffer.len();
        let layout =
            Layout::from_size_align(mem::size_of::<vhost_vdpa_config>() + buffer_len, 1).unwrap();
        let ret: c_int;

        unsafe {
            let ptr = alloc(layout);
            let config = ptr as *mut vhost_vdpa_config;
            (*config).off = offset;
            (*config).len = buffer_len as u32;

            (*config)
                .buf
                .as_mut_slice(buffer_len)
                .copy_from_slice(&buffer);

            ret = ioctl_with_ptr(self, VHOST_VDPA_SET_CONFIG(), ptr);

            dealloc(ptr, layout);
        };

        ioctl_result(ret, ())
    }

    fn set_vring_enable(&self, queue_index: usize, enabled: bool) -> Result<()> {
        let vring_state = vhost_vring_state {
            index: queue_index as u32,
            num: enabled as u32,
        };

        let ret = unsafe { ioctl_with_ref(self, VHOST_VDPA_SET_VRING_ENABLE(), &vring_state) };
        ioctl_result(ret, ())
    }

    fn get_vring_num(&self) -> Result<u16> {
        let mut vring_num: u16 = 0;
        let ret = unsafe { ioctl_with_mut_ref(self, VHOST_VDPA_GET_VRING_NUM(), &mut vring_num) };
        ioctl_result(ret, vring_num)
    }

    fn set_config_call(&self, fd: &EventFd) -> Result<()> {
        let event_fd: ::std::os::raw::c_int = fd.as_raw_fd();
        let ret = unsafe { ioctl_with_ref(self, VHOST_VDPA_SET_CONFIG_CALL(), &event_fd) };
        ioctl_result(ret, ())
    }

    fn get_iova_range(&self) -> Result<VhostVdpaIovaRange> {
        let mut low_iova_range = vhost_vdpa_iova_range { first: 0, last: 0 };

        let ret =
            unsafe { ioctl_with_mut_ref(self, VHOST_VDPA_GET_VRING_NUM(), &mut low_iova_range) };

        let iova_range = VhostVdpaIovaRange {
            first: low_iova_range.first,
            last: low_iova_range.last,
        };

        ioctl_result(ret, iova_range)
    }
}

impl<AS: GuestAddressSpace> VhostKernBackend for VhostKernVdpa<AS> {
    type AS = AS;

    fn mem(&self) -> &Self::AS {
        &self.mem
    }
}

impl<AS: GuestAddressSpace> AsRawFd for VhostKernVdpa<AS> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    const VHOST_VDPA_PATH: &str = "/dev/vhost-vdpa-0";

    use vm_memory::{GuestAddress, GuestMemory, GuestMemoryMmap};
    use vmm_sys_util::eventfd::EventFd;

    use super::*;
    use crate::{
        VhostBackend, VhostUserDirtyLogRegion, VhostUserMemoryRegionInfo, VringConfigData,
    };
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_vdpa_kern_new_device() {
        let m = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10_0000)]).unwrap();
        let vdpa = VhostKernVdpa::new(VHOST_VDPA_PATH, &m).unwrap();

        assert!(vdpa.as_raw_fd() >= 0);
        assert!(vdpa.mem().find_region(GuestAddress(0x100)).is_some());
        assert!(vdpa.mem().find_region(GuestAddress(0x10_0000)).is_none());
    }

    #[test]
    #[serial]
    fn test_vdpa_kern_is_valid() {
        let m = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10_0000)]).unwrap();
        let vdpa = VhostKernVdpa::new(VHOST_VDPA_PATH, &m).unwrap();

        let mut config = VringConfigData {
            queue_max_size: 32,
            queue_size: 32,
            flags: 0,
            desc_table_addr: 0x1000,
            used_ring_addr: 0x2000,
            avail_ring_addr: 0x3000,
            log_addr: None,
        };
        assert_eq!(vdpa.is_valid(&config), true);

        config.queue_size = 0;
        assert_eq!(vdpa.is_valid(&config), false);
        config.queue_size = 31;
        assert_eq!(vdpa.is_valid(&config), false);
        config.queue_size = 33;
        assert_eq!(vdpa.is_valid(&config), false);
    }

    #[test]
    #[serial]
    fn test_vdpa_kern_ioctls() {
        let m = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10_0000)]).unwrap();
        let vdpa = VhostKernVdpa::new(VHOST_VDPA_PATH, &m).unwrap();

        let features = vdpa.get_features().unwrap();
        // VIRTIO_F_VERSION_1 (bit 32) should be set
        assert_ne!(features & (1 << 32), 0);
        vdpa.set_features(features).unwrap();

        vdpa.set_owner().unwrap();

        vdpa.set_mem_table(&[]).unwrap_err();

        let region = VhostUserMemoryRegionInfo {
            guest_phys_addr: 0x0,
            memory_size: 0x10_0000,
            userspace_addr: m.get_host_address(GuestAddress(0x0)).unwrap() as u64,
            mmap_offset: 0,
            mmap_handle: -1,
        };
        vdpa.set_mem_table(&[region]).unwrap();

        assert!(vdpa.get_device_id().unwrap() > 0);

        assert_eq!(vdpa.get_status().unwrap(), 0x0);
        vdpa.set_status(0x1).unwrap();
        assert_eq!(vdpa.get_status().unwrap(), 0x1);

        let mut vec = vec![0u8; 8];
        vdpa.get_config(0, &mut vec).unwrap();
        vdpa.set_config(0, &vec).unwrap();

        let eventfd = EventFd::new(0).unwrap();

        // set_log_base() and set_log_fd() are not supported by vhost-vdpa
        vdpa.set_log_base(
            0x4000,
            Some(VhostUserDirtyLogRegion {
                mmap_size: 0x1000,
                mmap_offset: 0x10,
                mmap_handle: 1,
            }),
        )
        .unwrap_err();
        vdpa.set_log_base(0x4000, None).unwrap_err();
        vdpa.set_log_fd(eventfd.as_raw_fd()).unwrap_err();

        let max_queues = vdpa.get_vring_num().unwrap();
        vdpa.set_vring_num(0, max_queues + 1).unwrap_err();

        vdpa.set_vring_num(0, 32).unwrap();

        let config = VringConfigData {
            queue_max_size: 32,
            queue_size: 32,
            flags: 0,
            desc_table_addr: 0x1000,
            used_ring_addr: 0x2000,
            avail_ring_addr: 0x3000,
            log_addr: None,
        };
        vdpa.set_vring_addr(0, &config).unwrap();
        vdpa.set_vring_base(0, 1).unwrap();
        vdpa.set_vring_call(0, &eventfd).unwrap();
        vdpa.set_vring_kick(0, &eventfd).unwrap();
        vdpa.set_vring_err(0, &eventfd).unwrap();

        vdpa.set_config_call(&eventfd).unwrap();

        assert_eq!(vdpa.get_vring_base(0).unwrap(), 1);

        vdpa.set_vring_enable(0, true).unwrap();
        vdpa.set_vring_enable(0, false).unwrap();
    }
}