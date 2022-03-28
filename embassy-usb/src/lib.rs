#![no_std]
#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]

// This mod MUST go first, so that the others see its macros.
pub(crate) mod fmt;

mod builder;
pub mod class;
pub mod control;
pub mod descriptor;
pub mod driver;
pub mod types;
mod util;

use class::ControlInRequestStatus;
use heapless::Vec;

use self::class::{RequestStatus, UsbClass};
use self::control::*;
use self::descriptor::*;
use self::driver::*;
use self::types::*;
use self::util::*;

pub use self::builder::Config;
pub use self::builder::UsbDeviceBuilder;

/// The global state of the USB device.
///
/// In general class traffic is only possible in the `Configured` state.
#[repr(u8)]
#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum UsbDeviceState {
    /// The USB device has just been created or reset.
    Default,

    /// The USB device has received an address from the host.
    Addressed,

    /// The USB device has been configured and is fully functional.
    Configured,

    /// The USB device has been suspended by the host or it has been unplugged from the USB bus.
    Suspend,
}

/// The bConfiguration value for the not configured state.
pub const CONFIGURATION_NONE: u8 = 0;

/// The bConfiguration value for the single configuration supported by this device.
pub const CONFIGURATION_VALUE: u8 = 1;

/// The default value for bAlternateSetting for all interfaces.
pub const DEFAULT_ALTERNATE_SETTING: u8 = 0;

pub const MAX_CLASS_COUNT: usize = 4;

pub struct UsbDevice<'d, D: Driver<'d>> {
    bus: D::Bus,
    control: D::ControlPipe,

    config: Config<'d>,
    device_descriptor: &'d [u8],
    config_descriptor: &'d [u8],
    bos_descriptor: &'d [u8],

    device_state: UsbDeviceState,
    remote_wakeup_enabled: bool,
    self_powered: bool,
    pending_address: u8,

    classes: Vec<&'d mut dyn UsbClass, MAX_CLASS_COUNT>,
}

impl<'d, D: Driver<'d>> UsbDevice<'d, D> {
    pub(crate) fn build(
        mut driver: D,
        config: Config<'d>,
        device_descriptor: &'d [u8],
        config_descriptor: &'d [u8],
        bos_descriptor: &'d [u8],
        classes: Vec<&'d mut dyn UsbClass, MAX_CLASS_COUNT>,
    ) -> Self {
        let control = driver
            .alloc_control_pipe(config.max_packet_size_0 as u16)
            .expect("failed to alloc control endpoint");

        // Enable the USB bus.
        // This prevent further allocation by consuming the driver.
        let driver = driver.enable();

        Self {
            bus: driver,
            config,
            control,
            device_descriptor,
            config_descriptor,
            bos_descriptor,
            device_state: UsbDeviceState::Default,
            remote_wakeup_enabled: false,
            self_powered: false,
            pending_address: 0,
            classes,
        }
    }

    pub async fn run(&mut self) {
        loop {
            let control_fut = self.control.setup();
            let bus_fut = self.bus.poll();
            match select(bus_fut, control_fut).await {
                Either::Left(evt) => match evt {
                    Event::Reset => {
                        self.bus.reset();

                        self.device_state = UsbDeviceState::Default;
                        self.remote_wakeup_enabled = false;
                        self.pending_address = 0;

                        for c in self.classes.iter_mut() {
                            c.reset();
                        }
                    }
                    Event::Resume => {}
                    Event::Suspend => {
                        self.bus.suspend();
                        self.device_state = UsbDeviceState::Suspend;
                    }
                },
                Either::Right(req) => {
                    info!("control request: {:x}", req);

                    match req.direction {
                        UsbDirection::In => self.handle_control_in(req).await,
                        UsbDirection::Out => self.handle_control_out(req).await,
                    }
                }
            }
        }
    }

    async fn control_in_accept_writer(
        &mut self,
        req: Request,
        f: impl FnOnce(&mut DescriptorWriter),
    ) {
        let mut buf = [0; 256];
        let mut w = DescriptorWriter::new(&mut buf);
        f(&mut w);
        let pos = w.position().min(usize::from(req.length));
        self.control.accept_in(&buf[..pos]).await;
    }

    async fn handle_control_out(&mut self, req: Request) {
        {
            let mut buf = [0; 128];
            let data = if req.length > 0 {
                let size = self.control.data_out(&mut buf).await.unwrap();
                &buf[0..size]
            } else {
                &[]
            };

            for c in self.classes.iter_mut() {
                match c.control_out(req, data) {
                    RequestStatus::Accepted => return self.control.accept(),
                    RequestStatus::Rejected => return self.control.reject(),
                    RequestStatus::Unhandled => (),
                }
            }
        }

        const CONFIGURATION_NONE_U16: u16 = CONFIGURATION_NONE as u16;
        const CONFIGURATION_VALUE_U16: u16 = CONFIGURATION_VALUE as u16;
        const DEFAULT_ALTERNATE_SETTING_U16: u16 = DEFAULT_ALTERNATE_SETTING as u16;

        match req.request_type {
            RequestType::Standard => match (req.recipient, req.request, req.value) {
                (
                    Recipient::Device,
                    Request::CLEAR_FEATURE,
                    Request::FEATURE_DEVICE_REMOTE_WAKEUP,
                ) => {
                    self.remote_wakeup_enabled = false;
                    self.control.accept();
                }

                (Recipient::Endpoint, Request::CLEAR_FEATURE, Request::FEATURE_ENDPOINT_HALT) => {
                    //self.bus.set_stalled(((req.index as u8) & 0x8f).into(), false);
                    self.control.accept();
                }

                (
                    Recipient::Device,
                    Request::SET_FEATURE,
                    Request::FEATURE_DEVICE_REMOTE_WAKEUP,
                ) => {
                    self.remote_wakeup_enabled = true;
                    self.control.accept();
                }

                (Recipient::Endpoint, Request::SET_FEATURE, Request::FEATURE_ENDPOINT_HALT) => {
                    self.bus
                        .set_stalled(((req.index as u8) & 0x8f).into(), true);
                    self.control.accept();
                }

                (Recipient::Device, Request::SET_ADDRESS, 1..=127) => {
                    self.pending_address = req.value as u8;

                    // on NRF the hardware auto-handles SET_ADDRESS.
                    self.control.accept();
                }

                (Recipient::Device, Request::SET_CONFIGURATION, CONFIGURATION_VALUE_U16) => {
                    self.device_state = UsbDeviceState::Configured;
                    self.control.accept();
                }

                (Recipient::Device, Request::SET_CONFIGURATION, CONFIGURATION_NONE_U16) => {
                    match self.device_state {
                        UsbDeviceState::Default => {
                            self.control.accept();
                        }
                        _ => {
                            self.device_state = UsbDeviceState::Addressed;
                            self.control.accept();
                        }
                    }
                }

                (Recipient::Interface, Request::SET_INTERFACE, DEFAULT_ALTERNATE_SETTING_U16) => {
                    // TODO: do something when alternate settings are implemented
                    self.control.accept();
                }

                _ => self.control.reject(),
            },
            _ => self.control.reject(),
        }
    }

    async fn handle_control_in(&mut self, req: Request) {
        let mut buf = [0; 128];
        for c in self.classes.iter_mut() {
            match c.control_in(req, class::ControlIn::new(&mut buf)) {
                ControlInRequestStatus {
                    status: RequestStatus::Accepted,
                    data,
                } => return self.control.accept_in(data).await,
                ControlInRequestStatus {
                    status: RequestStatus::Rejected,
                    ..
                } => return self.control.reject(),
                ControlInRequestStatus {
                    status: RequestStatus::Unhandled,
                    ..
                } => (),
            }
        }

        match req.request_type {
            RequestType::Standard => match (req.recipient, req.request) {
                (Recipient::Device, Request::GET_STATUS) => {
                    let mut status: u16 = 0x0000;
                    if self.self_powered {
                        status |= 0x0001;
                    }
                    if self.remote_wakeup_enabled {
                        status |= 0x0002;
                    }
                    self.control.accept_in(&status.to_le_bytes()).await;
                }

                (Recipient::Interface, Request::GET_STATUS) => {
                    let status: u16 = 0x0000;
                    self.control.accept_in(&status.to_le_bytes()).await;
                }

                (Recipient::Endpoint, Request::GET_STATUS) => {
                    let ep_addr: EndpointAddress = ((req.index as u8) & 0x8f).into();
                    let mut status: u16 = 0x0000;
                    if self.bus.is_stalled(ep_addr) {
                        status |= 0x0001;
                    }
                    self.control.accept_in(&status.to_le_bytes()).await;
                }

                (Recipient::Device, Request::GET_DESCRIPTOR) => {
                    self.handle_get_descriptor(req).await;
                }

                (Recipient::Device, Request::GET_CONFIGURATION) => {
                    let status = match self.device_state {
                        UsbDeviceState::Configured => CONFIGURATION_VALUE,
                        _ => CONFIGURATION_NONE,
                    };
                    self.control.accept_in(&status.to_le_bytes()).await;
                }

                (Recipient::Interface, Request::GET_INTERFACE) => {
                    // TODO: change when alternate settings are implemented
                    let status = DEFAULT_ALTERNATE_SETTING;
                    self.control.accept_in(&status.to_le_bytes()).await;
                }
                _ => self.control.reject(),
            },
            _ => self.control.reject(),
        }
    }

    async fn handle_get_descriptor(&mut self, req: Request) {
        let (dtype, index) = req.descriptor_type_index();
        let config = self.config.clone();

        match dtype {
            descriptor_type::BOS => self.control.accept_in(self.bos_descriptor).await,
            descriptor_type::DEVICE => self.control.accept_in(self.device_descriptor).await,
            descriptor_type::CONFIGURATION => self.control.accept_in(self.config_descriptor).await,
            descriptor_type::STRING => {
                if index == 0 {
                    self.control_in_accept_writer(req, |w| {
                        w.write(descriptor_type::STRING, &lang_id::ENGLISH_US.to_le_bytes())
                            .unwrap();
                    })
                    .await
                } else {
                    let s = match index {
                        1 => self.config.manufacturer,
                        2 => self.config.product,
                        3 => self.config.serial_number,
                        _ => {
                            let index = StringIndex::new(index);
                            let lang_id = req.index;
                            None
                            //classes
                            //    .iter()
                            //    .filter_map(|cls| cls.get_string(index, lang_id))
                            //    .nth(0)
                        }
                    };

                    if let Some(s) = s {
                        self.control_in_accept_writer(req, |w| w.string(s).unwrap())
                            .await;
                    } else {
                        self.control.reject()
                    }
                }
            }
            _ => self.control.reject(),
        }
    }
}
