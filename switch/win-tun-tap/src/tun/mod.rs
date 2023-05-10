use std::io;
use std::net::Ipv4Addr;

use winapi::um::{handleapi, synchapi, winbase, winnt};

use crate::{decode_utf16, encode_utf16, ffi, IFace, netsh, route};
mod wintun_raw;
mod log;
pub mod packet;

/// The maximum size of wintun's internal ring buffer (in bytes)
pub const MAX_RING_CAPACITY: u32 = 0x400_0000;

/// The minimum size of wintun's internal ring buffer (in bytes)
pub const MIN_RING_CAPACITY: u32 = 0x2_0000;

/// Maximum pool name length including zero terminator
pub const MAX_POOL: usize = 256;


pub struct TunDevice {
    /// The session handle given to us by WintunStartSession
    pub(crate) session: wintun_raw::WINTUN_SESSION_HANDLE,

    /// Shared dll for required wintun driver functions
    pub(crate) win_tun: wintun_raw::wintun,

    /// Windows event handle that is signaled by the wintun driver when data becomes available to
    /// read
    pub(crate) read_event: winnt::HANDLE,

    /// Windows event handle that is signaled when [`TunSession::shutdown`] is called force blocking
    /// readers to exit
    pub(crate) shutdown_event: winnt::HANDLE,

    /// The adapter that owns this session
    pub(crate) adapter: wintun_raw::WINTUN_ADAPTER_HANDLE,

}

unsafe impl Send for TunDevice {}

unsafe impl Sync for TunDevice {}
winapi::DEFINE_GUID! {
    GUID_NETWORK_ADAPTER,
    0x4d36e972, 0xe325, 0x11ce,
    0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18
}
impl TunDevice {
    pub unsafe fn create<L>(library: L, pool: &str, name: &str) -> io::Result<Self>
        where L: Into<libloading::Library>, {
        let win_tun = match wintun_raw::wintun::from_library(library) {
            Ok(win_tun) => { win_tun }
            Err(e) => {
                return Err(io::Error::new(io::ErrorKind::Other, format!("library error {:?} ", e)));
            }
        };
        let pool_utf16 = encode_utf16(pool);
        if pool_utf16.len() > MAX_POOL {
            return Err(io::Error::new(io::ErrorKind::Other, format!("长度大于{}:{:?}", MAX_POOL, pool)));
        }
        let name_utf16 = encode_utf16(name);
        if name_utf16.len() > MAX_POOL {
            return Err(io::Error::new(io::ErrorKind::Other, format!("长度大于{}:{:?}", MAX_POOL, pool)));
        }
        //SAFETY: guid is a unique integer so transmuting either all zeroes or the user's preferred
        //guid to the winapi guid type is safe and will allow the windows kernel to see our GUID
        let guid_struct: wintun_raw::GUID = unsafe { std::mem::transmute(GUID_NETWORK_ADAPTER) };
        let guid_ptr = &guid_struct as *const wintun_raw::GUID;

        log::set_default_logger_if_unset(&win_tun);

        //SAFETY: the function is loaded from the wintun dll properly, we are providing valid
        //pointers, and all the strings are correct null terminated UTF-16. This safety rationale
        //applies for all Wintun* functions below
        let adapter = win_tun.WintunCreateAdapter(pool_utf16.as_ptr(), name_utf16.as_ptr(), guid_ptr);
        if adapter.is_null() {
            return Err(io::Error::new(io::ErrorKind::Other, "Failed to crate adapter"));
        }
        Self::init(win_tun, adapter)
    }
    pub unsafe fn init(win_tun: wintun_raw::wintun, adapter: wintun_raw::WINTUN_ADAPTER_HANDLE) -> io::Result<Self> {
        // 开启session
        let session = win_tun.WintunStartSession(adapter, 128 * 1024);
        if session.is_null() {
            return Err(io::Error::new(io::ErrorKind::Other, "WintunStartSession failed"));
        }
        //SAFETY: We follow the contract required by CreateEventA. See MSDN
        //(the pointers are allowed to be null, and 0 is okay for the others)
        let shutdown_event = synchapi::CreateEventA(std::ptr::null_mut(),
                                                    0, 0, std::ptr::null_mut());
        let read_event = win_tun.WintunGetReadWaitEvent(session) as winnt::HANDLE;

        Ok(TunDevice {
            session,
            win_tun,
            read_event,
            shutdown_event,
            adapter,
        })
    }
    pub unsafe fn open<L>(library: L, name: &str) -> io::Result<Self>
        where L: Into<libloading::Library>, {
        let win_tun = match wintun_raw::wintun::from_library(library) {
            Ok(win_tun) => win_tun,
            Err(e) => {
                return Err(io::Error::new(io::ErrorKind::Other, format!("library error {:?} ", e)));
            }
        };
        log::set_default_logger_if_unset(&win_tun);
        let name_utf16 = encode_utf16(name);
        let adapter = win_tun.WintunOpenAdapter(name_utf16.as_ptr());
        if adapter.is_null() {
            return Err(io::Error::new(io::ErrorKind::Other, "Failed to open adapter"));
        }
        Self::init(win_tun, adapter)
    }
    pub fn delete(self) -> io::Result<()> {
        drop(self);
        Ok(())
    }
    pub fn version(&self) -> io::Result<Version> {
        let version = unsafe { self.win_tun.WintunGetRunningDriverVersion() };
        if version == 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "WintunGetRunningDriverVersion"));
        } else {
            Ok(Version {
                major: ((version >> 16) & 0xFF) as u16,
                minor: (version & 0xFF) as u16,
            })
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
}

impl TunDevice {
    fn get_adapter_luid(&self) -> u64 {
        let mut luid: wintun_raw::NET_LUID = unsafe { std::mem::zeroed() };
        unsafe { self.win_tun.WintunGetAdapterLUID(self.adapter, &mut luid as *mut wintun_raw::NET_LUID) };
        unsafe { std::mem::transmute(luid) }
    }
}

impl IFace for TunDevice {
    fn shutdown(&self) -> io::Result<()> {
        let _ = unsafe { synchapi::SetEvent(self.shutdown_event) };
        let _ = unsafe { handleapi::CloseHandle(self.shutdown_event) };
        Ok(())
    }

    fn get_index(&self) -> io::Result<u32> {
        let luid = self.get_adapter_luid();
        ffi::luid_to_index(&unsafe { std::mem::transmute(luid) }).map(|index| index as u32)
    }

    fn get_name(&self) -> io::Result<String> {
        let luid = self.get_adapter_luid();
        ffi::luid_to_alias(&unsafe { std::mem::transmute(luid) }).map(|name| {
            decode_utf16(&name)
        })
    }

    fn set_name(&self, new_name: &str) -> io::Result<()> {
        let name = self.get_name()?;
        netsh::set_interface_name(&name, new_name)
    }

    fn set_ip<IP>(&self, address: IP, mask: IP) -> io::Result<()> where IP: Into<Ipv4Addr> {
        netsh::set_interface_ip(self.get_index()?, &address.into(), &mask.into())
    }

    fn add_route<IP>(&self, dest: IP, netmask: IP, gateway: IP) -> io::Result<()> where IP: Into<Ipv4Addr> {
        route::add_route(self.get_index()?, dest.into(), netmask.into(), gateway.into())
    }

    fn delete_route<IP>(&self, dest: IP, netmask: IP, gateway: IP) -> io::Result<()> where IP: Into<Ipv4Addr> {
        route::delete_route(self.get_index()?, dest.into(), netmask.into(), gateway.into())
    }

    fn set_mtu(&self, mtu: u16) -> io::Result<()> {
        netsh::set_interface_mtu(self.get_index()?, mtu)
    }
}

impl TunDevice {
    pub fn try_receive(&self) -> io::Result<Option<packet::TunPacket>> {
        let mut size = 0u32;

        let bytes_ptr = unsafe {
            self.win_tun
                .WintunReceivePacket(self.session, &mut size as *mut u32)
        };

        debug_assert!(size <= u16::MAX as u32);
        if bytes_ptr.is_null() {
            //Wintun returns ERROR_NO_MORE_ITEMS instead of blocking if packets are not available
            let last_error = unsafe { winapi::um::errhandlingapi::GetLastError() };
            if last_error == winapi::shared::winerror::ERROR_NO_MORE_ITEMS {
                Ok(None)
            } else {
                Err(io::Error::new(io::ErrorKind::Other, "try_receive failed"))
            }
        } else {
            Ok(Some(packet::TunPacket {
                kind: packet::Kind::ReceivePacket,
                size: size as usize,
                //SAFETY: ptr is non null, aligned for u8, and readable for up to size bytes (which
                //must be less than isize::MAX because bytes is a u16
                bytes_ptr,
                tun_device: Some(&self),
            }))
        }
    }
    pub fn receive_blocking(&self) -> io::Result<packet::TunPacket> {
        loop {
            //Try 5 times to receive without blocking so we don't have to issue a syscall to wait
            //for the event if packets are being received at a rapid rate
            for _ in 0..5 {
                match self.try_receive()? {
                    None => {
                        continue;
                    }
                    Some(packet) => {
                        return Ok(packet);
                    }
                }
            }
            //Wait on both the read handle and the shutdown handle so that we stop when requested
            let handles = [self.read_event, self.shutdown_event];
            let result = unsafe {
                //SAFETY: We abide by the requirements of WaitForMultipleObjects, handles is a
                //pointer to valid, aligned, stack memory
                synchapi::WaitForMultipleObjects(
                    2,
                    &handles as *const winnt::HANDLE,
                    0,
                    winbase::INFINITE,
                )
            };
            match result {
                winbase::WAIT_FAILED => return Err(io::Error::new(io::ErrorKind::Other, "WAIT_FAILED")),
                _ => {
                    if result == winbase::WAIT_OBJECT_0 {
                        //We have data!
                        continue;
                    } else if result == winbase::WAIT_OBJECT_0 + 1 {
                        //Shutdown event triggered
                        return Err(io::Error::new(io::ErrorKind::Other, "Shutdown event triggered"));
                    }
                }
            }
        }
    }
}

impl TunDevice {
    pub fn allocate_send_packet(&self, size: u16) -> io::Result<packet::TunPacket> {
        let bytes_ptr = unsafe {
            self.win_tun.WintunAllocateSendPacket(self.session, size as u32)
        };
        if bytes_ptr.is_null() {
            Err(io::Error::new(io::ErrorKind::Other, "allocate_send_packet failed"))
        } else {
            Ok(packet::TunPacket {
                kind: packet::Kind::SendPacketPending,
                size: size as usize,
                //SAFETY: ptr is non null, aligned for u8, and readable for up to size bytes (which
                //must be less than isize::MAX because bytes is a u16
                bytes_ptr,
                tun_device: None,
            })
        }
    }
    pub fn send_packet(&self, mut packet: packet::TunPacket) {
        assert!(matches!(packet.kind, packet::Kind::SendPacketPending));

        unsafe {
            self.win_tun
                .WintunSendPacket(self.session, packet.bytes_ptr)
        };
        //Mark the packet at sent
        packet.kind = packet::Kind::SendPacketSent;
    }
}


impl Drop for TunDevice {
    fn drop(&mut self) {
        //Close adapter on drop
        //This is why we need an Arc of wintun
        unsafe {
            self.win_tun.WintunCloseAdapter(self.adapter);
            self.win_tun.WintunDeleteDriver()
        };
    }
}