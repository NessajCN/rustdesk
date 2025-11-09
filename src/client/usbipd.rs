use super::usbip_protocol::{UsbIpCommand, UsbIpResponse};
use super::usbip_protocol::{USBIP_RET_SUBMIT, USBIP_RET_UNLINK};
use hbb_common::tokio_util::io::StreamReader;
use hbb_common::{
    allow_err,
    futures::stream::iter,
    message_proto::{self, Message},
    tokio::{io, sync::RwLock},
};
use hbb_common::{log, Stream};
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use rusb::{Device, DeviceHandle, GlobalContext, Version as rusbVersion};
use std::any::Any;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::{Arc, Mutex};

/// A handler to pass requests to interface of a rusb USB device of the host
#[derive(Clone, Debug)]
pub struct RusbUsbHostInterfaceHandler {
    handle: Arc<Mutex<DeviceHandle<GlobalContext>>>,
}

impl RusbUsbHostInterfaceHandler {
    pub fn new(handle: Arc<Mutex<DeviceHandle<GlobalContext>>>) -> Self {
        Self { handle }
    }
}

impl UsbInterfaceHandler for RusbUsbHostInterfaceHandler {
    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        ep: UsbEndpoint,
        transfer_buffer_length: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        log::debug!("To host device: ep={ep:?} setup={setup:?} req={req:?}",);
        let mut buffer = vec![0u8; transfer_buffer_length as usize];
        let timeout = std::time::Duration::new(1, 0);
        let handle = self.handle.lock().unwrap();
        if ep.attributes == EndpointAttributes::Control as u8 {
            // control
            if let Direction::In = ep.direction() {
                // control in
                if let Ok(len) = handle.read_control(
                    setup.request_type,
                    setup.request,
                    setup.value,
                    setup.index,
                    &mut buffer,
                    timeout,
                ) {
                    return Ok(Vec::from(&buffer[..len]));
                }
            } else {
                // control out
                handle
                    .write_control(
                        setup.request_type,
                        setup.request,
                        setup.value,
                        setup.index,
                        req,
                        timeout,
                    )
                    .ok();
            }
        } else if ep.attributes == EndpointAttributes::Interrupt as u8 {
            // interrupt
            if let Direction::In = ep.direction() {
                // interrupt in
                if let Ok(len) = handle.read_interrupt(ep.address, &mut buffer, timeout) {
                    log::info!("intr in {:?}", &buffer[..len]);
                    return Ok(Vec::from(&buffer[..len]));
                }
            } else {
                // interrupt out
                handle.write_interrupt(ep.address, req, timeout).ok();
            }
        } else if ep.attributes == EndpointAttributes::Bulk as u8 {
            // bulk
            if let Direction::In = ep.direction() {
                // bulk in
                if let Ok(len) = handle.read_bulk(ep.address, &mut buffer, timeout) {
                    return Ok(Vec::from(&buffer[..len]));
                }
            } else {
                // bulk out
                handle.write_bulk(ep.address, req, timeout).ok();
            }
        }
        Ok(vec![])
    }

    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        vec![]
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// A handler to pass requests to device of a rusb USB device of the host
#[derive(Clone, Debug)]
pub struct RusbUsbHostDeviceHandler {
    handle: Arc<Mutex<DeviceHandle<GlobalContext>>>,
}

impl RusbUsbHostDeviceHandler {
    pub fn new(handle: Arc<Mutex<DeviceHandle<GlobalContext>>>) -> Self {
        Self { handle }
    }
}

impl UsbDeviceHandler for RusbUsbHostDeviceHandler {
    fn handle_urb(
        &mut self,
        transfer_buffer_length: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        log::debug!("To host device: setup={setup:?} req={req:?}");
        let mut buffer = vec![0u8; transfer_buffer_length as usize];
        let timeout = std::time::Duration::new(1, 0);
        let handle = self.handle.lock().unwrap();
        // control
        if setup.request_type & 0x80 == 0 {
            // control out
            handle
                .write_control(
                    setup.request_type,
                    setup.request,
                    setup.value,
                    setup.index,
                    req,
                    timeout,
                )
                .ok();
        } else {
            // control in
            if let Ok(len) = handle.read_control(
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                &mut buffer,
                timeout,
            ) {
                return Ok(Vec::from(&buffer[..len]));
            }
        }
        Ok(vec![])
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Parse the SETUP packet of control transfers
#[derive(Clone, Copy, Debug, Default)]
pub struct SetupPacket {
    /// bmRequestType
    pub request_type: u8,
    /// bRequest
    pub request: u8,
    /// wValue
    pub value: u16,
    /// wIndex
    pub index: u16,
    /// wLength
    pub length: u16,
}

impl SetupPacket {
    /// Parse a [SetupPacket] from raw setup packet
    pub fn parse(setup: &[u8; 8]) -> SetupPacket {
        SetupPacket {
            request_type: setup[0],
            request: setup[1],
            value: ((setup[3] as u16) << 8) | (setup[2] as u16),
            index: ((setup[5] as u16) << 8) | (setup[4] as u16),
            length: ((setup[7] as u16) << 8) | (setup[6] as u16),
        }
    }
}

/// Represent a USB interface
#[derive(Clone, Debug)]
pub struct UsbInterface {
    pub interface_class: u8,
    pub interface_subclass: u8,
    pub interface_protocol: u8,
    pub endpoints: Vec<UsbEndpoint>,
    pub string_interface: u8,
    pub class_specific_descriptor: Vec<u8>,

    pub handler: Arc<Mutex<Box<dyn UsbInterfaceHandler + Send>>>,
}

/// A handler of a custom usb interface
pub trait UsbInterfaceHandler: std::fmt::Debug {
    /// Return the class specific descriptor which is inserted between interface descriptor and endpoint descriptor
    fn get_class_specific_descriptor(&self) -> Vec<u8>;

    /// Handle a URB(USB Request Block) targeting at this interface
    ///
    /// Can be one of: control transfer to ep0 or other types of transfer to its endpoint.
    /// The resulting data should not exceed `transfer_buffer_length`.
    fn handle_urb(
        &mut self,
        interface: &UsbInterface,
        ep: UsbEndpoint,
        transfer_buffer_length: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>>;

    /// Helper to downcast to actual struct
    ///
    /// Please implement it as:
    /// ```ignore
    /// fn as_any(&mut self) -> &mut dyn Any {
    ///     self
    /// }
    /// ```
    fn as_any(&mut self) -> &mut dyn Any;
}

/// A list of defined USB endpoint attributes
#[derive(Copy, Clone, Debug, FromPrimitive)]
pub enum EndpointAttributes {
    Control = 0,
    Isochronous,
    Bulk,
    Interrupt,
}

/// USB endpoint direction: IN or OUT
/// Already exists in rusb crate
pub use rusb::Direction;

/// A list of defined USB standard requests
/// from USB 2.0 standard Table 9.4. Standard Request Codes
#[derive(Copy, Clone, Debug, FromPrimitive)]
pub enum StandardRequest {
    GetStatus = 0,
    ClearFeature = 1,
    SetFeature = 3,
    SetAddress = 5,
    GetDescriptor = 6,
    SetDescriptor = 7,
    GetConfiguration = 8,
    SetConfiguration = 9,
    GetInterface = 10,
    SetInterface = 11,
    SynchFrame = 12,
}

/// A list of defined USB descriptor types
/// from USB 2.0 standard Table 9.5. Descriptor Types
#[derive(Copy, Clone, Debug, FromPrimitive)]
pub enum DescriptorType {
    /// DEVICE
    Device = 1,
    /// CONFIGURATION
    Configuration = 2,
    /// STRING
    String = 3,
    /// INTERFACE
    Interface = 4,
    /// ENDPOINT
    Endpoint = 5,
    /// DEVICE_QUALIFIER
    DeviceQualifier = 6,
    /// OTHER_SPEED_CONFIGURATION
    OtherSpeedConfiguration = 7,
    /// INTERFACE_POINTER
    InterfacePower = 8,
    /// OTG
    OTG = 9,
    /// DEBUG
    Debug = 0xA,
    /// INTERFACE_ASSOCIATION
    InterfaceAssociation = 0xB,
    /// BOS
    BOS = 0xF,
    // DEVICE CAPABILITY
    DeviceCapability = 0x10,
    /// SUPERSPEED_USB_ENDPOINT_COMPANION
    SuperspeedUsbEndpointCompanion = 0x30,
}

/// Represent a USB endpoint
#[derive(Clone, Copy, Debug, Default)]
pub struct UsbEndpoint {
    /// bEndpointAddress
    pub address: u8,
    /// bmAttributes
    pub attributes: u8,
    /// wMaxPacketSize
    pub max_packet_size: u16,
    /// bInterval
    pub interval: u8,
}

impl UsbEndpoint {
    /// Get direction from MSB of address
    pub fn direction(&self) -> Direction {
        if self.address & 0x80 != 0 {
            Direction::In
        } else {
            Direction::Out
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
}

impl From<rusbVersion> for Version {
    fn from(value: rusbVersion) -> Self {
        Self {
            major: value.major(),
            minor: value.minor(),
            patch: value.sub_minor(),
        }
    }
}

impl From<Version> for rusbVersion {
    fn from(val: Version) -> Self {
        rusbVersion(val.major, val.minor, val.patch)
    }
}

/// bcdDevice
impl From<u16> for Version {
    fn from(value: u16) -> Self {
        Self {
            major: (value >> 8) as u8,
            minor: ((value >> 4) & 0xF) as u8,
            patch: (value & 0xF) as u8,
        }
    }
}

/// Represent a USB device
#[derive(Clone, Default, Debug)]
pub struct UsbDevice {
    pub path: String,
    pub bus_id: String,
    pub bus_num: u32,
    pub dev_num: u32,
    pub speed: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_bcd: Version,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub num_configurations: u8,
    pub interfaces: Vec<UsbInterface>,

    pub device_handler: Option<Arc<Mutex<Box<dyn UsbDeviceHandler + Send>>>>,

    pub usb_version: Version,

    pub ep0_in: UsbEndpoint,
    pub ep0_out: UsbEndpoint,
    // strings
    pub string_pool: HashMap<u8, String>,
    pub string_configuration: u8,
    pub string_manufacturer: u8,
    pub string_product: u8,
    pub string_serial: u8,
}

impl UsbDevice {
    pub fn new_string(&mut self, s: &str) -> u8 {
        for i in 1.. {
            if let std::collections::hash_map::Entry::Vacant(e) = self.string_pool.entry(i) {
                e.insert(s.to_string());
                return i;
            }
        }
        panic!("string poll exhausted")
    }

    pub fn find_ep(&self, ep: u8) -> Option<(UsbEndpoint, Option<&UsbInterface>)> {
        if ep == self.ep0_in.address {
            Some((self.ep0_in, None))
        } else if ep == self.ep0_out.address {
            Some((self.ep0_out, None))
        } else {
            for intf in &self.interfaces {
                for endpoint in &intf.endpoints {
                    if endpoint.address == ep {
                        return Some((*endpoint, Some(intf)));
                    }
                }
            }
            None
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(312);

        let mut path = self.path.as_bytes().to_vec();
        debug_assert!(path.len() <= 256);
        path.resize(256, 0);
        result.extend_from_slice(path.as_slice());

        let mut bus_id = self.bus_id.as_bytes().to_vec();
        debug_assert!(bus_id.len() <= 32);
        bus_id.resize(32, 0);
        result.extend_from_slice(bus_id.as_slice());

        result.extend_from_slice(&self.bus_num.to_be_bytes());
        result.extend_from_slice(&self.dev_num.to_be_bytes());
        result.extend_from_slice(&self.speed.to_be_bytes());
        result.extend_from_slice(&self.vendor_id.to_be_bytes());
        result.extend_from_slice(&self.product_id.to_be_bytes());
        result.push(self.device_bcd.major);
        result.push(self.device_bcd.minor);
        result.push(self.device_class);
        result.push(self.device_subclass);
        result.push(self.device_protocol);
        result.push(self.configuration_value);
        result.push(self.num_configurations);
        result.push(self.interfaces.len() as u8);

        result
    }

    pub fn to_bytes_with_interfaces(&self) -> Vec<u8> {
        let mut result = self.to_bytes();
        result.reserve(4 * self.interfaces.len());

        for intf in &self.interfaces {
            result.push(intf.interface_class);
            result.push(intf.interface_subclass);
            result.push(intf.interface_protocol);
            result.push(0); // padding
        }

        result
    }

    pub async fn handle_urb(
        &self,
        ep: UsbEndpoint,
        intf: Option<&UsbInterface>,
        transfer_buffer_length: u32,
        setup_packet: SetupPacket,
        out_data: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        use DescriptorType::*;
        use Direction::*;
        use EndpointAttributes::*;
        use StandardRequest::*;

        match (FromPrimitive::from_u8(ep.attributes), ep.direction()) {
            (Some(Control), In) => {
                // control in
                log::debug!("Control IN setup={setup_packet:x?}");
                match (
                    setup_packet.request_type,
                    FromPrimitive::from_u8(setup_packet.request),
                ) {
                    (0b10000000, Some(GetDescriptor)) => {
                        // high byte: type
                        match FromPrimitive::from_u16(setup_packet.value >> 8) {
                            Some(Device) => {
                                log::debug!("Get device descriptor");
                                // Standard Device Descriptor
                                let mut desc = vec![
                                    0x12,         // bLength
                                    Device as u8, // bDescriptorType: Device
                                    self.usb_version.minor,
                                    self.usb_version.major, // bcdUSB: USB 2.0
                                    self.device_class,      // bDeviceClass
                                    self.device_subclass,   // bDeviceSubClass
                                    self.device_protocol,   // bDeviceProtocol
                                    self.ep0_in.max_packet_size as u8, // bMaxPacketSize0
                                    self.vendor_id as u8,   // idVendor
                                    (self.vendor_id >> 8) as u8,
                                    self.product_id as u8, // idProduct
                                    (self.product_id >> 8) as u8,
                                    self.device_bcd.minor, // bcdDevice
                                    self.device_bcd.major,
                                    self.string_manufacturer, // iManufacturer
                                    self.string_product,      // iProduct
                                    self.string_serial,       // iSerial
                                    self.num_configurations,  // bNumConfigurations
                                ];

                                // requested len too short: wLength < real length
                                if setup_packet.length < desc.len() as u16 {
                                    desc.resize(setup_packet.length as usize, 0);
                                }
                                Ok(desc)
                            }
                            Some(BOS) => {
                                log::debug!("Get BOS descriptor");
                                let mut desc = vec![
                                    0x05,      // bLength
                                    BOS as u8, // bDescriptorType: BOS
                                    0x05, 0x00, // wTotalLength
                                    0x00, // bNumCapabilities
                                ];

                                // requested len too short: wLength < real length
                                if setup_packet.length < desc.len() as u16 {
                                    desc.resize(setup_packet.length as usize, 0);
                                }
                                Ok(desc)
                            }
                            Some(Configuration) => {
                                log::debug!("Get configuration descriptor");
                                // Standard Configuration Descriptor
                                let mut desc = vec![
                                    0x09,                // bLength
                                    Configuration as u8, // bDescriptorType: Configuration
                                    0x00,
                                    0x00, // wTotalLength: to be filled below
                                    self.interfaces.len() as u8, // bNumInterfaces
                                    self.configuration_value, // bConfigurationValue
                                    self.string_configuration, // iConfiguration
                                    0x80, // bmAttributes: Bus Powered
                                    0x32, // bMaxPower: 100mA
                                ];
                                for (i, intf) in self.interfaces.iter().enumerate() {
                                    let mut intf_desc = vec![
                                        0x09,                       // bLength
                                        Interface as u8,            // bDescriptorType: Interface
                                        i as u8,                    // bInterfaceNum
                                        0x00,                       // bAlternateSettings
                                        intf.endpoints.len() as u8, // bNumEndpoints
                                        intf.interface_class,       // bInterfaceClass
                                        intf.interface_subclass,    // bInterfaceSubClass
                                        intf.interface_protocol,    // bInterfaceProtocol
                                        intf.string_interface,      //iInterface
                                    ];
                                    // class specific endpoint
                                    let mut specific = intf.class_specific_descriptor.clone();
                                    intf_desc.append(&mut specific);
                                    // endpoint descriptors
                                    for endpoint in &intf.endpoints {
                                        let mut ep_desc = vec![
                                            0x07,                // bLength
                                            Endpoint as u8,      // bDescriptorType: Endpoint
                                            endpoint.address,    // bEndpointAddress
                                            endpoint.attributes, // bmAttributes
                                            endpoint.max_packet_size as u8,
                                            (endpoint.max_packet_size >> 8) as u8, // wMaxPacketSize
                                            endpoint.interval,                     // bInterval
                                        ];
                                        intf_desc.append(&mut ep_desc);
                                    }
                                    desc.append(&mut intf_desc);
                                }
                                // length
                                let len = desc.len() as u16;
                                desc[2] = len as u8;
                                desc[3] = (len >> 8) as u8;

                                // requested len too short: wLength < real length
                                if setup_packet.length < desc.len() as u16 {
                                    desc.resize(setup_packet.length as usize, 0);
                                }
                                Ok(desc)
                            }
                            Some(String) => {
                                log::debug!("Get string descriptor");
                                let index = setup_packet.value as u8;
                                if index == 0 {
                                    // String Descriptor Zero, Specifying Languages Supported by the Device
                                    // language ids
                                    let mut desc = vec![
                                        4,                            // bLength
                                        DescriptorType::String as u8, // bDescriptorType
                                        0x09,
                                        0x04, // wLANGID[0], en-US
                                    ];
                                    // requested len too short: wLength < real length
                                    if setup_packet.length < desc.len() as u16 {
                                        desc.resize(setup_packet.length as usize, 0);
                                    }
                                    Ok(desc)
                                } else if let Some(s) = &self.string_pool.get(&index) {
                                    // UNICODE String Descriptor
                                    let bytes: Vec<u16> = s.encode_utf16().collect();
                                    let mut desc = vec![
                                        2 + bytes.len() as u8 * 2,    // bLength
                                        DescriptorType::String as u8, // bDescriptorType
                                    ];
                                    for byte in bytes {
                                        desc.push(byte as u8);
                                        desc.push((byte >> 8) as u8);
                                    }

                                    // requested len too short: wLength < real length
                                    if setup_packet.length < desc.len() as u16 {
                                        desc.resize(setup_packet.length as usize, 0);
                                    }
                                    Ok(desc)
                                } else {
                                    Err(std::io::Error::new(
                                        std::io::ErrorKind::InvalidInput,
                                        format!("Invalid string index: {index}"),
                                    ))
                                }
                            }
                            Some(DeviceQualifier) => {
                                log::debug!("Get device qualifier descriptor");
                                // Device_Qualifier Descriptor
                                let mut desc = vec![
                                    0x0A,                  // bLength
                                    DeviceQualifier as u8, // bDescriptorType: Device Qualifier
                                    self.usb_version.minor,
                                    self.usb_version.major, // bcdUSB
                                    self.device_class,      // bDeviceClass
                                    self.device_subclass,   // bDeviceSUbClass
                                    self.device_protocol,   // bDeviceProtocol
                                    self.ep0_in.max_packet_size as u8, // bMaxPacketSize0
                                    self.num_configurations, // bNumConfigurations
                                    0x00,                   // bReserved
                                ];

                                // requested len too short: wLength < real length
                                if setup_packet.length < desc.len() as u16 {
                                    desc.resize(setup_packet.length as usize, 0);
                                }
                                Ok(desc)
                            }
                            _ => {
                                log::warn!("unknown desc type: {setup_packet:x?}");
                                Ok(vec![])
                            }
                        }
                    }
                    _ if setup_packet.request_type & 0xF == 1 => {
                        // to interface
                        // see https://www.beyondlogic.org/usbnutshell/usb6.shtml
                        // only low 8 bits are valid
                        let intf = &self.interfaces[setup_packet.index as usize & 0xFF];
                        let mut handler = intf.handler.lock().unwrap();
                        handler.handle_urb(intf, ep, transfer_buffer_length, setup_packet, out_data)
                    }
                    _ if setup_packet.request_type & 0xF == 0 && self.device_handler.is_some() => {
                        // to device
                        // see https://www.beyondlogic.org/usbnutshell/usb6.shtml
                        let lock = self.device_handler.as_ref().unwrap();
                        let mut handler = lock.lock().unwrap();
                        handler.handle_urb(transfer_buffer_length, setup_packet, out_data)
                    }
                    _ => unimplemented!("control in"),
                }
            }
            (Some(Control), Out) => {
                // control out
                log::debug!("Control OUT setup={setup_packet:x?}");
                match (
                    setup_packet.request_type,
                    FromPrimitive::from_u8(setup_packet.request),
                ) {
                    (0b00000000, Some(SetConfiguration)) => {
                        let mut desc = vec![
                            self.configuration_value, // bConfigurationValue
                        ];

                        // requested len too short: wLength < real length
                        if setup_packet.length < desc.len() as u16 {
                            desc.resize(setup_packet.length as usize, 0);
                        }
                        Ok(desc)
                    }
                    _ if setup_packet.request_type & 0xF == 1 => {
                        // to interface
                        // see https://www.beyondlogic.org/usbnutshell/usb6.shtml
                        // only low 8 bits are valid
                        let intf = &self.interfaces[setup_packet.index as usize & 0xFF];
                        let mut handler = intf.handler.lock().unwrap();
                        handler.handle_urb(intf, ep, transfer_buffer_length, setup_packet, out_data)
                    }
                    _ if setup_packet.request_type & 0xF == 0 && self.device_handler.is_some() => {
                        // to device
                        // see https://www.beyondlogic.org/usbnutshell/usb6.shtml
                        let lock = self.device_handler.as_ref().unwrap();
                        let mut handler = lock.lock().unwrap();
                        handler.handle_urb(transfer_buffer_length, setup_packet, out_data)
                    }
                    _ => unimplemented!("control out"),
                }
            }
            (Some(_), _) => {
                // others
                let intf = intf.unwrap();
                let mut handler = intf.handler.lock().unwrap();
                handler.handle_urb(intf, ep, transfer_buffer_length, setup_packet, out_data)
            }
            _ => unimplemented!("transfer to {:?}", ep),
        }
    }
}

/// A handler for URB targeting the device
pub trait UsbDeviceHandler: std::fmt::Debug {
    /// Handle a URB(USB Request Block) targeting at this device
    ///
    /// When the lower 4 bits of `bmRequestType` is zero and the URB is not handled by the library, this function is called.
    /// The resulting data should not exceed `transfer_buffer_length`
    fn handle_urb(
        &mut self,
        transfer_buffer_length: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>>;

    /// Helper to downcast to actual struct
    ///
    /// Please implement it as:
    /// ```ignore
    /// fn as_any(&mut self) -> &mut dyn Any {
    ///     self
    /// }
    /// ```
    fn as_any(&mut self) -> &mut dyn Any;
}

/// Main struct of a USB/IP server
#[derive(Default)]
pub struct UsbIpServer {
    available_devices: RwLock<Vec<UsbDevice>>,
    used_devices: RwLock<HashMap<String, UsbDevice>>,
}

pub async fn read_usbip_command_from_bytes(raw: bytes::Bytes) -> io::Result<UsbIpCommand> {
    let stream = iter(vec![io::Result::Ok(raw)]);
    let mut reader = StreamReader::new(stream);
    UsbIpCommand::read_from_socket(&mut reader).await
}

impl UsbIpServer {
    /// Create a [UsbIpServer] with Vec<[rusb::DeviceHandle]> for sharing host devices
    pub fn with_rusb_device_handles(
        device_handles: Vec<DeviceHandle<GlobalContext>>,
    ) -> Vec<UsbDevice> {
        let mut devices = vec![];
        for open_device in device_handles {
            let dev = open_device.device();
            let desc = match dev.device_descriptor() {
                Ok(desc) => desc,
                Err(err) => {
                    log::warn!(
                        "Impossible to get device descriptor for {dev:?}: {err}, ignoring device",
                    );
                    continue;
                }
            };
            let cfg = match dev.active_config_descriptor() {
                Ok(desc) => desc,
                Err(err) => {
                    log::warn!(
                        "Impossible to get config descriptor for {dev:?}: {err}, ignoring device",
                    );
                    continue;
                }
            };

            let handle = Arc::new(Mutex::new(open_device));
            let mut interfaces = vec![];
            handle
                .lock()
                .unwrap()
                .set_auto_detach_kernel_driver(true)
                .ok();
            for intf in cfg.interfaces() {
                // ignore alternate settings
                let intf_desc = intf.descriptors().next().unwrap();
                handle
                    .lock()
                    .unwrap()
                    .set_auto_detach_kernel_driver(true)
                    .ok();
                let mut endpoints = vec![];

                for ep_desc in intf_desc.endpoint_descriptors() {
                    endpoints.push(UsbEndpoint {
                        address: ep_desc.address(),
                        attributes: ep_desc.transfer_type() as u8,
                        max_packet_size: ep_desc.max_packet_size(),
                        interval: ep_desc.interval(),
                    });
                }

                let handler = Arc::new(Mutex::new(Box::new(RusbUsbHostInterfaceHandler::new(
                    handle.clone(),
                ))
                    as Box<dyn UsbInterfaceHandler + Send>));
                interfaces.push(UsbInterface {
                    interface_class: intf_desc.class_code(),
                    interface_subclass: intf_desc.sub_class_code(),
                    interface_protocol: intf_desc.protocol_code(),
                    endpoints,
                    string_interface: intf_desc.description_string_index().unwrap_or(0),
                    class_specific_descriptor: Vec::from(intf_desc.extra()),
                    handler,
                });
            }
            let mut device = UsbDevice {
                path: format!(
                    "/sys/bus/{}/{}/{}",
                    dev.bus_number(),
                    dev.address(),
                    dev.port_number()
                ),
                bus_id: format!(
                    "{}-{}-{}",
                    dev.bus_number(),
                    dev.address(),
                    dev.port_number()
                ),
                bus_num: dev.bus_number() as u32,
                dev_num: dev.port_number() as u32,
                speed: dev.speed() as u32,
                vendor_id: desc.vendor_id(),
                product_id: desc.product_id(),
                device_class: desc.class_code(),
                device_subclass: desc.sub_class_code(),
                device_protocol: desc.protocol_code(),
                device_bcd: desc.device_version().into(),
                configuration_value: cfg.number(),
                num_configurations: desc.num_configurations(),
                ep0_in: UsbEndpoint {
                    address: 0x80,
                    attributes: EndpointAttributes::Control as u8,
                    max_packet_size: desc.max_packet_size() as u16,
                    interval: 0,
                },
                ep0_out: UsbEndpoint {
                    address: 0x00,
                    attributes: EndpointAttributes::Control as u8,
                    max_packet_size: desc.max_packet_size() as u16,
                    interval: 0,
                },
                interfaces,
                device_handler: Some(Arc::new(Mutex::new(Box::new(
                    RusbUsbHostDeviceHandler::new(handle.clone()),
                )))),
                usb_version: desc.usb_version().into(),
                ..UsbDevice::default()
            };

            // set strings
            if let Some(index) = desc.manufacturer_string_index() {
                device.string_manufacturer = device.new_string(
                    &handle
                        .lock()
                        .unwrap()
                        .read_string_descriptor_ascii(index)
                        .unwrap(),
                )
            }
            if let Some(index) = desc.product_string_index() {
                device.string_product = device.new_string(
                    &handle
                        .lock()
                        .unwrap()
                        .read_string_descriptor_ascii(index)
                        .unwrap(),
                )
            }
            if let Some(index) = desc.serial_number_string_index() {
                device.string_serial = device.new_string(
                    &handle
                        .lock()
                        .unwrap()
                        .read_string_descriptor_ascii(index)
                        .unwrap(),
                )
            }
            devices.push(device);
        }
        devices
    }

    fn with_rusb_devices(device_list: Vec<Device<GlobalContext>>) -> Vec<UsbDevice> {
        let mut device_handles = vec![];

        for dev in device_list {
            let open_device = match dev.open() {
                Ok(dev) => dev,
                Err(err) => {
                    log::warn!("Impossible to share {dev:?}: {err}, ignoring device");
                    continue;
                }
            };
            device_handles.push(open_device);
        }
        Self::with_rusb_device_handles(device_handles)
    }

    /// Create a [UsbIpServer] exposing devices in the host, and redirect all USB transfers to them using libusb \
    /// Some Windows version has no libusb preinstalled. \
    /// Replace current usb driver with libusb via https://github.com/mcuee/libusb-win32/releases/tag/release_1.4.0.0
    pub fn new_from_host() -> Self {
        Self::new_from_host_with_filter(|_| true)
    }

    /// Create a [UsbIpServer] exposing filtered devices in the host, and redirect all USB transfers to them using libusb
    pub fn new_from_host_with_filter<F>(filter: F) -> Self
    where
        F: FnMut(&Device<GlobalContext>) -> bool,
    {
        match rusb::devices() {
            Ok(list) => {
                let mut devs = vec![];
                for d in list.iter().filter(filter) {
                    devs.push(d)
                }
                Self {
                    available_devices: RwLock::new(Self::with_rusb_devices(devs)),
                    ..Default::default()
                }
            }
            Err(_) => Default::default(),
        }
    }

    pub async fn handler(
        &self,
        command: io::Result<UsbIpCommand>,
        current_import_device_id: Arc<Mutex<Option<String>>>,
        peer: &mut Stream,
    ) -> std::io::Result<()> {
        if let Err(err) = command {
            let current_import_device_id = current_import_device_id.lock().unwrap();
            if let Some(dev_id) = current_import_device_id.as_ref() {
                let mut used_devices = self.used_devices.write().await;
                let mut available_devices = self.available_devices.write().await;
                match used_devices.remove(dev_id) {
                    Some(dev) => available_devices.push(dev),
                    None => unreachable!(),
                }
            }

            if err.kind() == ErrorKind::UnexpectedEof {
                log::info!("Remote closed the connection");
                return Ok(());
            } else {
                return Err(err);
            }
        }

        let used_devices = self.used_devices.read().await;
        let mut current_import_device = {
            let cidi = current_import_device_id.lock().unwrap();
            cidi.clone().and_then(|ref id| used_devices.get(id))
        };

        let command = command?;
        match command {
            UsbIpCommand::OpReqDevlist { .. } => {
                log::info!("Got OP_REQ_DEVLIST");
                let devices = self.available_devices.read().await;

                // OP_REP_DEVLIST
                let res = UsbIpResponse::op_rep_devlist(devices.as_slice());
                let mut msg = Message::new();
                msg.set_usb_ip_response(message_proto::UsbIpResponse {
                    raw_response: res.to_bytes().into(),
                    ..Default::default()
                });
                allow_err!(peer.send(&msg).await);
                // UsbIpResponse::op_rep_devlist(&devices)
                //     .write_to_socket(socket)
                //     .await?;
                log::info!("Sent OP_REP_DEVLIST");
            }
            UsbIpCommand::OpReqImport { busid, .. } => {
                log::info!("Got OP_REQ_IMPORT");
                {
                    let mut cidi = current_import_device_id.lock().unwrap();
                    *cidi = None;
                }
                current_import_device = None;
                std::mem::drop(used_devices);

                let mut used_devices = self.used_devices.write().await;
                let mut available_devices = self.available_devices.write().await;
                let busid_compare =
                    &busid[..busid.iter().position(|&x| x == 0).unwrap_or(busid.len())];
                for (i, dev) in available_devices.iter().enumerate() {
                    if busid_compare == dev.bus_id.as_bytes() {
                        let dev = available_devices.remove(i);
                        let dev_id = dev.bus_id.clone();
                        used_devices.insert(dev.bus_id.clone(), dev);
                        {
                            let mut cidi = current_import_device_id.lock().unwrap();
                            *cidi = Some(dev_id.clone());
                        }
                        // current_import_device_id = dev_id.clone().into();
                        current_import_device = Some(used_devices.get(&dev_id).unwrap());
                        break;
                    }
                }

                let res = if let Some(dev) = current_import_device {
                    UsbIpResponse::op_rep_import_success(dev)
                } else {
                    UsbIpResponse::op_rep_import_fail()
                };

                let mut msg = Message::new();
                msg.set_usb_ip_response(message_proto::UsbIpResponse {
                    raw_response: res.to_bytes().into(),
                    ..Default::default()
                });
                allow_err!(peer.send(&msg).await);
                // res.write_to_socket(socket).await?;
                log::info!("Sent OP_REP_IMPORT");
            }
            UsbIpCommand::UsbIpCmdSubmit {
                mut header,
                transfer_buffer_length,
                setup,
                data,
                ..
            } => {
                log::info!("Got USBIP_CMD_SUBMIT");
                let device = current_import_device.unwrap();

                let out = header.direction == 0;
                let real_ep = if out { header.ep } else { header.ep | 0x80 };

                header.command = USBIP_RET_SUBMIT.into();

                let res = match device.find_ep(real_ep as u8) {
                    None => {
                        log::warn!("Endpoint {real_ep:02x?} not found");
                        UsbIpResponse::usbip_ret_submit_fail(&header)
                    }
                    Some((ep, intf)) => {
                        log::info!("->Endpoint {ep:02x?}");
                        log::info!("->Setup {setup:02x?}");
                        log::info!("->Request {data:02x?}");
                        let resp = device
                            .handle_urb(
                                ep,
                                intf,
                                transfer_buffer_length,
                                SetupPacket::parse(&setup),
                                &data,
                            )
                            .await;

                        match resp {
                            Ok(resp) => {
                                if out {
                                    log::info!("<-Wrote {}", data.len());
                                } else {
                                    log::info!("<-Resp {resp:02x?}");
                                }
                                UsbIpResponse::usbip_ret_submit_success(&header, 0, 0, resp, vec![])
                            }
                            Err(err) => {
                                log::warn!("Error handling URB: {err}");
                                UsbIpResponse::usbip_ret_submit_fail(&header)
                            }
                        }
                    }
                };

                let mut msg = Message::new();
                msg.set_usb_ip_response(message_proto::UsbIpResponse {
                    raw_response: res.to_bytes().into(),
                    ..Default::default()
                });
                allow_err!(peer.send(&msg).await);
                // res.write_to_socket(socket).await?;
                log::info!("Sent USBIP_RET_SUBMIT");
            }
            UsbIpCommand::UsbIpCmdUnlink {
                mut header,
                unlink_seqnum,
            } => {
                log::info!("Got USBIP_CMD_UNLINK for {unlink_seqnum:10x?}");

                header.command = USBIP_RET_UNLINK.into();

                let res = UsbIpResponse::usbip_ret_unlink_success(&header);
                let mut msg = Message::new();
                msg.set_usb_ip_response(message_proto::UsbIpResponse {
                    raw_response: res.to_bytes().into(),
                    ..Default::default()
                });
                allow_err!(peer.send(&msg).await);
                // res.write_to_socket(socket).await?;
                log::info!("Sent USBIP_RET_UNLINK");
            }
        }
        Ok(())
    }
}
