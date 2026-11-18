//! `seiji forward`: a tunnel from a lease's host to a port on the developer's machine.
//!
//! The simulator runs on the Seiji host, so `127.0.0.1:8081` there is the host, not the laptop
//! where Metro (or any dev server) runs. `seiji forward 8081` opens a WebSocket to
//! `/v1/leases/{id}/forward`. The server listens on a free loopback port for that lease and
//! carries every TCP connection made to it through the WebSocket; the CLI dials its local port
//! for each one. The CLI only ever dials the one port it was given, so the server cannot reach
//! anything else on the laptop.
//!
//! Frames are binary WebSocket messages: one kind byte, a big-endian `u32` connection id, then
//! the payload. `OPEN` (server to client) announces a connection, `DATA` carries bytes either way
//! and `CLOSE` says that side stopped sending. The first message, a text one from the server,
//! is the [`Hello`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Platform;
use crate::pool::Pool;
use crate::store::{Lease, User, now};

pub const OPEN: u8 = 1;
pub const DATA: u8 = 2;
pub const CLOSE: u8 = 3;

/// Bytes read from a socket per `DATA` frame.
const CHUNK: usize = 64 * 1024;
/// Frames queued for the WebSocket, and per connection for its socket. A full queue makes the
/// sender wait: a slow side slows the tunnel instead of filling memory.
const QUEUE: usize = 64;
/// An open tunnel counts as activity on its lease this often.
const TOUCH_EVERY: Duration = Duration::from_secs(60);

/// What the server tells the client once the tunnel is up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub lease_id: String,
    pub platform: Platform,
    /// The port on the host's loopback.
    pub port: u16,
    /// `host:port` as the device reaches it: `127.0.0.1:<port>` on an iOS simulator (it shares
    /// the host's network), `10.0.2.2:<port>` on an Android emulator.
    pub device_address: String,
}

pub fn frame(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn parse(msg: &[u8]) -> Option<(u8, u32, &[u8])> {
    let (head, payload) = msg.split_at_checked(5)?;
    Some((head[0], u32::from_be_bytes([head[1], head[2], head[3], head[4]]), payload))
}

/// Pumps one TCP connection: its bytes go out as `DATA` frames on `out`, and the bytes that
/// arrive on `inbound` are written to it. Ends when both directions are done.
pub async fn pump(stream: TcpStream, id: u32, out: mpsc::Sender<Vec<u8>>, mut inbound: mpsc::Receiver<Bytes>) {
    let (mut read, mut write) = stream.into_split();
    let up = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out.send(frame(DATA, id, &buf[..n])).await.is_err() {
                        return;
                    }
                }
            }
        }
        let _ = out.send(frame(CLOSE, id, &[])).await;
    };
    let down = async {
        while let Some(bytes) = inbound.recv().await {
            if write.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = write.shutdown().await;
    };
    tokio::join!(up, down);
}

/// The open connections of one tunnel end, by id.
#[derive(Default)]
pub struct Conns(HashMap<u32, mpsc::Sender<Bytes>>);

impl Conns {
    /// Starts pumping `stream` as connection `id`.
    pub fn start(&mut self, id: u32, stream: TcpStream, out: &mpsc::Sender<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(QUEUE);
        self.0.insert(id, tx);
        tokio::spawn(pump(stream, id, out.clone(), rx));
    }

    /// Hands a `DATA` or `CLOSE` frame from the other end to its connection.
    pub async fn deliver(&mut self, kind: u8, id: u32, payload: &[u8]) {
        match kind {
            DATA => {
                if let Some(tx) = self.0.get(&id)
                    && tx.send(Bytes::copy_from_slice(payload)).await.is_err()
                {
                    self.0.remove(&id);
                }
            }
            // Dropping the sender ends that connection's write side.
            CLOSE => {
                self.0.remove(&id);
            }
            _ => {}
        }
    }
}

// ---- server ----

/// Open tunnels per lease.
#[derive(Default)]
pub struct Forwards {
    open: Arc<StdMutex<HashMap<String, usize>>>,
}

/// Holds a tunnel's place in its lease's count until dropped.
struct Slot {
    open: Arc<StdMutex<HashMap<String, usize>>>,
    lease_id: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut open = self.open.lock().expect("forwards mutex");
        if let Some(n) = open.get_mut(&self.lease_id) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.lease_id);
            }
        }
    }
}

/// Why a tunnel was refused, as HTTP sees it.
#[derive(Debug)]
pub enum ForwardError {
    Disabled,
    NotFound(anyhow::Error),
    Full(usize),
    Internal(anyhow::Error),
}

/// A tunnel admitted for a lease, listening, not yet carrying anything.
pub struct Tunnel {
    listener: TcpListener,
    hello: Hello,
    cancel: CancellationToken,
    _slot: Slot,
}

impl Pool {
    /// Admits a tunnel for the caller's ready lease and binds its loopback port.
    pub async fn open_forward(&self, user: &User, lease_id: &str) -> Result<Tunnel, ForwardError> {
        if !self.config.forward.enabled {
            return Err(ForwardError::Disabled);
        }
        let lease: Lease = self.peek_lease(user, Some(lease_id)).map_err(ForwardError::NotFound)?;
        let rt = self
            .runtime_of(&lease.id)
            .ok_or_else(|| ForwardError::NotFound(anyhow::anyhow!("lease {} is ending", lease.id)))?;
        let max = self.config.forward.max_per_lease;
        let slot = {
            let mut open = self.forwards.open.lock().expect("forwards mutex");
            let n = open.entry(lease.id.clone()).or_default();
            if *n >= max {
                return Err(ForwardError::Full(max));
            }
            *n += 1;
            Slot { open: self.forwards.open.clone(), lease_id: lease.id.clone() }
        };
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("binding a loopback port")
            .map_err(ForwardError::Internal)?;
        let port = listener.local_addr().map_err(|e| ForwardError::Internal(e.into()))?.port();
        let device_host = match lease.platform {
            Platform::Ios => "127.0.0.1",
            Platform::Android => "10.0.2.2",
        };
        let hello = Hello {
            lease_id: lease.id.clone(),
            platform: lease.platform,
            port,
            device_address: format!("{device_host}:{port}"),
        };
        Ok(Tunnel { listener, hello, cancel: rt.cancel.child_token(), _slot: slot })
    }
}

/// One side of the WebSocket, whichever library speaks it.
pub enum Wire {
    Text(String),
    Binary(Vec<u8>),
    Closed,
}

/// Runs a tunnel over a WebSocket until either side closes or the lease ends. `wire` and
/// `recv` adapt the WebSocket library: messages to send, and messages received.
pub async fn serve<R>(pool: Arc<Pool>, tunnel: Tunnel, wire: mpsc::Sender<Wire>, mut recv: R)
where
    R: futures_util::Stream<Item = Wire> + Unpin + Send,
{
    use futures_util::StreamExt;
    let Tunnel { listener, hello, cancel, _slot } = tunnel;
    let send = |w: Wire| {
        let wire = wire.clone();
        async move { wire.send(w).await.is_ok() }
    };
    if !send(Wire::Text(serde_json::to_string(&hello).expect("hello serializes"))).await {
        return;
    }
    tracing::info!(lease = %hello.lease_id, port = hello.port, "forward open");
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(QUEUE);
    let mut conns = Conns::default();
    let mut next_id: u32 = 0;
    let mut touch = tokio::time::interval(TOUCH_EVERY);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                next_id = next_id.wrapping_add(1);
                if !send(Wire::Binary(frame(OPEN, next_id, &[]))).await {
                    break;
                }
                conns.start(next_id, stream, &out_tx);
            }
            Some(out) = out_rx.recv() => {
                if !send(Wire::Binary(out)).await {
                    break;
                }
            }
            msg = recv.next() => match msg {
                Some(Wire::Binary(b)) => {
                    if let Some((kind, id, payload)) = parse(&b) {
                        conns.deliver(kind, id, payload).await;
                    }
                }
                Some(Wire::Text(_)) => {}
                Some(Wire::Closed) | None => break,
            },
            _ = touch.tick() => {
                let _ = pool.store.touch_lease(&hello.lease_id, now());
            }
            _ = cancel.cancelled() => break,
        }
    }
    let _ = send(Wire::Closed).await;
    tracing::info!(lease = %hello.lease_id, port = hello.port, "forward closed");
}

// ---- client ----

/// The client end: connects to `ws_url` with `token`, then dials `127.0.0.1:local_port`
/// (falling back to `[::1]`) for every connection the server announces. `on_hello` runs once
/// the server says where the device reaches the tunnel. Returns when the tunnel closes or
/// `stop` fires.
pub async fn connect(
    ws_url: &str,
    token: &str,
    local_port: u16,
    stop: CancellationToken,
    on_hello: impl FnOnce(&Hello),
) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

    let mut request = ws_url.into_client_request().context("invalid server URL")?;
    request.headers_mut().insert("authorization", HeaderValue::from_str(&format!("Bearer {token}")).context("token")?);
    request.headers_mut().insert("user-agent", HeaderValue::from_static(crate::USER_AGENT));
    let (ws, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(ok) => ok,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            let body = resp.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(String::from))
                .unwrap_or_else(|| body.trim().to_string());
            bail!("the server refused the tunnel ({}): {message}", resp.status());
        }
        Err(e) => return Err(e).context("connecting to the server"),
    };
    let (mut sink, mut stream) = ws.split();
    let hello: Hello = loop {
        match stream.next().await {
            Some(Ok(Message::Text(t))) => break serde_json::from_str(&t).context("reading the server's hello")?,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(other)) => bail!("unexpected first message from the server: {other:?}"),
            Some(Err(e)) => return Err(e).context("reading the server's hello"),
            None => bail!("the server closed the tunnel"),
        }
    };
    on_hello(&hello);
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(QUEUE);
    let mut conns = Conns::default();
    loop {
        tokio::select! {
            Some(out) = out_rx.recv() => {
                if sink.send(Message::Binary(out.into())).await.is_err() {
                    break;
                }
            }
            msg = stream.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    let Some((kind, id, payload)) = parse(&b) else { continue };
                    if kind == OPEN {
                        match dial(local_port).await {
                            Ok(tcp) => conns.start(id, tcp, &out_tx),
                            Err(e) => {
                                eprintln!("seiji forward: nothing answers on localhost:{local_port} ({e})");
                                if sink.send(Message::Binary(frame(CLOSE, id, &[]).into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    } else {
                        conns.deliver(kind, id, payload).await;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e).context("the tunnel broke"),
            },
            _ = stop.cancelled() => {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }
        }
    }
    Ok(())
}

async fn dial(port: u16) -> std::io::Result<TcpStream> {
    match TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => Ok(s),
        Err(_) => TcpStream::connect(("::1", port)).await,
    }
}
