use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Range,
    sync::Arc,
};

use crate::{
    dump::udp_packet::dhcp::{
        options::{DhcpOptionMessageType, DhcpOptions},
        DhcpEthFrame, DhcpOptionFrame,
    },
    macaddr::MacAddr,
    service::ServiceStatus,
};

use cidr::Ipv4Inet;
use futures::TryStreamExt;
use netlink_packet_route::address::AddressAttribute;
use rtnetlink::{new_connection, Handle};
use tokio::sync::watch;

use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use super::DhcpServerIpv4Config;

const DEFAULT_RENT_TIME: u64 = 60 * 60 * 12;
const IP_EXPIRE_INTERVAL: u64 = 60 * 10;

async fn add_address(link_name: &str, ip: IpAddr, prefix_length: u8, handle: Handle) {
    let mut links = handle.link().get().match_name(link_name.to_string()).execute();
    if let Some(link) = links.try_next().await.unwrap() {
        let mut addr_iter = handle.address().get().execute();
        // 与要添加的 ip 是否相同
        let mut need_create_ip = true;
        while let Some(addr) = addr_iter.try_next().await.unwrap() {
            let perfix_len_equal = addr.header.prefix_len == prefix_length;
            let mut link_name_equal = false;
            let mut ip_equal = false;

            for attr in addr.attributes.iter() {
                match attr {
                    AddressAttribute::Address(addr) => {
                        if *addr == ip {
                            ip_equal = true;
                        }
                    }
                    AddressAttribute::Label(label) => {
                        if *label == link_name.to_string() {
                            link_name_equal = true;
                        }
                    }
                    _ => {}
                }
            }

            if link_name_equal {
                if ip_equal && perfix_len_equal {
                    need_create_ip = false;
                } else {
                    println!("del: {addr:?}");
                    handle.address().del(addr).execute().await.unwrap();
                    need_create_ip = true;
                }
            }
        }

        if need_create_ip {
            // println!("need create ip: {need_create_ip:?}");
            handle.address().add(link.header.index, ip, prefix_length).execute().await.unwrap()
        }
    }
}

pub async fn init_dhcp_server(
    iface_name: String,
    dhcp_config: DhcpServerIpv4Config,
    dhcp_server_status: watch::Sender<ServiceStatus>,
) {
    dhcp_server_status.send_replace(ServiceStatus::Staring);
    let ip = dhcp_config.server_ip_addr;

    let prefix_length = dhcp_config.network_mask;
    let link_name = iface_name.clone();
    tokio::spawn(async move {
        let (connection, handle, _) = new_connection().unwrap();
        tokio::spawn(connection);
        add_address(&link_name, IpAddr::V4(ip), prefix_length, handle).await
    });

    let socket_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 67);

    let socket = UdpSocket::bind(socket_addr).await.unwrap();
    if let Err(e) = socket.bind_device(Some(iface_name.clone().as_bytes())) {
        println!("Failed to bind to device: {:?}", e);
    } else {
        println!("Successfully bound to device {}", iface_name);
    }
    if let Err(e) = socket.set_broadcast(true) {
        println!("Failed to set broadcast: {:?}", e);
    }

    let send_socket = Arc::new(socket);
    let recive_socket_raw = send_socket.clone();

    let (message_tx, mut message_rx) = tokio::sync::mpsc::channel::<(Vec<u8>, SocketAddr)>(1024);

    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        loop {
            tokio::select! {
                result = recive_socket_raw.recv_from(&mut buf) => {
                    // 接收数据包
                    match result {
                        Ok((len, addr)) => {
                            // println!("Received {} bytes from {}", len, addr);
                            let message = buf[..len].to_vec();
                            if let Err(e) = message_tx.try_send((message, addr)) {
                                println!("Error sending message to channel: {:?}", e);
                            }
                        }
                        Err(e) => {
                            println!("Error receiving data: {:?}", e);
                        }
                    }
                },
                _ = message_tx.closed() => {
                    break;
                }
            }
        }
    });

    let ipv4_inet = dhcp_config.get_ipv4_inet();
    let options = dhcp_config.options.clone();
    let host_range = dhcp_config.host_range;

    dhcp_server_status.send_replace(ServiceStatus::Running);
    let mut dhcp_server_service_status = dhcp_server_status.subscribe();
    tokio::task::spawn(async move {
        let timeout_timer =
            tokio::time::sleep(tokio::time::Duration::from_secs(IP_EXPIRE_INTERVAL));
        tokio::pin!(timeout_timer);
        if let Some(server_ip) = ipv4_inet {
            let mut dhcp_status = DhcpServerStatus::new(server_ip, options, host_range);
            loop {
                tokio::select! {
                    // 处理消息分支
                    message = message_rx.recv() => {
                        match message {
                            Some(message) => handle_dhcp_message(&mut dhcp_status, &send_socket, message).await,
                            None => {
                                println!("dhcp server handle server fail, exit loop");
                                break;
                            }
                        }
                    }
                    // 租期超时分支
                    _ = &mut timeout_timer => {
                        dhcp_status.expire_check();
                        timeout_timer.as_mut().reset(tokio::time::Instant::now() + tokio::time::Duration::from_secs(IP_EXPIRE_INTERVAL));
                    }
                    // 处理外部关闭服务通知
                    change_result = dhcp_server_service_status.changed() => {
                        if let Err(_) = change_result {
                            println!("get change result error. exit loop");
                            break;
                        }

                        let current_status = &*dhcp_server_service_status.borrow();
                        match current_status {
                            ServiceStatus::Stopping | ServiceStatus::Stop{..} => {
                                println!("stop exit");
                                break;
                            },
                            _ => {}
                        }
                    }
                }
            }
            println!("dhcp server handle loop end");
            dhcp_server_status.send_replace(ServiceStatus::Stop { message: None });
        }
    });
}

async fn handle_dhcp_message(
    dhcp_status: &mut DhcpServerStatus,
    send_socket: &Arc<UdpSocket>,
    (message, msg_addr): (Vec<u8>, SocketAddr),
) {
    let dhcp = DhcpEthFrame::new(&message);
    println!("dhcp: {dhcp:?}");

    if let Some(dhcp) = dhcp {
        println!("dhcp xid: {:04x}", dhcp.xid);
        match dhcp.op {
            1 => match dhcp.options.message_type {
                DhcpOptionMessageType::Discover => {
                    let payload = dhcp_status.gen_offer(dhcp);
                    let payload = crate::dump::udp_packet::EthUdpType::Dhcp(Box::new(payload));

                    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 68);

                    println!("payload: {payload:?}");
                    match send_socket.send_to(&payload.convert_to_payload(), &addr).await {
                        Ok(len) => {
                            println!("send len: {:?}", len);
                        }
                        Err(e) => {
                            println!("error: {:?}", e);
                        }
                    }
                }
                DhcpOptionMessageType::Request => {
                    let Some(payload) = dhcp_status.gen_ack(dhcp) else {
                        return;
                    };

                    let addr = if payload.is_broaddcast() {
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)), 68)
                    } else {
                        SocketAddr::new(IpAddr::V4(payload.yiaddr.clone()), msg_addr.port())
                    };

                    let payload = crate::dump::udp_packet::EthUdpType::Dhcp(Box::new(payload));

                    println!("payload ack: {:?}", payload.convert_to_payload());
                    match send_socket.send_to(&payload.convert_to_payload(), &addr).await {
                        Ok(len) => {
                            println!("send len: {:?}", len);
                        }
                        Err(e) => {
                            println!("error: {:?}", e);
                        }
                    }
                }
                DhcpOptionMessageType::Decline => todo!(),
                DhcpOptionMessageType::Ack => todo!(),
                DhcpOptionMessageType::Nak => todo!(),
                DhcpOptionMessageType::Release => {
                    println!("{dhcp:?}");
                }
                DhcpOptionMessageType::Inform => todo!(),
                DhcpOptionMessageType::ForceRenew => todo!(),
                DhcpOptionMessageType::LeaseQuery => todo!(),
                DhcpOptionMessageType::LeaseUnassigned => todo!(),
                DhcpOptionMessageType::LeaseUnknown => todo!(),
                DhcpOptionMessageType::LeaseActive => todo!(),
                DhcpOptionMessageType::BulkLeaseQuery => todo!(),
                DhcpOptionMessageType::LeaseQueryDone => todo!(),
                DhcpOptionMessageType::ActiveLeaseQuery => todo!(),
                DhcpOptionMessageType::LeaseQueryStatus => todo!(),
                DhcpOptionMessageType::Tls => todo!(),
                _ => {}
            },
            2 => {}
            3 => {}
            _ => {}
        }
    }
}

// DHCP Server Status
#[derive(Debug)]
struct DhcpServerStatus {
    /// mac addr ip pair
    ip_map: HashMap<MacAddr, (Ipv4Addr, Instant)>,
    ///
    server_ip: Ipv4Inet,

    options_map: HashMap<u8, DhcpOptions>,

    /// allocatd host ids
    allocated_host: HashSet<u32>,

    host_range: Range<u32>,
}

impl DhcpServerStatus {
    pub fn new(server_ip: Ipv4Inet, options: Vec<DhcpOptions>, host_range: Range<u32>) -> Self {
        let allocated_host = HashSet::new();
        let mut options_map = HashMap::new();
        for each in options.iter() {
            options_map.insert(each.get_index(), each.clone());
        }
        Self {
            host_range,
            ip_map: HashMap::new(),
            server_ip,
            options_map,
            allocated_host,
        }
    }

    /// get offer
    pub fn gen_offer(&mut self, frame: DhcpEthFrame) -> DhcpEthFrame {
        let mut options = vec![];
        let request_params = if let Some(request_params) = frame.options.has_option(55) {
            request_params
        } else {
            crate::dump::udp_packet::dhcp::get_default_request_list()
        };

        if let DhcpOptions::ParameterRequestList(info_list) = request_params {
            for each_index in info_list {
                if let Some(opt) = self.options_map.get(&each_index) {
                    options.push(opt.clone());
                } else {
                    println!("在配置中找不到这个 option 配置, index: {each_index:?}");
                }
            }
        }

        options.push(DhcpOptions::ServerIdentifier(self.server_ip.address()));

        let options = DhcpOptionFrame {
            message_type: DhcpOptionMessageType::Offer,
            options,
            end: vec![255],
        };

        // Check if IP is assigned
        let client_addr = if let Some((ip_add, _)) = self.ip_map.get(&frame.chaddr) {
            ip_add.clone()
        } else {
            println!("checksum: {:?}", frame.chaddr.u32_ckecksum());
            let host_id = get_host_id(
                self.host_range.clone(),
                frame.chaddr.u32_ckecksum(),
                &mut self.allocated_host,
            )
            .unwrap();
            let (client_addr, _overflow) = self.server_ip.first().overflowing_add_u32(host_id);
            let expire_instant = Instant::now() + Duration::from_secs(DEFAULT_RENT_TIME);
            self.ip_map.insert(frame.chaddr, (client_addr.address(), expire_instant));
            client_addr.address()
        };

        let offer = DhcpEthFrame {
            op: 2,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: frame.xid,
            secs: frame.secs,
            flags: frame.flags,
            ciaddr: Ipv4Addr::new(0, 0, 0, 0),
            yiaddr: client_addr,
            siaddr: Ipv4Addr::new(0, 0, 0, 0),
            giaddr: Ipv4Addr::new(0, 0, 0, 0),
            chaddr: frame.chaddr,
            sname: [0; 64].to_vec(),
            file: [0; 128].to_vec(),
            magic_cookie: frame.magic_cookie,
            options,
        };
        offer
    }

    fn gen_ack(&mut self, frame: DhcpEthFrame) -> Option<DhcpEthFrame> {
        let mut options = vec![];
        let request_params = if let Some(request_params) = frame.options.has_option(55) {
            request_params
        } else {
            crate::dump::udp_packet::dhcp::get_default_request_list()
        };
        if let DhcpOptions::ParameterRequestList(info_list) = request_params {
            if !info_list.contains(&51) {
                options.push(DhcpOptions::AddressLeaseTime(40));
            }
            for each_index in info_list {
                if let Some(opt) = self.options_map.get(&each_index) {
                    options.push(opt.clone());
                }
            }
        }

        let (message_type, client_addr) =
            if let Some((ip_add, expire)) = self.ip_map.get_mut(&frame.chaddr) {
                // 刷新过期时间
                *expire = Instant::now() + Duration::from_secs(DEFAULT_RENT_TIME);
                (DhcpOptionMessageType::Ack, ip_add.clone())
            } else {
                (DhcpOptionMessageType::Nak, Ipv4Addr::UNSPECIFIED)
            };

        let options = DhcpOptionFrame { message_type, options, end: vec![255] };

        let offer = DhcpEthFrame {
            op: 2,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: frame.xid,
            secs: frame.secs,
            flags: frame.flags,
            ciaddr: Ipv4Addr::new(0, 0, 0, 0),
            yiaddr: client_addr,
            siaddr: Ipv4Addr::new(0, 0, 0, 0),
            giaddr: Ipv4Addr::new(0, 0, 0, 0),
            chaddr: frame.chaddr,
            sname: [0; 64].to_vec(),
            file: [0; 128].to_vec(),
            magic_cookie: frame.magic_cookie,
            options,
        };
        Some(offer)
    }

    pub fn expire_check(&mut self) {
        let now = Instant::now();
        self.ip_map.retain(|mac_addr, (ip, time)| {
            if *time > now {
                println!("mac: {mac_addr}, ip: {ip}, expire");
                true
            } else {
                false
            }
        });
    }
}

/// Generate a unique host ID within the specified range
fn get_host_id(
    host_range: Range<u32>,
    seed: u32,
    allocated_host: &mut HashSet<u32>,
) -> Option<u32> {
    let host_range_size = host_range.end - host_range.start;

    let index = seed % host_range_size;
    let query_index = host_range.start + index;
    // println!("query_index: {query_index:?}");
    let result_index = if !allocated_host.contains(&query_index) {
        allocated_host.insert(query_index);
        query_index
    } else {
        let mut inc_seed = index;

        loop {
            inc_seed += 1;
            if inc_seed / host_range_size >= 2 {
                break u32::MAX;
            }
            let index = inc_seed % host_range_size;
            let query_index = host_range.start + index;

            if !allocated_host.contains(&query_index) {
                // TODO 返回这个 index
                allocated_host.insert(query_index);
                break query_index;
            }
        }
    };
    if result_index == u32::MAX {
        None
    } else {
        Some(result_index)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::get_host_id;

    #[test]
    pub fn test_ip_alloc() {
        let host_size = 1..8;
        let mut seed = 2;
        let mut allocated_host = HashSet::new();
        allocated_host.insert(5);

        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");

        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");
        seed = 0;
        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");

        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");

        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");

        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");

        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");

        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");

        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");

        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");
    }

    #[test]
    pub fn test_ip_alloc_same_seed_large_then_2_lap() {
        let host_size = 1..254;
        let seed = 1398943828;
        let mut allocated_host = HashSet::new();

        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");
        let index = get_host_id(host_size.clone(), seed, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");

        let index = get_host_id(host_size.clone(), 2060278997, &mut allocated_host);
        println!("index: {index:?}, {allocated_host:?}");
    }
}
