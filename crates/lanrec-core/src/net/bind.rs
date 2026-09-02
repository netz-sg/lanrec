//! Forcing traffic onto one specific adapter.
//!
//! Connecting a socket normally leaves the choice of outgoing interface to the
//! routing table. That is fine when only one route exists, and quietly wrong when
//! two do: with a direct cable *and* a router link present, a stream meant for
//! the cable can end up going through the house network, where it competes with
//! everything else and never reaches the rate the cable could carry.
//!
//! Two mechanisms are used together, because neither alone is airtight:
//!
//! 1. Binding the socket to the adapter's own address before connecting. Windows
//!    uses the strong host model for sending, so a socket with that source
//!    address goes out that adapter.
//! 2. `IP_UNICAST_IF`, which pins the outgoing interface explicitly and outranks
//!    the routing table.
//!
//! The caller is expected to check `local_addr()` afterwards. Neither mechanism
//! is worth trusting silently.

use std::os::windows::io::RawSocket;

use anyhow::{Result, bail};
use windows::Win32::Networking::WinSock::{IPPROTO_IP, SOCKET, SOCKET_ERROR, setsockopt};

/// Socket option that pins the outgoing interface for IPv4.
///
/// Not exposed by the windows crate's constant set, and its value is fixed by
/// the Winsock headers.
const IP_UNICAST_IF: i32 = 31;

/// Pin an existing socket to the adapter with this interface index.
///
/// `if_index` is the `IfIndex` reported by [`crate::net::nic`].
pub fn pin_to_interface(socket: RawSocket, if_index: u32) -> Result<()> {
    if if_index == 0 {
        bail!("Interface-Index 0 ist ungueltig");
    }

    // Documented quirk: unlike almost every other socket option, IP_UNICAST_IF
    // takes the interface index in network byte order for IPv4.
    let value = if_index.to_be().to_ne_bytes();

    let rc = unsafe {
        setsockopt(
            SOCKET(socket as usize),
            IPPROTO_IP.0,
            IP_UNICAST_IF,
            Some(&value),
        )
    };

    if rc == SOCKET_ERROR {
        bail!(
            "IP_UNICAST_IF auf Interface {if_index} fehlgeschlagen (WSA-Fehler {})",
            unsafe { windows::Win32::Networking::WinSock::WSAGetLastError().0 }
        );
    }
    Ok(())
}
