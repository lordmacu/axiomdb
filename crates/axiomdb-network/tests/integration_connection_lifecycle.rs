use std::{sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

use axiomdb_network::mysql::{
    handler::handle_connection_with_timeouts,
    lifecycle::LifecycleTimeouts,
    packets::{
        CLIENT_INTERACTIVE, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
    },
    Database,
};

struct TestServer {
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

async fn spawn_server(timeouts: LifecycleTimeouts) -> TestServer {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(Mutex::new(
        Database::open(dir.path()).expect("open test db"),
    ));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        handle_connection_with_timeouts(stream, db, 1, timeouts).await;
    });
    TestServer {
        addr,
        task,
        _dir: dir,
    }
}

async fn read_packet(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    let seq = header[3];
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok((seq, payload))
}

async fn write_packet(stream: &mut TcpStream, seq: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    let header = [
        (len & 0xFF) as u8,
        ((len >> 8) & 0xFF) as u8,
        ((len >> 16) & 0xFF) as u8,
        seq,
    ];
    stream.write_all(&header).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn authenticate(stream: &mut TcpStream, interactive: bool) -> std::io::Result<()> {
    let (_seq, greeting) = read_packet(stream).await?;
    assert_eq!(greeting[0], 10, "server must start with HandshakeV10");

    let mut payload = Vec::new();
    let mut caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
    if interactive {
        caps |= CLIENT_INTERACTIVE;
    }
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes()); // max_packet_size
    payload.push(255u8); // utf8mb4 collation id
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(b"root\0");
    payload.push(0u8); // empty auth response
    payload.extend_from_slice(b"caching_sha2_password\0");
    write_packet(stream, 1, &payload).await?;

    let (_seq, auth_more) = read_packet(stream).await?;
    assert_eq!(
        auth_more.as_slice(),
        &[0x01, 0x03],
        "expected fast auth success"
    );

    write_packet(stream, 3, &[]).await?;
    let (_seq, ok) = read_packet(stream).await?;
    assert_eq!(ok[0], 0x00, "expected OK after auth");
    Ok(())
}

async fn com_query(stream: &mut TcpStream, sql: &str) -> std::io::Result<Vec<u8>> {
    let mut payload = Vec::with_capacity(1 + sql.len());
    payload.push(0x03);
    payload.extend_from_slice(sql.as_bytes());
    write_packet(stream, 0, &payload).await?;
    let (_seq, response) = read_packet(stream).await?;
    Ok(response)
}

async fn com_reset_connection(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    write_packet(stream, 0, &[0x1f]).await?;
    let (_seq, response) = read_packet(stream).await?;
    Ok(response)
}

async fn com_ping(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    write_packet(stream, 0, &[0x0e]).await?;
    let (_seq, response) = read_packet(stream).await?;
    Ok(response)
}

async fn com_quit(stream: &mut TcpStream) -> std::io::Result<()> {
    write_packet(stream, 0, &[0x01]).await
}

#[tokio::test]
async fn test_auth_timeout_closes_unauthenticated_connection() {
    let server = spawn_server(LifecycleTimeouts {
        auth_timeout: Duration::from_millis(50),
    })
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    let _ = read_packet(&mut stream).await.expect("greeting");
    tokio::time::sleep(Duration::from_millis(120)).await;

    let mut buf = [0u8; 32];
    let n = tokio::time::timeout(Duration::from_millis(300), stream.read(&mut buf))
        .await
        .expect("auth timeout read must finish")
        .expect("socket read");
    assert_eq!(n, 0, "server must close the connection after auth timeout");

    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_idle_timeout_closes_non_interactive_connection() {
    let server = spawn_server(LifecycleTimeouts {
        auth_timeout: Duration::from_millis(200),
    })
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    authenticate(&mut stream, false).await.expect("auth");

    let ok = com_query(&mut stream, "SET wait_timeout = 1")
        .await
        .expect("SET wait_timeout");
    assert_eq!(ok[0], 0x00, "SET wait_timeout must return OK");

    tokio::time::sleep(Duration::from_millis(1200)).await;
    let mut buf = [0u8; 32];
    let n = tokio::time::timeout(Duration::from_millis(300), stream.read(&mut buf))
        .await
        .expect("idle-timeout read must finish")
        .expect("socket read");
    assert_eq!(
        n, 0,
        "non-interactive connection must close on wait_timeout"
    );

    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_reset_connection_preserves_interactive_classification() {
    let server = spawn_server(LifecycleTimeouts {
        auth_timeout: Duration::from_millis(200),
    })
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    authenticate(&mut stream, true).await.expect("auth");

    let ok = com_query(&mut stream, "SET wait_timeout = 1")
        .await
        .expect("SET wait_timeout");
    assert_eq!(ok[0], 0x00);

    let ok = com_reset_connection(&mut stream)
        .await
        .expect("COM_RESET_CONNECTION");
    assert_eq!(ok[0], 0x00, "COM_RESET_CONNECTION must return OK");

    let ok = com_query(&mut stream, "SET wait_timeout = 1")
        .await
        .expect("SET wait_timeout after reset");
    assert_eq!(ok[0], 0x00);

    tokio::time::sleep(Duration::from_millis(1200)).await;
    let ok = com_ping(&mut stream)
        .await
        .expect("interactive connection must stay open");
    assert_eq!(
        ok[0], 0x00,
        "interactive connection must still answer COM_PING"
    );

    com_quit(&mut stream).await.expect("COM_QUIT");
    server.task.await.expect("server task");
}
