use eth::EthFram;
use pnet::datalink::{self, NetworkInterface};

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use landscape_common::net::MacAddr;

pub mod eth;
pub mod icmp;
pub mod ipv4;
pub mod pppoe;
pub mod udp_packet;

/// Create Dump Thread
pub async fn create_dump(iface_name: String) -> (Sender<Vec<u8>>, Receiver<Box<EthFram>>) {
    use pnet::datalink::Channel::Ethernet;

    let interface_names_match = |iface: &NetworkInterface| iface.name == iface_name;

    // Find the network interface with the provided name
    let interfaces = datalink::interfaces();
    let interface = interfaces
        .into_iter()
        .filter(interface_names_match)
        .next()
        .unwrap_or_else(|| panic!("No such network interface: {}", iface_name));

    println!("interface name: {:?}", interface);
    // Create a channel to receive on
    let (mut tx, mut rx) = match datalink::channel(&interface, Default::default()) {
        Ok(Ethernet(tx, rx)) => (tx, rx),
        Ok(_) => panic!("packetdump: unhandled channel type"),
        Err(e) => panic!("packetdump: unable to create channel: {}", e),
    };
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1024);
    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Box<EthFram>>(1024);
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    // let base_time = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
    //     Ok(n) => n.as_nanos(),
    //     Err(_) => panic!("SystemTime before UNIX EPOCH!"),
    // };
    // let time = Instant::now();
    let mac = interface.mac.map(|mac| mac.octets()).map(|o| MacAddr::from(o));
    std::thread::spawn(move || loop {
        println!("loop 1");
        match rx.next() {
            Ok(packet) => {
                println!("catch data stard ==>");
                println!("catch data: {:?}", packet);
                let result = EthFram::new(packet, mac);
                println!("catch data end <==");
                if let Err(e) = out_tx.blocking_send(Box::new(result)) {
                    stop_rx.close(); // TODO 是否有必要
                    panic!("packetdump: unable to receive packet: {}", e)
                }
            }
            Err(e) => panic!("packetdump: unable to receive packet: {}", e),
        }
        match stop_rx.try_recv() {
            Ok(_) | Err(TryRecvError::Closed) => {
                println!("[dump] recevied signal to stop dump packet");
                break;
            }
            Err(TryRecvError::Empty) => {}
        }
    });
    std::thread::spawn(move || loop {
        println!("loop 2");
        match in_rx.blocking_recv() {
            Some(data) => {
                tx.send_to(&data, Some(interface.clone()));
            }
            None => {
                let _ = stop_tx.send(());
                println!("[dump] send signal to stop dump packet");
                break;
            }
        }
    });
    println!("create dump frunction end");
    (in_tx, out_rx)
}
