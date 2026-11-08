//! The TCP interface ([`super::TCP_IFACE`]): an HDLC-framed stream to one
//! configured `host:port`, reconnecting on loss — the client half of the
//! reference TCPClientInterface, which is the shape a field node uses to
//! reach a hub.

use alloc::vec::Vec;

use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Timer;
use leviculum_core::framing::hdlc::Deframer;
use static_cell::StaticCell;

/// Framed packets the loop routed to this interface.
pub static TX: Channel<CriticalSectionRawMutex, Vec<u8>, 8> = Channel::new();

const TARGET: &str = match option_env!("LEVICULUM_TCP_TARGET") {
    Some(s) => s,
    None => "",
};

/// The socket task: connect, shuffle the stream against the outbound
/// channel until the connection drops, then reconnect. Never returns.
#[embassy_executor::task]
pub async fn task(stack: Stack<'static>) {
    stack.wait_config_up().await;

    let Some((addr, port)) = super::parse_endpoint(TARGET) else {
        let _ = super::LOG.try_send("tcp=bad-target");
        return;
    };
    let endpoint = IpEndpoint::new(IpAddress::Ipv4(addr.into()), port);

    let rx_buf = RX_BUF.init([0u8; 2048]);
    let tx_buf = TX_BUF.init([0u8; 2048]);
    let mut socket = TcpSocket::new(stack, rx_buf, tx_buf);

    let mut deframer = Deframer::with_max_frame(super::TCP_HW_MTU);
    let mut chunk = [0u8; 512];
    loop {
        socket.abort();
        if socket.connect(endpoint).await.is_err() {
            let _ = super::LOG.try_send("tcp=connect-failed");
            Timer::after_millis(5000).await;
            continue;
        }
        let _ = super::LOG.try_send("tcp=connected");
        deframer.reset();

        loop {
            match embassy_futures::select::select(socket.read(&mut chunk), TX.receive()).await {
                embassy_futures::select::Either::First(Ok(0))
                | embassy_futures::select::Either::First(Err(_)) => {
                    let _ = super::LOG.try_send("tcp=disconnected");
                    break;
                }
                embassy_futures::select::Either::First(Ok(n)) => {
                    super::publish(super::TCP_IFACE, deframer.process(&chunk[..n]));
                }
                embassy_futures::select::Either::Second(framed) => {
                    if send(&mut socket, &framed).await.is_err() {
                        let _ = super::LOG.try_send("tcp=disconnected");
                        break;
                    }
                }
            }
        }
        Timer::after_millis(1000).await;
    }
}

static RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
static TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();

/// Write the whole frame. `TcpSocket::write` may accept fewer bytes than
/// offered; a short write silently drops the tail and corrupts the HDLC
/// stream for the rest of the connection's life.
async fn send(socket: &mut TcpSocket<'_>, data: &[u8]) -> Result<(), ()> {
    let mut off = 0;
    while off < data.len() {
        match socket.write(&data[off..]).await {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => off += n,
        }
    }
    socket.flush().await.map_err(|_| ())
}
