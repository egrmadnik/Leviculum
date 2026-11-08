//! The UDP interface ([`super::UDP_IFACE`]): one HDLC-framed packet per
//! datagram, received on the configured port and forwarded to the
//! configured address — the broadcast address by default, so nodes on
//! the same LAN hear each other exactly like the reference UDPInterface.
//!
//! The port and forward address are runtime settings
//! ([`super::cfg_current`]): the task re-checks them every second and
//! rebinds when the port changed. A bind failure is retried, not fatal —
//! the interface comes up whenever the stack and settings allow.

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Timer;
use leviculum_core::framing::hdlc::Deframer;
use static_cell::StaticCell;

/// Framed packets the loop routed to this interface.
pub static TX: Channel<CriticalSectionRawMutex, alloc::vec::Vec<u8>, 8> = Channel::new();

/// The socket task: waits for DHCP, binds the settings' port, then
/// shuffles datagrams and the outbound channel forever. A deframe that
/// completes goes [`super::INBOUND`]; a `TX` send frames and forwards.
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

    let mut bound_port: Option<u16> = None;
    let mut seen_version = super::cfg_version();
    let mut deframer = Deframer::with_max_frame(super::UDP_HW_MTU);
    let mut datagram = [0u8; 2048];
    loop {
        let s = super::cfg_current();
        // (Re)bind when the settings' port differs from the bound one —
        // including after a failed bind, which retries every pass.
        if bound_port != Some(s.udp_port) {
            socket.close();
            if socket.bind(s.udp_port).is_err() {
                let _ = super::LOG.try_send("udp=bind-failed");
                bound_port = None;
            } else {
                bound_port = Some(s.udp_port);
                super::log_str(alloc::format!("udp=listening port={}", s.udp_port));
            }
        }
        let forward = s
            .udp_forward
            .map(|a| IpEndpoint::new(IpAddress::Ipv4(a.into()), s.udp_port));

        match embassy_futures::select::select3(
            socket.recv_from(&mut datagram),
            TX.receive(),
            Timer::after_millis(1000),
        )
        .await
        {
            embassy_futures::select::Either3::First(Ok((len, _src))) => {
                super::publish(super::UDP_IFACE, deframer.process(&datagram[..len]));
            }
            embassy_futures::select::Either3::First(Err(_)) => {
                let _ = super::LOG.try_send("udp=rx-error");
                Timer::after_millis(1000).await;
            }
            embassy_futures::select::Either3::Second(framed) => {
                if let Some(ep) = forward {
                    if socket.send_to(&framed, ep).await.is_err() {
                        let _ = super::LOG.try_send("udp=tx-error");
                    }
                }
            }
            embassy_futures::select::Either3::Third(_) => {
                if super::cfg_version() != seen_version {
                    // Settings changed: re-read on the next pass. A
                    // partial frame from the old configuration is stale.
                    seen_version = super::cfg_version();
                    deframer.reset();
                }
            }
        }
    }
}

static RX_META: StaticCell<[PacketMetadata; 8]> = StaticCell::new();
static RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
static TX_META: StaticCell<[PacketMetadata; 8]> = StaticCell::new();
static TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
