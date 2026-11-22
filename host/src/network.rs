use anyhow::{Context, Result, bail};
use std::net::Ipv4Addr;
use std::ptr::null;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

#[derive(Clone, Copy)]
pub struct Interface {
    pub address: Ipv4Addr,
    prefix: u8,
    adapter_type: u32,
}

impl Interface {
    pub fn pick(requested: Option<Ipv4Addr>) -> Result<Self> {
        if requested == Some(Ipv4Addr::LOCALHOST) {
            return Ok(Self {
                address: Ipv4Addr::LOCALHOST,
                prefix: 32,
                adapter_type: 0,
            });
        }
        let mut interfaces = interfaces()?;
        if let Some(address) = requested {
            return interfaces
                .into_iter()
                .find(|interface| interface.address == address)
                .context("--bind 需要本机当前的局域网 IPv4 地址");
        }
        interfaces.sort_by_key(|interface| match interface.adapter_type {
            71 => 0,
            6 => 1,
            _ => 2,
        });
        interfaces
            .into_iter()
            .next()
            .context("没有可用的局域网 IPv4，请先连接 Wi-Fi 或有线网络")
    }

    pub fn contains(self, address: Ipv4Addr) -> bool {
        let mask = u32::MAX << (32 - self.prefix);
        u32::from(address) & mask == u32::from(self.address) & mask
    }
}

fn interfaces() -> Result<Vec<Interface>> {
    let mut size = 16 * 1024_u32;
    let options = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut buffer = vec![0_u64; (size as usize).div_ceil(8)];
    let mut head = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    let mut result =
        unsafe { GetAdaptersAddresses(u32::from(AF_INET), options, null(), head, &mut size) };
    if result == 111 {
        buffer.resize((size as usize).div_ceil(8), 0);
        head = buffer.as_mut_ptr().cast();
        result =
            unsafe { GetAdaptersAddresses(u32::from(AF_INET), options, null(), head, &mut size) };
    }
    if result != 0 {
        bail!("读取网络适配器失败（{result}）");
    }
    let mut result = Vec::new();
    let mut adapter = head;
    while let Some(info) = unsafe { adapter.as_ref() } {
        if info.OperStatus == 1 {
            let mut unicast = info.FirstUnicastAddress;
            while let Some(entry) = unsafe { unicast.as_ref() } {
                let sockaddr = entry.Address.lpSockaddr;
                if !sockaddr.is_null() && unsafe { (*sockaddr).sa_family == AF_INET } {
                    let socket = unsafe { &*sockaddr.cast::<SOCKADDR_IN>() };
                    let address =
                        Ipv4Addr::from(unsafe { socket.sin_addr.S_un.S_addr }.to_ne_bytes());
                    if address.is_private() && (1..=32).contains(&entry.OnLinkPrefixLength) {
                        result.push(Interface {
                            address,
                            prefix: entry.OnLinkPrefixLength,
                            adapter_type: info.IfType,
                        });
                    }
                }
                unicast = entry.Next;
            }
        }
        adapter = info.Next;
    }
    Ok(result)
}
