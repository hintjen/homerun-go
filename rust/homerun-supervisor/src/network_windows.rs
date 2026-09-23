//! One native, checked TCP/UDP snapshot. No shell, localized output, or child process.
use super::Listening;
use homerun_core::tunnel::Protocol;
use std::{
    ffi::c_void,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};
use windows_sys::Win32::{
    Foundation::ERROR_INSUFFICIENT_BUFFER,
    NetworkManagement::IpHelper::*,
    Networking::WinSock::{AF_INET, AF_INET6},
};

fn table<T: Copy>(mut read: impl FnMut(*mut c_void, *mut u32) -> u32) -> Result<Vec<T>, String> {
    let mut size = 0u32;
    let initial = read(std::ptr::null_mut(), &mut size);
    if initial != ERROR_INSUFFICIENT_BUFFER && initial != 0 {
        return Err(format!(
            "Cannot inspect game network sockets (Windows error {initial})."
        ));
    }
    for _ in 0..4 {
        if size < 4 || size > 32 * 1024 * 1024 {
            return Err("Invalid network table size.".into());
        }
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        let capacity = buffer.len() * 8;
        size = capacity as u32;
        let result = read(buffer.as_mut_ptr().cast(), &mut size);
        if result == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if result != 0 {
            return Err(format!(
                "Cannot inspect game network sockets (Windows error {result})."
            ));
        }
        // All four OWNER_PID tables have a DWORD count followed by DWORD-aligned rows.
        let ptr = buffer.as_ptr().cast::<u8>();
        let count = unsafe { ptr.cast::<u32>().read_unaligned() } as usize;
        let available = (size as usize).min(capacity);
        if available < 4 || count > (available - 4) / std::mem::size_of::<T>() {
            return Err("Incomplete network table.".into());
        }
        return Ok((0..count)
            .map(|i| unsafe {
                ptr.add(4 + i * std::mem::size_of::<T>())
                    .cast::<T>()
                    .read_unaligned()
            })
            .collect());
    }
    Err("Network sockets changed too quickly to inspect.".into())
}

pub(super) fn snapshot(pids: &[u32]) -> Result<Vec<(u32, Listening)>, String> {
    let mut out = Vec::new();
    let mut add = |pid, protocol, address, port: u32| {
        if pids.contains(&pid) {
            out.push((
                pid,
                Listening {
                    protocol,
                    address,
                    port: u16::from_be(port as u16),
                },
            ));
        }
    };
    for row in table::<MIB_TCPROW_OWNER_PID>(|p, n| unsafe {
        GetExtendedTcpTable(p, n, 0, AF_INET as u32, TCP_TABLE_OWNER_PID_LISTENER, 0)
    })? {
        add(
            row.dwOwningPid,
            Protocol::Tcp,
            IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes())),
            row.dwLocalPort,
        );
    }
    for row in table::<MIB_TCP6ROW_OWNER_PID>(|p, n| unsafe {
        GetExtendedTcpTable(p, n, 0, AF_INET6 as u32, TCP_TABLE_OWNER_PID_LISTENER, 0)
    })? {
        add(
            row.dwOwningPid,
            Protocol::Tcp,
            IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
            row.dwLocalPort,
        );
    }
    for row in table::<MIB_UDPROW_OWNER_PID>(|p, n| unsafe {
        GetExtendedUdpTable(p, n, 0, AF_INET as u32, UDP_TABLE_OWNER_PID, 0)
    })? {
        add(
            row.dwOwningPid,
            Protocol::Udp,
            IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes())),
            row.dwLocalPort,
        );
    }
    for row in table::<MIB_UDP6ROW_OWNER_PID>(|p, n| unsafe {
        GetExtendedUdpTable(p, n, 0, AF_INET6 as u32, UDP_TABLE_OWNER_PID, 0)
    })? {
        add(
            row.dwOwningPid,
            Protocol::Udp,
            IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
            row.dwLocalPort,
        );
    }
    out.sort_unstable_by_key(|(pid, l)| (*pid, l.protocol == Protocol::Udp, l.port, l.address));
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inspection_failure_is_not_an_empty_snapshot() {
        assert!(table::<MIB_TCPROW_OWNER_PID>(|_, _| 5).is_err());
        assert!(table::<MIB_TCPROW_OWNER_PID>(|_, n| {
            unsafe {
                *n = 8;
            }
            ERROR_INSUFFICIENT_BUFFER
        })
        .is_err());
    }
    #[test]
    fn native_snapshot_retains_protocol_address_and_owner() {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = std::net::UdpSocket::bind("[::]:0").unwrap();
        let tcp6 = std::net::TcpListener::bind("[::1]:0").unwrap();
        let udp4 = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let pid = std::process::id();
        let rows = snapshot(&[pid]).unwrap();
        assert!(rows.iter().any(|(owner, l)| *owner == pid
            && l.protocol == Protocol::Tcp
            && l.port == tcp.local_addr().unwrap().port()
            && l.is_confined()));
        assert!(rows.iter().any(|(owner, l)| *owner == pid
            && l.protocol == Protocol::Udp
            && l.port == udp.local_addr().unwrap().port()
            && !l.is_confined()));
        assert!(rows.iter().any(|(owner, l)| *owner == pid
            && l.protocol == Protocol::Tcp
            && l.port == tcp6.local_addr().unwrap().port()
            && l.address == "::1".parse::<IpAddr>().unwrap()));
        assert!(rows.iter().any(|(owner, l)| *owner == pid
            && l.protocol == Protocol::Udp
            && l.port == udp4.local_addr().unwrap().port()
            && l.address == "0.0.0.0".parse::<IpAddr>().unwrap()));
        assert!(snapshot(&[]).unwrap().is_empty());
    }
}
