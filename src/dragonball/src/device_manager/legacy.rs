// Copyright (C) 2022 Alibaba Cloud. All rights reserved.
// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Device Manager for Legacy Devices.

use std::io;
use std::sync::{Arc, Mutex};

use dbs_device::device_manager::Error as IoManagerError;
use dbs_legacy_devices::SerialDevice;
#[cfg(target_arch = "aarch64")]
use dbs_legacy_devices::RTCDevice;
use vmm_sys_util::eventfd::EventFd;

// The I8042 Data Port (IO Port 0x60) is used for reading data that was received from a I8042 device or from the I8042 controller itself and writing data to a I8042 device or to the I8042 controller itself.
const I8042_DATA_PORT: u16 = 0x60;

/// Errors generated by legacy device manager.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Cannot add legacy device to Bus.
    #[error("bus failure while managing legacy device")]
    BusError(#[source] IoManagerError),

    /// Cannot create EventFd.
    #[error("failure while reading EventFd file descriptor")]
    EventFd(#[source] io::Error),

    /// Failed to register/deregister interrupt.
    #[error("failure while managing interrupt for legacy device")]
    IrqManager(#[source] vmm_sys_util::errno::Error),
}

/// The `LegacyDeviceManager` is a wrapper that is used for registering legacy devices
/// on an I/O Bus.
///
/// It currently manages the uart and i8042 devices. The `LegacyDeviceManger` should be initialized
/// only by using the constructor.
pub struct LegacyDeviceManager {
    #[cfg(target_arch = "x86_64")]
    i8042_reset_eventfd: EventFd,
    #[cfg(target_arch = "aarch64")]
    pub(crate) _rtc_device: Arc<Mutex<RTCDevice>>,
    #[cfg(target_arch = "aarch64")]
    _rtc_eventfd: EventFd,
    pub(crate) com1_device: Arc<Mutex<SerialDevice>>,
    _com1_eventfd: EventFd,
    pub(crate) com2_device: Arc<Mutex<SerialDevice>>,
    _com2_eventfd: EventFd,
}

impl LegacyDeviceManager {
    /// Get the serial device for com1.
    pub fn get_com1_serial(&self) -> Arc<Mutex<SerialDevice>> {
        self.com1_device.clone()
    }

    /// Get the serial device for com2
    pub fn get_com2_serial(&self) -> Arc<Mutex<SerialDevice>> {
        self.com2_device.clone()
    }
}

#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64 {
    use super::*;
    use dbs_device::device_manager::IoManager;
    use dbs_device::resources::Resource;
    use dbs_legacy_devices::{EventFdTrigger, I8042Device, I8042DeviceMetrics};
    use kvm_ioctls::VmFd;

    pub(crate) const COM1_IRQ: u32 = 4;
    pub(crate) const COM1_PORT1: u16 = 0x3f8;
    pub(crate) const COM2_IRQ: u32 = 3;
    pub(crate) const COM2_PORT1: u16 = 0x2f8;

    type Result<T> = ::std::result::Result<T, Error>;

    impl LegacyDeviceManager {
        /// Create a LegacyDeviceManager instance handling legacy devices (uart, i8042).
        pub fn create_manager(bus: &mut IoManager, vm_fd: Option<Arc<VmFd>>) -> Result<Self> {
            let (com1_device, com1_eventfd) =
                Self::create_com_device(bus, vm_fd.as_ref(), COM1_IRQ, COM1_PORT1)?;
            let (com2_device, com2_eventfd) =
                Self::create_com_device(bus, vm_fd.as_ref(), COM2_IRQ, COM2_PORT1)?;

            let exit_evt = EventFd::new(libc::EFD_NONBLOCK).map_err(Error::EventFd)?;
            let i8042_device = Arc::new(Mutex::new(I8042Device::new(
                EventFdTrigger::new(exit_evt.try_clone().map_err(Error::EventFd)?),
                Arc::new(I8042DeviceMetrics::default()),
            )));
            let resources = [Resource::PioAddressRange {
                // 0x60 and 0x64 are the io ports that i8042 devices used.
                // We register pio address range from 0x60 - 0x64 with base I8042_DATA_PORT for i8042 to use.
                base: I8042_DATA_PORT,
                size: 0x5,
            }];
            bus.register_device_io(i8042_device, &resources)
                .map_err(Error::BusError)?;

            Ok(LegacyDeviceManager {
                i8042_reset_eventfd: exit_evt,
                com1_device,
                _com1_eventfd: com1_eventfd,
                com2_device,
                _com2_eventfd: com2_eventfd,
            })
        }

        /// Get the eventfd for exit notification.
        pub fn get_reset_eventfd(&self) -> Result<EventFd> {
            self.i8042_reset_eventfd.try_clone().map_err(Error::EventFd)
        }

        fn create_com_device(
            bus: &mut IoManager,
            vm_fd: Option<&Arc<VmFd>>,
            irq: u32,
            port_base: u16,
        ) -> Result<(Arc<Mutex<SerialDevice>>, EventFd)> {
            let eventfd = EventFd::new(libc::EFD_NONBLOCK).map_err(Error::EventFd)?;
            let device = Arc::new(Mutex::new(SerialDevice::new(
                eventfd.try_clone().map_err(Error::EventFd)?,
            )));
            // port_base defines the base port address for the COM devices.
            // Since every COM device has 8 data registers so we register the pio address range as size 0x8.
            let resources = [Resource::PioAddressRange {
                base: port_base,
                size: 0x8,
            }];
            bus.register_device_io(device.clone(), &resources)
                .map_err(Error::BusError)?;

            if let Some(fd) = vm_fd {
                fd.register_irqfd(&eventfd, irq)
                    .map_err(Error::IrqManager)?;
            }

            Ok((device, eventfd))
        }
    }
}

#[cfg(target_arch = "aarch64")]
pub(crate) mod aarch64 {
    use super::*;
    use dbs_device::device_manager::{IoManager};
    use dbs_device::resources::DeviceResources;
    use std::collections::HashMap;
    use kvm_ioctls::VmFd;

    type Result<T> = ::std::result::Result<T, Error>;

    /// LegacyDeviceType: com1
    pub const COM1: &str = "com1";
    /// LegacyDeviceType: com2
    pub const COM2: &str = "com2";
    /// LegacyDeviceType: rtc
    pub const RTC: &str = "rtc";

    impl LegacyDeviceManager {
        /// Create a LegacyDeviceManager instance handling legacy devices.
        pub fn create_manager(
            bus: &mut IoManager,
            vm_fd: Option<Arc<VmFd>>,
            resources: &HashMap<String, DeviceResources>,
        ) -> Result<Self> {
            let (com1_device, com1_eventfd) =
                Self::create_com_device(bus, vm_fd.as_ref(), resources.get(COM1).unwrap())?;
            let (com2_device, com2_eventfd) =
                Self::create_com_device(bus, vm_fd.as_ref(), resources.get(COM2).unwrap())?;
            let (rtc_device, rtc_eventfd) =
                Self::create_rtc_device(bus, vm_fd.as_ref(), resources.get(RTC).unwrap())?;

            Ok(LegacyDeviceManager {
                _rtc_device: rtc_device,
                _rtc_eventfd: rtc_eventfd,
                com1_device,
                _com1_eventfd: com1_eventfd,
                com2_device,
                _com2_eventfd: com2_eventfd,
            })
        }

        fn create_com_device(
            bus: &mut IoManager,
            vm_fd: Option<&Arc<VmFd>>,
            resources: &DeviceResources,
        ) -> Result<(Arc<Mutex<SerialDevice>>, EventFd)> {
            let eventfd = EventFd::new(libc::EFD_NONBLOCK).map_err(Error::EventFd)?;
            let device = Arc::new(Mutex::new(SerialDevice::new(
                eventfd.try_clone().map_err(Error::EventFd)?
            )));

            bus.register_device_io(device.clone(), resources.get_all_resources())
                .map_err(Error::BusError)?;

            if let Some(fd) = vm_fd {
                let irq = resources.get_legacy_irq().unwrap();
                fd.register_irqfd(&eventfd, irq)
                    .map_err(Error::IrqManager)?;
            }

            Ok((device, eventfd))
        }

        fn create_rtc_device(
            bus: &mut IoManager,
            vm_fd: Option<&Arc<VmFd>>,
            resources: &DeviceResources,
        ) -> Result<(Arc<Mutex<RTCDevice>>, EventFd)> {
            let eventfd = EventFd::new(libc::EFD_NONBLOCK).map_err(Error::EventFd)?;
            let device = Arc::new(Mutex::new(RTCDevice::new()));

            bus.register_device_io(device.clone(), resources.get_all_resources())
                .map_err(Error::BusError)?;

            if let Some(fd) = vm_fd {
                let irq = resources.get_legacy_irq().unwrap();
                fd.register_irqfd(&eventfd, irq)
                    .map_err(Error::IrqManager)?;
            }

            Ok((device, eventfd))
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "x86_64")]
    use super::*;

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn test_create_legacy_device_manager() {
        let mut bus = dbs_device::device_manager::IoManager::new();
        let mgr = LegacyDeviceManager::create_manager(&mut bus, None).unwrap();
        let _exit_fd = mgr.get_reset_eventfd().unwrap();
    }
}
