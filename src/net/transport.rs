//! Incremental, nonblocking framing. Partial frames survive WouldBlock and
//! every caller returns to its cancellation/liveness checks on each tick.
use anyhow::{bail, Context, Result};
use bincode::Options;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub struct Framed {
    pub stream: TcpStream,
    input: Vec<u8>,
    output: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
    limit: usize,
}
impl Framed {
    pub fn new(stream: TcpStream, limit: usize) -> Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            input: Vec::new(),
            output: VecDeque::new(),
            offset: 0,
            queued: 0,
            limit,
        })
    }
    pub fn queue<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let data = bincode::serialize(value)?;
        if data.len() > self.limit || self.queued + data.len() + 4 > self.limit * 2 {
            bail!("outgoing queue full");
        }
        let mut frame = Vec::with_capacity(4 + data.len());
        frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
        frame.extend(data);
        self.queued += frame.len();
        self.output.push_back(frame);
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        // Bounded work even when a peer can consume continuously.
        let mut budget = 65536;
        while budget > 0 {
            let Some(front) = self.output.front() else {
                break;
            };
            let end = front.len().min(self.offset + budget);
            match self.stream.write(&front[self.offset..end]) {
                Ok(0) => bail!("peer closed while writing"),
                Ok(n) => {
                    self.offset += n;
                    self.queued -= n;
                    budget -= n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
            if self.offset == front.len() {
                self.output.pop_front();
                self.offset = 0;
            }
        }
        Ok(())
    }
    pub fn pending(&self) -> bool {
        !self.output.is_empty()
    }
    pub fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let mut budget = 65536;
        loop {
            if self.input.len() >= 4 {
                let len = u32::from_be_bytes(self.input[..4].try_into().unwrap()) as usize;
                if len == 0 || len > self.limit {
                    bail!("invalid frame length: {len}");
                }
                if self.input.len() >= len + 4 {
                    let value = bincode::DefaultOptions::new()
                        .with_fixint_encoding()
                        .with_limit(self.limit as u64)
                        .reject_trailing_bytes()
                        .deserialize(&self.input[4..len + 4])?;
                    self.input.drain(..len + 4);
                    return Ok(Some(value));
                }
            }
            if budget == 0 {
                return Ok(None);
            }
            let mut buf = [0; 8192];
            match self.stream.read(&mut buf[..budget.min(8192)]) {
                Ok(0) => bail!("peer closed connection"),
                Ok(n) => {
                    self.input.extend_from_slice(&buf[..n]);
                    budget -= n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

pub fn connect(addr: SocketAddr, stop: &AtomicBool, timeout: Duration) -> Result<TcpStream> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    if let Err(e) = socket.connect(&addr.into()) {
        // EINPROGRESS/EWOULDBLOCK (POSIX) and WSAEWOULDBLOCK (Windows).
        if e.kind() != std::io::ErrorKind::WouldBlock
            && !matches!(e.raw_os_error(), Some(36 | 115 | 10035))
        {
            return Err(e.into());
        }
    }
    let start = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        if let Some(error) = socket.take_error()? {
            return Err(error.into());
        }
        if socket.peer_addr().is_ok() {
            return Ok(socket.into());
        }
        if start.elapsed() >= timeout {
            bail!("connection timeout: {addr}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn validate_port(port: u16) -> Result<()> {
    anyhow::ensure!(port > 0 && port < 65535, "Port must be between 1 and 65534");
    Ok(())
}
pub fn parse_address(value: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = value
        .parse()
        .context("Use an IP address and port, for example 192.168.1.100:4242")?;
    validate_port(addr.port())?;
    Ok(addr)
}
