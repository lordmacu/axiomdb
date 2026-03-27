//! Explicit transport lifecycle for MySQL connections.
//!
//! Keeps socket/lifecycle concerns separate from `ConnectionState`, which owns
//! SQL session state. The lifecycle layer is responsible for:
//! - connection phase tracking
//! - timeout selection per phase
//! - TCP socket configuration (NODELAY + keepalive)
//! - timeout-wrapped packet reads and writes

use std::{io, time::Duration};

use futures::{SinkExt, StreamExt};
use socket2::{SockRef, TcpKeepalive};
use tokio::{
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    time::timeout,
};
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::warn;

use axiomdb_core::error::DbError;

use super::{
    codec::{MySqlCodec, MySqlCodecError, Packet},
    packets::CLIENT_INTERACTIVE,
    session::ConnectionState,
};

pub type MySqlReader = FramedRead<OwnedReadHalf, MySqlCodec>;
pub type MySqlWriter = FramedWrite<OwnedWriteHalf, MySqlCodec>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPhase {
    Connected,
    Auth,
    Idle,
    Executing,
    Closing,
}

#[derive(Debug, Clone, Copy)]
pub struct LifecycleTimeouts {
    pub auth_timeout: Duration,
}

impl Default for LifecycleTimeouts {
    fn default() -> Self {
        Self {
            auth_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ConnectionLifecycle {
    phase: ConnectionPhase,
    client_capability_flags: u32,
    timeouts: LifecycleTimeouts,
}

impl Default for ConnectionLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionLifecycle {
    pub fn new() -> Self {
        Self {
            phase: ConnectionPhase::Connected,
            client_capability_flags: 0,
            timeouts: LifecycleTimeouts::default(),
        }
    }

    pub fn with_timeouts(timeouts: LifecycleTimeouts) -> Self {
        Self {
            timeouts,
            ..Self::new()
        }
    }

    pub fn phase(&self) -> ConnectionPhase {
        self.phase
    }

    pub fn enter(&mut self, phase: ConnectionPhase) {
        self.phase = phase;
    }

    pub fn close(&mut self) {
        self.phase = ConnectionPhase::Closing;
    }

    pub fn set_client_capability_flags(&mut self, flags: u32) {
        self.client_capability_flags = flags;
    }

    pub fn client_capability_flags(&self) -> u32 {
        self.client_capability_flags
    }

    pub fn is_interactive(&self) -> bool {
        self.client_capability_flags & CLIENT_INTERACTIVE != 0
    }

    pub fn auth_timeout(&self) -> Duration {
        self.timeouts.auth_timeout
    }

    pub fn idle_timeout(&self, session: &ConnectionState) -> Result<Duration, DbError> {
        let secs = if self.is_interactive() {
            session.interactive_timeout_secs()?
        } else {
            session.wait_timeout_secs()?
        };
        Ok(Duration::from_secs(secs))
    }

    pub fn execute_read_timeout(&self, session: &ConnectionState) -> Result<Duration, DbError> {
        Ok(Duration::from_secs(session.net_read_timeout_secs()?))
    }

    pub fn execute_write_timeout(&self, session: &ConnectionState) -> Result<Duration, DbError> {
        Ok(Duration::from_secs(session.net_write_timeout_secs()?))
    }
}

#[derive(Debug)]
pub enum ConnectionIoError {
    Timeout(ConnectionPhase),
    Read(MySqlCodecError),
    Write(io::Error),
    InvalidConfig(DbError),
    Closed,
}

impl std::fmt::Display for ConnectionIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(phase) => write!(f, "connection timeout while in {phase:?}"),
            Self::Read(e) => write!(f, "connection read error: {e}"),
            Self::Write(e) => write!(f, "connection write error: {e}"),
            Self::InvalidConfig(e) => write!(f, "invalid connection timeout config: {e}"),
            Self::Closed => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for ConnectionIoError {}

pub fn configure_client_socket(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;

    let sock = SockRef::from(stream);
    sock.set_keepalive(true)?;

    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(30));

    if let Err(e) = sock.set_tcp_keepalive(&keepalive) {
        warn!(err = %e, "tcp keepalive tuning unsupported; using SO_KEEPALIVE only");
    }

    Ok(())
}

async fn read_packet_with_timeout(
    reader: &mut MySqlReader,
    timeout_dur: Duration,
    phase: ConnectionPhase,
) -> Result<Packet, ConnectionIoError> {
    match timeout(timeout_dur, reader.next()).await {
        Err(_) => Err(ConnectionIoError::Timeout(phase)),
        Ok(Some(Ok(packet))) => Ok(packet),
        Ok(Some(Err(e))) => Err(ConnectionIoError::Read(e)),
        Ok(None) => Err(ConnectionIoError::Closed),
    }
}

async fn send_packet_with_timeout(
    writer: &mut MySqlWriter,
    seq: u8,
    payload: &[u8],
    timeout_dur: Duration,
    phase: ConnectionPhase,
) -> Result<(), ConnectionIoError> {
    match timeout(timeout_dur, writer.send((seq, payload))).await {
        Err(_) => Err(ConnectionIoError::Timeout(phase)),
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(ConnectionIoError::Write(e)),
    }
}

pub async fn read_auth_packet(
    reader: &mut MySqlReader,
    lifecycle: &ConnectionLifecycle,
) -> Result<Packet, ConnectionIoError> {
    read_packet_with_timeout(reader, lifecycle.auth_timeout(), ConnectionPhase::Auth).await
}

pub async fn read_idle_packet(
    reader: &mut MySqlReader,
    lifecycle: &ConnectionLifecycle,
    session: &ConnectionState,
) -> Result<Packet, ConnectionIoError> {
    let timeout_dur = lifecycle
        .idle_timeout(session)
        .map_err(ConnectionIoError::InvalidConfig)?;
    read_packet_with_timeout(reader, timeout_dur, ConnectionPhase::Idle).await
}

pub async fn read_execute_packet(
    reader: &mut MySqlReader,
    lifecycle: &ConnectionLifecycle,
    session: &ConnectionState,
) -> Result<Packet, ConnectionIoError> {
    let timeout_dur = lifecycle
        .execute_read_timeout(session)
        .map_err(ConnectionIoError::InvalidConfig)?;
    read_packet_with_timeout(reader, timeout_dur, ConnectionPhase::Executing).await
}

pub async fn send_auth_packet(
    writer: &mut MySqlWriter,
    lifecycle: &ConnectionLifecycle,
    seq: u8,
    payload: &[u8],
) -> Result<(), ConnectionIoError> {
    send_packet_with_timeout(
        writer,
        seq,
        payload,
        lifecycle.auth_timeout(),
        ConnectionPhase::Auth,
    )
    .await
}

pub async fn send_execute_packet(
    writer: &mut MySqlWriter,
    lifecycle: &ConnectionLifecycle,
    session: &ConnectionState,
    seq: u8,
    payload: &[u8],
) -> Result<(), ConnectionIoError> {
    let timeout_dur = lifecycle
        .execute_write_timeout(session)
        .map_err(ConnectionIoError::InvalidConfig)?;
    send_packet_with_timeout(
        writer,
        seq,
        payload,
        timeout_dur,
        ConnectionPhase::Executing,
    )
    .await
}

pub async fn send_packet_batch(
    writer: &mut MySqlWriter,
    lifecycle: &ConnectionLifecycle,
    session: &ConnectionState,
    packets: &[(u8, Vec<u8>)],
) -> Result<(), ConnectionIoError> {
    let timeout_dur = lifecycle
        .execute_write_timeout(session)
        .map_err(ConnectionIoError::InvalidConfig)?;
    let result = timeout(timeout_dur, async {
        let n = packets.len();
        for (i, (seq, pkt)) in packets.iter().enumerate() {
            if i + 1 < n {
                writer.feed((*seq, pkt.as_slice())).await?;
            } else {
                writer.send((*seq, pkt.as_slice())).await?;
            }
        }
        Ok::<(), io::Error>(())
    })
    .await;

    match result {
        Err(_) => Err(ConnectionIoError::Timeout(ConnectionPhase::Executing)),
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(ConnectionIoError::Write(e)),
    }
}
