//! Identifies Raw Input devices and reads the polling interval a USB device
//! requests from its configuration descriptor.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Registry_PropertyW, CM_Get_Device_IDW, CM_Get_Device_Interface_ListW,
    CM_Get_Device_Interface_List_SizeW, CM_Get_Parent, CM_Locate_DevNodeW, CM_DRP_ADDRESS,
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_GetManufacturerString, HidD_GetProductString,
};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::UI::Input::{GetRawInputDeviceInfoW, RIDI_DEVICENAME};

const GUID_DEVINTERFACE_USB_HUB: GUID = GUID {
    data1: 0xF18A0E88,
    data2: 0xC30C,
    data3: 0x11D0,
    data4: [0x88, 0x15, 0x00, 0xA0, 0xC9, 0x06, 0xBE, 0xD8],
};
const IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX: u32 = 0x220448;
const IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION: u32 = 0x220410;
const USB_REQUEST_HEADER: usize = 12;
const SPEED_OFFSET: usize = 23;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    Usb,
    Bluetooth,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbSpeed {
    Low,
    Full,
    High,
    Super,
}

impl UsbSpeed {
    pub fn label(self) -> &'static str {
        match self {
            UsbSpeed::Low => "Low Speed",
            UsbSpeed::Full => "Full Speed",
            UsbSpeed::High => "High Speed",
            UsbSpeed::Super => "SuperSpeed",
        }
    }
}

/// The interrupt IN endpoint a device asks the host to poll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsbEndpoint {
    pub interval_us: f64,
    pub speed: UsbSpeed,
}

impl UsbEndpoint {
    pub fn rate_hz(&self) -> f64 {
        1e6 / self.interval_us
    }
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub vid_pid: Option<(u16, u16)>,
    pub connection: Connection,
    pub endpoint: Option<UsbEndpoint>,
}

impl DeviceInfo {
    pub fn injected() -> Self {
        Self {
            name: "Injected input".into(),
            vid_pid: None,
            connection: Connection::Other,
            endpoint: None,
        }
    }

    /// Identifies a device from its Raw Input handle.
    pub fn from_handle(handle: usize) -> Self {
        if handle == 0 {
            return Self::injected();
        }
        let path = raw_device_path(handle).unwrap_or_default();
        let vid_pid = parse_vid_pid(&path);
        let name = hid_name(&path).unwrap_or_else(|| match vid_pid {
            Some((vid, pid)) => format!("Device {vid:04X}:{pid:04X}"),
            None => "Unknown device".into(),
        });
        let (connection, endpoint) = usb_lookup(&path);
        Self { name, vid_pid, connection, endpoint }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub fn raw_device_path(handle: usize) -> Option<String> {
    let mut len = 0u32;
    unsafe {
        GetRawInputDeviceInfoW(handle as HANDLE, RIDI_DEVICENAME, null_mut(), &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let read = GetRawInputDeviceInfoW(handle as HANDLE, RIDI_DEVICENAME, buf.as_mut_ptr().cast(), &mut len);
        (read != u32::MAX).then(|| from_wide(&buf))
    }
}

/// Extracts VID and PID from a device path or instance ID.
pub fn parse_vid_pid(path: &str) -> Option<(u16, u16)> {
    let upper = path.to_ascii_uppercase();
    let field = |tag: &str| {
        let at = upper.find(tag)? + tag.len();
        u16::from_str_radix(upper.get(at..at + 4)?, 16).ok()
    };
    if let (Some(vid), Some(pid)) = (field("VID_"), field("PID_")) {
        return Some((vid, pid));
    }
    // Bluetooth LE paths use VID&<source><vid>_PID&<pid>.
    let vid = upper.find("VID&").and_then(|i| upper.get(i + 6..i + 10)).and_then(|s| u16::from_str_radix(s, 16).ok())?;
    let pid = upper.find("PID&").and_then(|i| upper.get(i + 4..i + 8)).and_then(|s| u16::from_str_radix(s, 16).ok())?;
    Some((vid, pid))
}

/// Converts `\\?\HID#VID_1&PID_2#3#{guid}` to the instance ID `HID\VID_1&PID_2\3`.
pub fn instance_id_from_path(path: &str) -> Option<String> {
    let body = path.strip_prefix(r"\\?\").or_else(|| path.strip_prefix(r"\??\"))?;
    let end = body.rfind("#{")?;
    Some(body[..end].replace('#', "\\"))
}

fn hid_name(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let wpath = wide(path);
    unsafe {
        let file = CreateFileW(
            wpath.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        );
        if file == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut product = [0u16; 127];
        let mut maker = [0u16; 127];
        let got_product = HidD_GetProductString(file, product.as_mut_ptr().cast(), 254) != 0;
        let got_maker = HidD_GetManufacturerString(file, maker.as_mut_ptr().cast(), 254) != 0;
        CloseHandle(file);
        let product = got_product.then(|| from_wide(&product)).unwrap_or_default();
        let maker = got_maker.then(|| from_wide(&maker)).unwrap_or_default();
        match (product.trim(), maker.trim()) {
            ("", _) => None,
            (p, m) if m.is_empty() || p.to_lowercase().contains(&m.to_lowercase()) => Some(p.to_string()),
            (p, m) => Some(format!("{m} {p}")),
        }
    }
}

fn device_id(node: u32) -> Option<String> {
    let mut buf = [0u16; 512];
    let ok = unsafe { CM_Get_Device_IDW(node, buf.as_mut_ptr(), buf.len() as u32, 0) };
    (ok == CR_SUCCESS).then(|| from_wide(&buf))
}

fn parent(node: u32) -> Option<u32> {
    let mut up = 0;
    (unsafe { CM_Get_Parent(&mut up, node, 0) } == CR_SUCCESS).then_some(up)
}

/// Walks up the device tree from a HID collection to find how it is connected
/// and, for USB, its endpoint descriptor.
fn usb_lookup(path: &str) -> (Connection, Option<UsbEndpoint>) {
    let Some(instance) = instance_id_from_path(path) else {
        return (Connection::Other, None);
    };
    let wid = wide(&instance);
    let mut node = 0;
    if unsafe { CM_Locate_DevNodeW(&mut node, wid.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) } != CR_SUCCESS {
        return (Connection::Other, None);
    }

    let mut interface = None;
    let mut current = node;
    for _ in 0..16 {
        let Some(id) = device_id(current) else { break };
        let upper = id.to_ascii_uppercase();
        if upper.starts_with("BTH") {
            return (Connection::Bluetooth, None);
        }
        if upper.starts_with("USB\\") {
            match parse_interface_number(&upper) {
                Some(n) => interface = Some(n),
                None => return (Connection::Usb, read_endpoint(current, interface)),
            }
        }
        match parent(current) {
            Some(up) => current = up,
            None => break,
        }
    }
    (Connection::Other, None)
}

/// Finds the interface number in `USB\VID_x&PID_y&MI_02\...`.
pub fn parse_interface_number(id: &str) -> Option<u8> {
    let at = id.find("&MI_")? + 4;
    u8::from_str_radix(id.get(at..at + 2)?, 16).ok()
}

fn read_endpoint(device_node: u32, interface: Option<u8>) -> Option<UsbEndpoint> {
    let mut port = 0u32;
    let mut size = size_of::<u32>() as u32;
    let ok = unsafe {
        CM_Get_DevNode_Registry_PropertyW(
            device_node,
            CM_DRP_ADDRESS,
            null_mut(),
            (&mut port as *mut u32).cast(),
            &mut size,
            0,
        )
    };
    if ok != CR_SUCCESS {
        return None;
    }
    let hub = open_hub(parent(device_node)?)?;
    let result = (|| {
        let speed = connection_speed(hub, port)?;
        let config = config_descriptor(hub, port)?;
        parse_hid_endpoint(&config, interface, speed)
    })();
    unsafe { CloseHandle(hub) };
    result
}

fn open_hub(hub_node: u32) -> Option<HANDLE> {
    let id = wide(&device_id(hub_node)?);
    let mut len = 0u32;
    unsafe {
        let got = CM_Get_Device_Interface_List_SizeW(
            &mut len,
            &GUID_DEVINTERFACE_USB_HUB,
            id.as_ptr(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        );
        if got != CR_SUCCESS || len <= 1 {
            return None;
        }
        let mut list = vec![0u16; len as usize];
        let got = CM_Get_Device_Interface_ListW(
            &GUID_DEVINTERFACE_USB_HUB,
            id.as_ptr(),
            list.as_mut_ptr(),
            len,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        );
        if got != CR_SUCCESS {
            return None;
        }
        let path = wide(&from_wide(&list));
        let file = CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        );
        (file != INVALID_HANDLE_VALUE).then_some(file)
    }
}

fn ioctl(hub: HANDLE, code: u32, buf: &mut [u8]) -> Option<usize> {
    let mut returned = 0;
    let len = buf.len() as u32;
    let ptr = buf.as_mut_ptr() as *mut c_void;
    let ok = unsafe { DeviceIoControl(hub, code, ptr, len, ptr, len, &mut returned, null_mut()) };
    (ok != 0).then_some(returned as usize)
}

fn connection_speed(hub: HANDLE, port: u32) -> Option<UsbSpeed> {
    let mut buf = [0u8; 512];
    buf[..4].copy_from_slice(&port.to_le_bytes());
    let returned = ioctl(hub, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX, &mut buf)?;
    if returned <= SPEED_OFFSET {
        return None;
    }
    match buf[SPEED_OFFSET] {
        0 => Some(UsbSpeed::Low),
        1 => Some(UsbSpeed::Full),
        2 => Some(UsbSpeed::High),
        3 | 4 => Some(UsbSpeed::Super),
        _ => None,
    }
}

fn request_config(hub: HANDLE, port: u32, length: u16) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; USB_REQUEST_HEADER + length as usize];
    buf[..4].copy_from_slice(&port.to_le_bytes());
    buf[4] = 0x80; // device to host, standard, device
    buf[5] = 6; // GET_DESCRIPTOR
    buf[6..8].copy_from_slice(&0x0200u16.to_le_bytes()); // configuration, index 0
    buf[10..12].copy_from_slice(&length.to_le_bytes());
    let returned = ioctl(hub, IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION, &mut buf)?;
    (returned > USB_REQUEST_HEADER).then(|| buf[USB_REQUEST_HEADER..returned].to_vec())
}

fn config_descriptor(hub: HANDLE, port: u32) -> Option<Vec<u8>> {
    let head = request_config(hub, port, 9)?;
    let total = u16::from_le_bytes([*head.get(2)?, *head.get(3)?]);
    request_config(hub, port, total)
}

/// Finds the first interrupt IN endpoint of a HID interface in a configuration
/// descriptor. `interface` narrows the search on composite devices.
pub fn parse_hid_endpoint(config: &[u8], interface: Option<u8>, speed: UsbSpeed) -> Option<UsbEndpoint> {
    const INTERFACE: u8 = 4;
    const ENDPOINT: u8 = 5;
    const HID_CLASS: u8 = 3;

    let mut in_match = false;
    let mut at = 0;
    while at + 2 <= config.len() {
        let len = config[at] as usize;
        if len < 2 || at + len > config.len() {
            return None;
        }
        let desc = &config[at..at + len];
        match desc[1] {
            INTERFACE if len >= 9 => {
                let number_ok = interface.map_or(true, |n| n == desc[2]);
                in_match = number_ok && desc[3] == 0 && desc[5] == HID_CLASS;
            }
            ENDPOINT if in_match && len >= 7 => {
                let is_in = desc[2] & 0x80 != 0;
                let is_interrupt = desc[3] & 0x03 == 3;
                if is_in && is_interrupt && desc[6] > 0 {
                    return Some(UsbEndpoint { interval_us: interval_us(desc[6], speed), speed });
                }
            }
            _ => {}
        }
        at += len;
    }
    None
}

/// Low and Full Speed count bInterval in milliseconds; High Speed and faster
/// use 2^(bInterval-1) microframes of 125 microseconds.
pub fn interval_us(b_interval: u8, speed: UsbSpeed) -> f64 {
    match speed {
        UsbSpeed::Low | UsbSpeed::Full => b_interval as f64 * 1000.0,
        UsbSpeed::High | UsbSpeed::Super => 125.0 * 2f64.powi((b_interval.clamp(1, 16) - 1) as i32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vid_pid_from_usb_and_ble_paths() {
        let usb = r"\\?\HID#VID_046D&PID_C08B&MI_01&Col01#8&abc#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        assert_eq!(parse_vid_pid(usb), Some((0x046D, 0xC08B)));
        let ble = r"\\?\HID#{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&02046d_PID&b023_REV&0007_e4b1#9&1#{guid}";
        assert_eq!(parse_vid_pid(ble), Some((0x046D, 0xB023)));
        assert_eq!(parse_vid_pid(r"\\?\Root#RDP_MOU#0000#{guid}"), None);
    }

    #[test]
    fn converts_path_to_instance_id() {
        let path = r"\\?\HID#VID_046D&PID_C08B&MI_00#8&2f&0&0000#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        assert_eq!(instance_id_from_path(path).unwrap(), r"HID\VID_046D&PID_C08B&MI_00\8&2f&0&0000");
        assert_eq!(instance_id_from_path("nonsense"), None);
    }

    #[test]
    fn parses_interface_number() {
        assert_eq!(parse_interface_number(r"USB\VID_1&PID_2&MI_02\X"), Some(2));
        assert_eq!(parse_interface_number(r"USB\VID_1&PID_2\X"), None);
    }

    #[test]
    fn converts_b_interval_by_speed() {
        assert_eq!(interval_us(8, UsbSpeed::Low), 8000.0);
        assert_eq!(interval_us(1, UsbSpeed::Full), 1000.0);
        assert_eq!(interval_us(1, UsbSpeed::High), 125.0);
        assert_eq!(interval_us(4, UsbSpeed::High), 1000.0);
    }

    fn sample_config() -> Vec<u8> {
        let mut c = vec![9, 2, 0, 0, 2, 1, 0, 0xA0, 50];
        // Interface 0: HID keyboard, endpoint 0x81, bInterval 8.
        c.extend([9, 4, 0, 0, 1, 3, 1, 1, 0]);
        c.extend([9, 0x21, 0x11, 1, 0, 1, 0x22, 64, 0]);
        c.extend([7, 5, 0x81, 3, 8, 0, 8]);
        // Interface 1: HID, an OUT endpoint then IN endpoint 0x82, bInterval 1.
        c.extend([9, 4, 1, 0, 2, 3, 0, 0, 0]);
        c.extend([7, 5, 0x02, 3, 64, 0, 1]);
        c.extend([7, 5, 0x82, 3, 64, 0, 1]);
        c
    }

    #[test]
    fn finds_first_hid_endpoint_without_interface_number() {
        let found = parse_hid_endpoint(&sample_config(), None, UsbSpeed::Full).unwrap();
        assert_eq!(found.interval_us, 8000.0);
    }

    #[test]
    fn honours_interface_number_and_skips_out_endpoints() {
        let found = parse_hid_endpoint(&sample_config(), Some(1), UsbSpeed::Full).unwrap();
        assert_eq!(found.rate_hz(), 1000.0);
    }

    #[test]
    fn rejects_truncated_descriptors() {
        let mut bad = sample_config();
        bad.truncate(20);
        assert_eq!(parse_hid_endpoint(&bad, None, UsbSpeed::Full), None);
    }
}
