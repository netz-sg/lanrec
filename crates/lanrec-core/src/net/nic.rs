//! Network interface discovery.
//!
//! The tool needs to answer three questions before a recording can start: which
//! adapters exist, which of them actually has a cable in it, and whether the link
//! is fast enough to carry the stream. Windows exposes all of that through
//! `GetAdaptersAddresses`, including negotiated link speed and MTU -- which
//! matters here, because a gigabit adapter that negotiated 100 Mbit/s (bad cable,
//! wrong port) looks completely normal until the recording starts dropping.

use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::config::Labels;
use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211,
    IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
};

/// Physical medium of an adapter, as far as we care about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Medium {
    Ethernet,
    WiFi,
    Loopback,
    Other,
}

/// How well suited an adapter is for carrying the capture stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Suitability {
    /// Wired, link up, >= 1 Gbit/s negotiated.
    Good,
    /// Usable but compromised -- see the attached reason.
    Marginal,
    /// Cannot carry the stream.
    Unusable,
}

#[derive(Debug, Clone)]
pub struct Nic {
    pub index: u32,
    /// The name the user sees in Windows, e.g. "Ethernet 2".
    pub name: String,
    /// Hardware description, e.g. "Realtek Gaming 2.5GbE Family Controller".
    pub description: String,
    pub mac: Option<[u8; 6]>,
    pub medium: Medium,
    /// Link is up -- for wired adapters this means a cable is plugged in at both
    /// ends and the PHYs have negotiated.
    pub up: bool,
    /// Negotiated transmit rate in bit/s. Only meaningful while `up`.
    pub link_speed_bps: u64,
    pub mtu: u32,
    pub ipv4: Vec<Ipv4Addr>,
    pub ipv6: Vec<Ipv6Addr>,
    /// True when the adapter has no default gateway, which is what a direct
    /// machine-to-machine cable looks like.
    pub has_gateway: bool,
}

impl Nic {
    /// Jumbo frames noticeably reduce interrupt load at a few hundred Mbit/s.
    pub fn jumbo_frames(&self) -> bool {
        self.mtu >= 9000
    }

    pub fn suitability(&self) -> (Suitability, Option<&'static str>) {
        if self.medium == Medium::Loopback {
            return (Suitability::Unusable, Some("Loopback-Adapter"));
        }
        if !self.up {
            return (
                Suitability::Unusable,
                Some("kein Link - Kabel steckt nicht"),
            );
        }
        if self.medium == Medium::WiFi {
            return (
                Suitability::Marginal,
                Some("WLAN - Jitter und Paketverlust machen die Aufnahme unbrauchbar"),
            );
        }
        if self.link_speed_bps < 1_000_000_000 {
            return (
                Suitability::Marginal,
                Some("unter 1 Gbit/s ausgehandelt - Kabel oder Port pruefen"),
            );
        }
        if self.ipv4.is_empty() {
            return (
                Suitability::Marginal,
                Some("keine IPv4-Adresse konfiguriert"),
            );
        }
        if !self.jumbo_frames() {
            return (
                Suitability::Good,
                Some("MTU 1500 - Jumbo Frames wuerden die Interrupt-Last senken"),
            );
        }
        (Suitability::Good, None)
    }

    /// Rough headroom check against an expected stream bitrate.
    ///
    /// Real-world throughput on a saturated link never reaches the nominal rate,
    /// so this budgets 85 %.
    pub fn can_carry(&self, bitrate_bps: u64) -> bool {
        self.up && (self.link_speed_bps as f64 * 0.85) as u64 >= bitrate_bps
    }
}

/// Enumerate all network adapters known to Windows.
pub fn enumerate() -> Result<Vec<Nic>> {
    // GetAdaptersAddresses wants a single buffer holding a linked list of
    // variable-length records, so the size has to be asked for first. The list can
    // change between the two calls, hence the retry.
    // INCLUDE_GATEWAYS is not optional: without it FirstGatewayAddress stays null
    // on every adapter, and the "no gateway, so this is a direct link" heuristic
    // silently reports true for all of them.
    let flags = GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER
        | GAA_FLAG_INCLUDE_GATEWAYS;
    let family = AF_UNSPEC.0 as u32;

    let mut size: u32 = 0;
    let mut buf: Vec<u8> = Vec::new();

    for _ in 0..3 {
        let rc = unsafe { GetAdaptersAddresses(family, flags, None, None, &mut size) };
        if WIN32_ERROR(rc) != ERROR_BUFFER_OVERFLOW && WIN32_ERROR(rc) != ERROR_SUCCESS {
            bail!("GetAdaptersAddresses (size query) failed: {rc}");
        }
        if size == 0 {
            return Ok(Vec::new());
        }

        buf.clear();
        buf.resize(size as usize, 0);

        let rc = unsafe {
            GetAdaptersAddresses(
                family,
                flags,
                None,
                Some(buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &mut size,
            )
        };
        match WIN32_ERROR(rc) {
            ERROR_SUCCESS => return Ok(unsafe { walk(buf.as_ptr().cast()) }),
            ERROR_BUFFER_OVERFLOW => continue, // adapter list grew; retry with new size
            _ => bail!("GetAdaptersAddresses failed: {rc}"),
        }
    }

    bail!("GetAdaptersAddresses kept growing its buffer -- adapter list unstable")
}

/// Walk the linked list the API filled into our buffer.
///
/// # Safety
/// `head` must point at a list written by a successful `GetAdaptersAddresses`,
/// and the backing buffer must outlive this call.
unsafe fn walk(head: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<Nic> {
    let mut out = Vec::new();
    let mut cur = head;

    while !cur.is_null() {
        let a = unsafe { &*cur };

        let mac = if a.PhysicalAddressLength >= 6 {
            let mut m = [0u8; 6];
            m.copy_from_slice(&a.PhysicalAddress[..6]);
            Some(m)
        } else {
            None
        };

        let medium = match a.IfType {
            IF_TYPE_ETHERNET_CSMACD => Medium::Ethernet,
            IF_TYPE_IEEE80211 => Medium::WiFi,
            IF_TYPE_SOFTWARE_LOOPBACK => Medium::Loopback,
            _ => Medium::Other,
        };

        let (mut ipv4, mut ipv6) = (Vec::new(), Vec::new());
        let mut ua = a.FirstUnicastAddress;
        while !ua.is_null() {
            let sa = unsafe { (*ua).Address.lpSockaddr };
            if !sa.is_null() {
                match unsafe { (*sa).sa_family } {
                    AF_INET => {
                        let v4 = unsafe { &*sa.cast::<SOCKADDR_IN>() };
                        ipv4.push(Ipv4Addr::from(u32::from_be(unsafe {
                            v4.sin_addr.S_un.S_addr
                        })));
                    }
                    AF_INET6 => {
                        let v6 = unsafe { &*sa.cast::<SOCKADDR_IN6>() };
                        ipv6.push(Ipv6Addr::from(unsafe { v6.sin6_addr.u.Byte }));
                    }
                    _ => {}
                }
            }
            ua = unsafe { (*ua).Next };
        }

        out.push(Nic {
            index: unsafe { a.Anonymous1.Anonymous.IfIndex },
            name: unsafe { wide_to_string(a.FriendlyName.0) },
            description: unsafe { wide_to_string(a.Description.0) },
            mac,
            medium,
            up: a.OperStatus == IfOperStatusUp,
            link_speed_bps: a.TransmitLinkSpeed,
            mtu: a.Mtu,
            ipv4,
            ipv6,
            has_gateway: !a.FirstGatewayAddress.is_null(),
        });

        cur = a.Next;
    }

    out
}

/// # Safety
/// `p` must be null or a null-terminated UTF-16 string.
unsafe fn wide_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

/// Format a link speed the way a network dialog would.
pub fn format_speed(bps: u64) -> String {
    match bps {
        0 => "-".into(),
        b if b >= 1_000_000_000 => format!("{:.0} Gbit/s", b as f64 / 1e9),
        b if b >= 1_000_000 => format!("{:.0} Mbit/s", b as f64 / 1e6),
        b => format!("{b} bit/s"),
    }
}

/// Flattened view of a [`Nic`] for the UI.
///
/// Kept separate from `Nic` so the wire format the frontend depends on does not
/// drift every time the capture side needs another field, and so everything the
/// UI renders -- labels, verdicts, formatting -- is decided once here rather than
/// reimplemented in TypeScript.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NicView {
    pub index: u32,
    /// The name Windows gives the adapter, e.g. "Ethernet 2".
    pub name: String,
    /// The name the user gave it, if any.
    pub label: Option<String>,
    /// What to actually show: the user's name when set, otherwise Windows'.
    pub display_name: String,
    pub description: String,
    pub mac: Option<String>,
    pub medium: Medium,
    pub up: bool,
    pub link_speed_bps: u64,
    pub link_speed_label: String,
    pub mtu: u32,
    pub jumbo: bool,
    pub ipv4: Vec<String>,
    pub has_gateway: bool,
    pub suitability: Suitability,
    pub note: Option<String>,
    /// Wired adapter with no default gateway -- the shape of a machine-to-machine
    /// cable rather than a route to the rest of the world.
    pub direct_link_candidate: bool,
}

impl NicView {
    fn build(n: &Nic, labels: &Labels) -> Self {
        let (suitability, note) = n.suitability();
        let mac = n.mac.map(|m| {
            m.iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(":")
        });
        let label = mac
            .as_deref()
            .and_then(|m| labels.get(m))
            .map(str::to_string);

        Self {
            index: n.index,
            name: n.name.clone(),
            display_name: label.clone().unwrap_or_else(|| n.name.clone()),
            label,
            description: n.description.clone(),
            mac,
            medium: n.medium,
            up: n.up,
            link_speed_bps: n.link_speed_bps,
            link_speed_label: format_speed(n.link_speed_bps),
            mtu: n.mtu,
            jumbo: n.jumbo_frames(),
            ipv4: n.ipv4.iter().map(|i| i.to_string()).collect(),
            has_gateway: n.has_gateway,
            suitability,
            note: note.map(str::to_string),
            direct_link_candidate: n.medium == Medium::Ethernet && !n.has_gateway,
        }
    }
}

/// Adapters as the UI wants them: physical only, most useful first, with any
/// user-chosen names applied.
pub fn enumerate_view(labels: &Labels) -> Result<Vec<NicView>> {
    let mut v: Vec<NicView> = enumerate()?
        .iter()
        .filter(|n| n.medium != Medium::Loopback)
        .map(|n| NicView::build(n, labels))
        .collect();

    // A live link outranks a dead one, then the faster link, then a stable name
    // so the list does not reshuffle on every poll.
    v.sort_by(|a, b| {
        b.up.cmp(&a.up)
            .then(b.link_speed_bps.cmp(&a.link_speed_bps))
            .then(a.name.cmp(&b.name))
    });
    Ok(v)
}
