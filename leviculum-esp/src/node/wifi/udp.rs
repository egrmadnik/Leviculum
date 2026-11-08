//! The UDP interface ([`super::UDP_IFACE`]): one HDLC-framed packet per
//! datagram, received on a bound port and forwarded to a configured
//! address — the broadcast address by default, so nodes on the same LAN
//! hear each other exactly like the reference UDPInterface.

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Timer;
use leviculum_core::framing::hdlc::Deframer;
use static_cell::StaticCell;

/// Framed packets the loop routed to this interface.
pub static TX: Channel<CriticalSectionRawMutex, alloc::vec::Vec<u8>, 8> = Channel::new();

const UDP_PORT: u16 = match parse_port(option_env!("LEVICULUM_UDP_PORT")) {
    Some(p) => p,
    None => 4968,
};
const FORWARD: &str = match option_env!("LEVICULUM_UDP_FORWARD") {
    Some(s) => s,
    None => "255.255.255.255",
};

const fn parse_port(text: Option<&str>) -> Option<u16> {
    let Some(text) = text else { return None };
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !(b >= b'0' && b <= b'9') {
            return None;
        }
        v = v * 10 + (b - b'0') as u32;
        if v > 65535 {
            return None;
        }
        i += 1;
    }
    Some(v as u16)
}

/// The socket task: waits for DHCP, binds the listen port, then shuffles
/// datagrams and the outbound channel forever. A deframe that completes
/// goes [`super::INBOUND`]; a `TX` send frames and forwards.
#[embassy_executor::task]
pub async fn task(stack: Stack<'static>) {
    stack.wait_config_up().await;
    if let Some(cfg) = stack.config_v4() {
        super::log_str(alloc::format!("net=up ip={}", cfg.address));
    }

    let rx_meta = RX_META.init([PacketMetadata::EMPTY; 8]);
    let rx_buf = RX_BUF.init([0u8; 2048]);
    let tx_meta = TX_META.init([PacketMetadata::EMPTY; 8]);
    let tx_buf = TX_BUF.init([0u8; 2048]);
    let mut socket = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);
    if socket.bind(UDP_PORT).is_err() {
        let _ = super::LOG.try_send("udp=bind-failed");
        return;
    }
    let _ = super::LOG.try_send("udp=listening");

    let forward =
        super::parse_ipv4(FORWARD).map(|a| IpEndpoint::new(IpAddress::Ipv4(a.into()), UDP_PORT));

    let mut deframer = Deframer::with_max_frame(super::UDP_HW_MTU);
    let mut datagram = [0u8; 2048];
    loop {
        match embassy_futures::select::select(socket.recv_from(&mut datagram), TX.receive()).await {
            embassy_futures::select::Either::First(Ok((len, _src))) => {
                super::publish(super::UDP_IFACE, deframer.process(&datagram[..len]));
            }
            embassy_futures::select::Either::First(Err(_)) => {
                let _ = super::LOG.try_send("udp=rx-error");
                Timer::after_millis(1000).await;
            }
            embassy_futures::select::Either::Second(framed) => {
                if let Some(ep) = forward {
                    if socket.send_to(&framed, ep).await.is_err() {
                        let _ = super::LOG.try_send("udp=tx-error");
                    }
                }
            }
        }
    }
}

static RX_META: StaticCell<[PacketMetadata; 8]> = StaticCell::new();
static RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
static TX_META: StaticCell<[PacketMetadata; 8]> = StaticCell::new();
static TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
