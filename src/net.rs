//! Wi-Fi, DHCP, DNS and TCP for `http`, on a hand-pumped smoltcp interface.

use crate::http::{self, Etag, FetchError, FetchOutcome, TlsBufs, TlsVerify};
use esp_hal::delay::Delay;
use esp_println::println;
use esp_radio::wifi::{Config as WifiConfig, WifiController, sta::StationConfig};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::socket::{dhcpv4, dns, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    DnsQueryType, EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address,
};
use static_cell::ConstStaticCell;

static TCP_RX: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0; 4096]);
static TCP_TX: ConstStaticCell<[u8; 1024]> = ConstStaticCell::new([0; 1024]);
static SOCKETS: ConstStaticCell<[SocketStorage<'static>; 3]> =
    ConstStaticCell::new([SocketStorage::EMPTY; 3]);
static DNS_QUERIES: ConstStaticCell<[Option<dns::DnsQuery>; 1]> = ConstStaticCell::new([None]);
static TLS_READ: ConstStaticCell<[u8; http::TLS_READ]> = ConstStaticCell::new([0; http::TLS_READ]);
static TLS_WRITE: ConstStaticCell<[u8; http::TLS_WRITE]> =
    ConstStaticCell::new([0; http::TLS_WRITE]);

const FALLBACK_DNS: [IpAddress; 2] = [
    IpAddress::Ipv4(Ipv4Address::new(1, 1, 1, 1)),
    IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8)),
];

const FETCH_DEADLINE_MS: u64 = 75_000;

fn millis() -> u64 {
    esp_hal::time::Instant::now()
        .duration_since_epoch()
        .as_millis()
}

fn now() -> Instant {
    let micros = esp_hal::time::Instant::now()
        .duration_since_epoch()
        .as_micros();
    Instant::from_micros(i64::try_from(micros).unwrap_or(i64::MAX))
}

/// No wakers: esp-radio's futures progress as the radio task preempts.
fn block_on_deadline<F: Future>(fut: F, timeout_ms: u64) -> Option<F::Output> {
    let mut fut = core::pin::pin!(fut);
    let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
    let t0 = millis();
    loop {
        if let core::task::Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return Some(out);
        }
        if millis() - t0 > timeout_ms {
            return None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    WifiDown,
    Dns,
    Fetch(FetchError),
}

impl From<dns::StartQueryError> for NetError {
    fn from(_: dns::StartQueryError) -> Self {
        Self::Dns
    }
}

impl From<tcp::ConnectError> for NetError {
    fn from(_: tcp::ConnectError) -> Self {
        Self::Fetch(FetchError::ConnectFail)
    }
}

pub struct Get<'a> {
    pub ip: Ipv4Address,
    pub host: &'a str,
    pub path: &'a str,
    pub bearer: Option<&'a str>,
    pub verify: TlsVerify<'a>,
}

/// Setup: connect, handshake and request; `None` if the request never left.
#[derive(Clone, Copy, Default)]
pub struct FetchTiming {
    pub setup_ms: Option<u64>,
    pub response_ms: Option<u64>,
}

/// esp-radio only ships an embassy-net driver.
struct RadioDevice(esp_radio::wifi::Interface);

struct RadioRxToken(esp_radio::wifi::WifiRxToken);
struct RadioTxToken(esp_radio::wifi::WifiTxToken);

impl smoltcp::phy::RxToken for RadioRxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        self.0.consume_token(|buf| f(buf))
    }
}

impl smoltcp::phy::TxToken for RadioTxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.0.consume_token(len, f)
    }
}

impl Device for RadioDevice {
    type RxToken<'a>
        = RadioRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = RadioTxToken
    where
        Self: 'a;

    fn receive(&mut self, _: Instant) -> Option<(RadioRxToken, RadioTxToken)> {
        self.0
            .receive()
            .map(|(rx, tx)| (RadioRxToken(rx), RadioTxToken(tx)))
    }

    fn transmit(&mut self, _: Instant) -> Option<RadioTxToken> {
        self.0.transmit().map(RadioTxToken)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

struct Link<'d> {
    controller: WifiController<'d>,
    device: RadioDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    tcp: SocketHandle,
    dns: SocketHandle,
    ip: Option<Ipv4Address>,
}

impl Link<'_> {
    fn poll(&mut self) {
        self.iface.poll(now(), &mut self.device, &mut self.sockets);
        let lease = match self.sockets.get_mut::<dhcpv4::Socket<'_>>(self.dhcp).poll() {
            Some(dhcpv4::Event::Configured(c)) => Some((c.address, c.router, c.dns_servers)),
            Some(dhcpv4::Event::Deconfigured) => None,
            None => return,
        };
        self.iface.routes_mut().remove_default_ipv4_route();
        self.iface.update_ip_addrs(|addrs| {
            addrs.clear();
            if let Some((cidr, ..)) = lease
                && addrs.push(IpCidr::Ipv4(cidr)).is_err()
            {
                println!("net: no room for address {cidr}");
            }
        });
        self.ip = lease.as_ref().map(|(cidr, ..)| cidr.address());
        let Some((_, router, dns)) = lease else {
            return;
        };
        if let Some(router) = router
            && let Err(full) = self.iface.routes_mut().add_default_ipv4_route(router)
        {
            println!("net: no default route via {router}: {full}");
        }
        let mut servers = FALLBACK_DNS;
        for (slot, server) in servers.iter_mut().zip(dns) {
            *slot = IpAddress::Ipv4(server);
        }
        self.sockets
            .get_mut::<dns::Socket<'_>>(self.dns)
            .update_servers(&servers);
    }

    fn tcp(&mut self) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut::<tcp::Socket<'_>>(self.tcp)
    }

    fn abort_tcp(&mut self) {
        self.tcp().abort();
        self.poll();
    }
}

pub struct Net<'d> {
    link: Link<'d>,
    tls_read: &'static mut [u8],
    tls_write: &'static mut [u8],
    next_port: u16,
    /// RAM only: the RTC frame hash absorbs a reboot's refetch.
    etag: Etag,
    pub last_timing: FetchTiming,
}

impl<'d> Net<'d> {
    pub fn new(
        controller: WifiController<'d>,
        station: esp_radio::wifi::Interface,
        seed: u64,
    ) -> Self {
        let mut device = RadioDevice(station);
        let mac = device.0.mac_address();
        let mut cfg = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        cfg.random_seed = seed;
        let iface = Interface::new(cfg, &mut device, now());
        let mut sockets = SocketSet::new(&mut SOCKETS.take()[..]);
        let dhcp = sockets.add(dhcpv4::Socket::new());
        let tcp = sockets.add(tcp::Socket::new(
            tcp::SocketBuffer::new(&mut TCP_RX.take()[..]),
            tcp::SocketBuffer::new(&mut TCP_TX.take()[..]),
        ));
        let dns = sockets.add(dns::Socket::new(&FALLBACK_DNS, &mut DNS_QUERIES.take()[..]));
        Self {
            link: Link {
                controller,
                device,
                iface,
                sockets,
                dhcp,
                tcp,
                dns,
                ip: None,
            },
            tls_read: TLS_READ.take(),
            tls_write: TLS_WRITE.take(),
            next_port: 49152,
            etag: Etag::new(),
            last_timing: FetchTiming::default(),
        }
    }

    /// Makes the next fetch a 200, not a 304.
    pub const fn forget_etag(&mut self) {
        self.etag = Etag::new();
    }

    pub fn connect_wifi(&mut self, ssid: &str, pass: &str) -> bool {
        let cfg = WifiConfig::Station(
            StationConfig::default()
                .with_ssid(ssid)
                .with_password(alloc::string::String::from(pass)),
        );
        self.link.controller.set_config(&cfg).is_ok() && self.reconnect()
    }

    /// Bounded: an absent AP fails, not wedges.
    pub fn reconnect(&mut self) -> bool {
        matches!(
            block_on_deadline(self.link.controller.connect_async(), 30_000),
            Some(Ok(_))
        )
    }

    pub fn poll(&mut self) {
        self.link.poll();
    }

    pub fn wait_ip(&mut self, timeout_ms: u64, delay: Delay) -> Option<Ipv4Address> {
        let t0 = millis();
        while millis() - t0 < timeout_ms {
            self.link.poll();
            if self.link.ip.is_some() {
                return self.link.ip;
            }
            delay.delay_millis(10);
        }
        None
    }

    pub fn resolve(
        &mut self,
        name: &str,
        timeout_ms: u64,
        delay: Delay,
        feed: &mut dyn FnMut(),
    ) -> Result<Ipv4Address, NetError> {
        let link = &mut self.link;
        let cx = link.iface.context();
        let handle = link
            .sockets
            .get_mut::<dns::Socket<'_>>(link.dns)
            .start_query(cx, name, DnsQueryType::A)?;
        let t0 = millis();
        loop {
            feed();
            link.poll();
            let socket = link.sockets.get_mut::<dns::Socket<'_>>(link.dns);
            match socket.get_query_result(handle) {
                Ok(addrs) => {
                    return match addrs.first() {
                        Some(IpAddress::Ipv4(ip)) => Ok(*ip),
                        None => Err(NetError::Dns),
                    };
                }
                Err(dns::GetQueryResultError::Pending) if millis() - t0 <= timeout_ms => {}
                Err(dns::GetQueryResultError::Pending) => {
                    socket.cancel_query(handle);
                    return Err(NetError::Dns);
                }
                Err(dns::GetQueryResultError::Failed) => return Err(NetError::Dns),
            }
            delay.delay_millis(1);
        }
    }

    pub fn get(
        &mut self,
        get: Get<'_>,
        body: &mut [u8],
        delay: Delay,
        feed: &mut dyn FnMut(),
    ) -> Result<FetchOutcome, NetError> {
        if !self.link.controller.is_connected() {
            return Err(NetError::WifiDown);
        }
        let t_start = millis();
        // A fresh ephemeral port dodges TIME_WAIT.
        self.next_port = if self.next_port >= 65000 {
            49152
        } else {
            self.next_port + 1
        };
        self.link.abort_tcp();
        let cx = self.link.iface.context();
        let remote = (IpAddress::Ipv4(get.ip), 443);
        self.link
            .sockets
            .get_mut::<tcp::Socket<'_>>(self.link.tcp)
            .connect(cx, remote, self.next_port)?;

        let etag = core::str::from_utf8(self.etag.get())
            .ok()
            .filter(|e| !e.is_empty());
        let head = http::build_request(get.host, get.path, etag, get.bearer);
        let mut new_etag = Etag::new();
        let mut t_sent = None;
        let request = http::Request {
            head: head.as_bytes(),
            sent: &mut || t_sent = Some(millis()),
        };
        let response = http::Response {
            body,
            etag: &mut new_etag,
        };
        let io = TcpIo {
            link: &mut self.link,
            delay,
            feed,
            deadline: millis() + FETCH_DEADLINE_MS,
        };
        let bufs = TlsBufs {
            read: &mut *self.tls_read,
            write: &mut *self.tls_write,
        };
        let result = http::tls_exchange(io, bufs, get.verify, request, response);
        let t_end = millis();
        self.last_timing = FetchTiming {
            setup_ms: t_sent.map(|t| t - t_start),
            response_ms: t_sent.map(|t| t_end - t),
        };
        // A 304 keeps the held ETag.
        self.link.abort_tcp();
        if matches!(result, Ok(FetchOutcome::Fetched(_))) {
            self.etag = new_etag;
        }
        result.map_err(NetError::Fetch)
    }
}

#[derive(Debug)]
pub enum IoError {
    Timeout,
    Closed,
}

impl From<tcp::RecvError> for IoError {
    fn from(_: tcp::RecvError) -> Self {
        Self::Closed
    }
}

impl From<tcp::SendError> for IoError {
    fn from(_: tcp::SendError) -> Self {
        Self::Closed
    }
}

impl core::fmt::Display for IoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self, f)
    }
}

impl core::error::Error for IoError {}

impl embedded_io::Error for IoError {
    fn kind(&self) -> embedded_io::ErrorKind {
        match self {
            Self::Timeout => embedded_io::ErrorKind::TimedOut,
            Self::Closed => embedded_io::ErrorKind::ConnectionReset,
        }
    }
}

/// Blocking, feeding the watchdog. It must write first (HTTP and TLS do): a
/// read in SYN-SENT looks like EOF.
struct TcpIo<'a, 'd> {
    link: &'a mut Link<'d>,
    delay: Delay,
    feed: &'a mut dyn FnMut(),
    deadline: u64,
}

impl TcpIo<'_, '_> {
    fn wait(&self, what: &str, state: tcp::State) -> Result<(), IoError> {
        if millis() > self.deadline {
            println!("net: {what} timeout ({state})");
            return Err(IoError::Timeout);
        }
        self.delay.delay_millis(1);
        Ok(())
    }
}

impl embedded_io::ErrorType for TcpIo<'_, '_> {
    type Error = IoError;
}

impl embedded_io::Read for TcpIo<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            (self.feed)();
            self.link.poll();
            let sock = self.link.tcp();
            if sock.can_recv() {
                return Ok(sock.recv_slice(buf)?);
            }
            if !sock.may_recv() {
                return Ok(0);
            }
            let state = sock.state();
            self.wait("read", state)?;
        }
    }
}

impl embedded_io::Write for TcpIo<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            (self.feed)();
            self.link.poll();
            let sock = self.link.tcp();
            if sock.can_send() {
                let n = sock.send_slice(buf)?;
                if n > 0 {
                    self.link.poll();
                    return Ok(n);
                }
            } else if !sock.is_active() {
                return Err(IoError::Closed);
            }
            let state = sock.state();
            self.wait("write", state)?;
        }
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.link.poll();
        Ok(())
    }
}
