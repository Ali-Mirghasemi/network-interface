use std::ffi::c_void;
use std::mem::size_of;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ptr::null_mut;
use std::slice::from_raw_parts;

use libc::{free, malloc, wchar_t, wcslen};
use winapi::{
    shared::{
        ws2def::{AF_UNSPEC, SOCKADDR_IN},
        ws2ipdef::SOCKADDR_IN6,
        netioapi::ConvertLengthToIpv4Mask,
        ntdef::ULONG,
    },
    um::{
        iptypes::{IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_UNICAST_ADDRESS_LH},
        iphlpapi::GetAdaptersAddresses,
    },
};

use crate::utils::hex::HexSlice;
use crate::{Error, NetworkInterface, NetworkInterfaceConfig, Result};
use crate::interface::Netmask;

/// An alias for `IP_ADAPTER_ADDRESSES_LH`
type AdapterAddress = IP_ADAPTER_ADDRESSES_LH;

/// The buffer size indicated by the `SizePointer` parameter is too small to hold the
/// adapter information or the `AdapterAddresses` parameter is `NULL`. The `SizePointer`
/// parameter returned points to the required size of the buffer to hold the adapter
/// information.
///
/// Source: https://docs.microsoft.com/en-us/windows/win32/api/iphlpapi/nf-iphlpapi-getadaptersaddresses#return-value
const ERROR_BUFFER_OVERFLOW: u32 = 111;

/// Max tries allowed to call `GetAdaptersAddresses` on a loop basis
const MAX_TRIES: usize = 3;

/// Success execution output from `GetAdaptersAddresses` call
const GET_ADAPTERS_ADDRESSES_SUCCESS_RESULT: u32 = 0;

/// A constant to store `winapi::shared::ws2def::AF_INET` casted as `u16`
const AF_INET: u16 = winapi::shared::ws2def::AF_INET as u16;

/// A constant to store ` winapi::shared::ws2def::AF_INET6` casted as `u16`
const AF_INET6: u16 = winapi::shared::ws2def::AF_INET6 as u16;

/// The address family of the addresses to retrieve. This parameter must be one of the following values.
/// The default address family is `AF_UNSPECT` in order to gather both IPv4 and IPv6 network interfaces.
///
/// Source: https://docs.microsoft.com/en-us/windows/win32/api/iphlpapi/nf-iphlpapi-getadaptersaddresses#parameters
const GET_ADAPTERS_ADDRESSES_FAMILY: u32 = AF_UNSPEC as u32;

/// A constant to store `winapi::um::iptypes::GAA_FLAG_INCLUDE_PREFIX`
const GET_ADAPTERS_ADDRESSES_FLAGS: ULONG = winapi::um::iptypes::GAA_FLAG_INCLUDE_PREFIX;

impl NetworkInterfaceConfig for NetworkInterface {
    fn show() -> Result<Vec<NetworkInterface>> {
        // Allocate a 15 KB buffer to start with.
        let mut size_pointer: u32 = 15000;
        let mut adapter_address = unsafe { malloc(size_pointer as usize) as *mut AdapterAddress };
        let mut iterations = 0;
        let mut get_adapter_addresses_result = 0;
        let mut network_interfaces: Vec<NetworkInterface> = Vec::new();

        while get_adapter_addresses_result != ERROR_BUFFER_OVERFLOW || iterations <= MAX_TRIES {
            adapter_address = unsafe { malloc(size_pointer as usize) as *mut AdapterAddress };

            if adapter_address.is_null() {
                // Memory allocation failed for IP_ADAPTER_ADDRESSES struct
                return Err(Error::GetIfAddrsError(
                    String::from("GetAdaptersAddresses"),
                    1,
                ));
            }

            get_adapter_addresses_result = unsafe {
                GetAdaptersAddresses(
                    GET_ADAPTERS_ADDRESSES_FAMILY,
                    GET_ADAPTERS_ADDRESSES_FLAGS,
                    null_mut(),
                    adapter_address,
                    &mut size_pointer,
                )
            };

            if get_adapter_addresses_result == ERROR_BUFFER_OVERFLOW {
                unsafe {
                    free(adapter_address as *mut c_void);
                };
                adapter_address = null_mut();
            } else {
                break;
            }

            iterations += 1;
        }

        if get_adapter_addresses_result == GET_ADAPTERS_ADDRESSES_SUCCESS_RESULT {
            while !adapter_address.is_null() {
                let address_name = make_adapter_address_name(&adapter_address)?;

                // Find broadcast address
                //
                // see https://docs.microsoft.com/en-us/windows/win32/api/iptypes/ns-iptypes-ip_adapter_addresses_lh
                //
                // On Windows Vista and later, the linked IP_ADAPTER_PREFIX structures pointed to by the FirstPrefix
                // member include three IP adapter prefixes for each IP address assigned to the adapter. These include
                // 0. the host IP address prefix
                // 1. the subnet IP address prefix
                // 2. and the subnet broadcast IP address prefix. << we want this
                // In addition, for each adapter there is a
                // 3. multicast address prefix
                // 4. and a broadcast address prefix.sb
                //
                // We only care for AF_INET entry with index 2.
                let mut current_prefix_address = unsafe { (*adapter_address).FirstPrefix };
                let mut prefix_index_ipv4 = 0;
                let mut bc_addr_ipv4 = None;
                while !current_prefix_address.is_null() {
                    let address = unsafe { (*current_prefix_address).Address };
                    if unsafe { (*address.lpSockaddr).sa_family } == AF_INET {
                        // only consider broadcast for IPv4
                        if prefix_index_ipv4 == 2 {
                            // 3rd IPv4 entry is broadcast address
                            let sockaddr: *mut SOCKADDR_IN = address.lpSockaddr as *mut SOCKADDR_IN;
                            bc_addr_ipv4 = Some(make_ipv4_addr(&sockaddr)?);
                            break;
                        }
                        prefix_index_ipv4 += 1; // only increase for AF_INET
                    }
                    current_prefix_address = unsafe { (*current_prefix_address).Next };
                }

                // see https://docs.microsoft.com/en-us/windows/win32/api/iphlpapi/nf-iphlpapi-getadaptersaddresses#examples
                let mac_addr_len = unsafe { (*adapter_address).PhysicalAddressLength } as _;
                let mac_addr = match mac_addr_len {
                    0 => None,
                    len => Some(format!(
                        "{}",
                        HexSlice::new(unsafe { &(*adapter_address).PhysicalAddress[..len] })
                    )),
                };

                // Find interface addresses
                let mut current_unicast_address = unsafe { (*adapter_address).FirstUnicastAddress };

                while !current_unicast_address.is_null() {
                    let address = unsafe { (*current_unicast_address).Address };

                    match unsafe { (*address.lpSockaddr).sa_family } {
                        AF_INET => {
                            let sockaddr: *mut SOCKADDR_IN = address.lpSockaddr as *mut SOCKADDR_IN;
                            let addr = make_ipv4_addr(&sockaddr)?;
                            let netmask = make_ipv4_netmask(&current_unicast_address);
                            let network_interface = NetworkInterface::new_afinet(
                                &address_name,
                                addr,
                                netmask,
                                bc_addr_ipv4,
                            )
                            .with_mac_addr(mac_addr.clone());

                            network_interfaces.push(network_interface);
                        }
                        AF_INET6 => {
                            let sockaddr: *mut SOCKADDR_IN6 =
                                address.lpSockaddr as *mut SOCKADDR_IN6;
                            let addr = make_ipv6_addr(&sockaddr)?;
                            let netmask = make_ipv6_netmask(&sockaddr);
                            let network_interface =
                                NetworkInterface::new_afinet6(&address_name, addr, netmask, None)
                                    .with_mac_addr(mac_addr.clone());

                            network_interfaces.push(network_interface);
                        }
                        _ => {}
                    }

                    if !current_unicast_address.is_null() {
                        current_unicast_address = unsafe { (*current_unicast_address).Next };
                    }
                }

                if !adapter_address.is_null() {
                    adapter_address = unsafe { (*adapter_address).Next };
                }
            }
        }

        Ok(network_interfaces)
    }
}

/// Retrieves the network interface name
fn make_adapter_address_name(adapter_address: &*mut AdapterAddress) -> Result<String> {
    let address_name = unsafe { (*(*adapter_address)).FriendlyName };
    let address_name_length = unsafe { wcslen(address_name as *const wchar_t) };
    let byte_slice = unsafe { from_raw_parts(address_name, address_name_length) };
    let string = String::from_utf16(byte_slice).map_err(Error::from)?;

    Ok(string)
}

/// Creates a `Ipv6Addr` from a `SOCKADDR_IN6`
fn make_ipv6_addr(sockaddr: &*mut SOCKADDR_IN6) -> Result<Ipv6Addr> {
    let address_bytes = unsafe { (*(*sockaddr)).sin6_addr.u.Byte() };
    let ip = Ipv6Addr::from(*address_bytes);

    Ok(ip)
}

/// Creates a `Ipv4Addr` from a `SOCKADDR_IN`
fn make_ipv4_addr(sockaddr: &*mut SOCKADDR_IN) -> Result<Ipv4Addr> {
    let address = unsafe { (*(*sockaddr)).sin_addr.S_un.S_addr() };

    if cfg!(target_endian = "little") {
        // due to a difference on how bytes are arranged on a
        // single word of memory by the CPU, swap bytes based
        // on CPU endianess to avoid having twisted IP addresses
        //
        // refer: https://github.com/rust-lang/rust/issues/48819
        return Ok(Ipv4Addr::from(address.swap_bytes()));
    }

    Ok(Ipv4Addr::from(*address))
}

/// This function relies on the `GetAdapterAddresses` API which is available only on Windows Vista
/// and later versions.
///
/// An implementation of `GetIpAddrTable` to get all available network interfaces would be required
/// in order to support previous versions of Windows.
fn make_ipv4_netmask(unicast_address: &*mut IP_ADAPTER_UNICAST_ADDRESS_LH) -> Netmask<Ipv4Addr> {
    let mask = unsafe { malloc(size_of::<u32>()) as *mut u32 };
    let on_link_prefix_length = unsafe { (*(*unicast_address)).OnLinkPrefixLength };

    match unsafe { ConvertLengthToIpv4Mask(on_link_prefix_length as u32, mask) } {
        0.. => {
            let mask = unsafe { *mask };

            if cfg!(target_endian = "little") {
                // due to a difference on how bytes are arranged on a
                // single word of memory by the CPU, swap bytes based
                // on CPU endianess to avoid having twisted IP addresses
                //
                // refer: https://github.com/rust-lang/rust/issues/48819
                return Some(Ipv4Addr::from(mask.swap_bytes()));
            }

            Some(Ipv4Addr::from(mask))
        }
    }
}

fn make_ipv6_netmask(_sockaddr: &*mut SOCKADDR_IN6) -> Netmask<Ipv6Addr> {
    None
}

#[cfg(test)]
mod tests {
    use std::{process::Command, cmp::min};

    use crate::{NetworkInterface, NetworkInterfaceConfig};

    #[test]
    fn test_mac_addr() {
        const MAC_ADDR_LEN: usize = "00:22:48:03:ED:76".len();

        let output = Command::new("getmac").arg("/nh").output().unwrap().stdout;
        let output_string = String::from_utf8(output).unwrap();
        let mac_addr_list: Vec<_> = output_string
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let line = &line[..min(MAC_ADDR_LEN, line.len())];
                match line.split('-').count() {
                    6 => Some(line.replace('-', ":")),
                    _ => None,
                }
            })
            .collect();
        assert!(mac_addr_list.len() > 0);

        let interfaces = NetworkInterface::show().unwrap();
        for mac_addr in mac_addr_list {
            assert!(interfaces
                .iter()
                .any(|int| int.mac_addr.as_ref() == Some(&mac_addr)));
        }
    }
}
