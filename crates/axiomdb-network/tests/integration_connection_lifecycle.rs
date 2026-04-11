use std::{sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use axiomdb_network::mysql::{
    handler::handle_connection_with_timeouts,
    lifecycle::LifecycleTimeouts,
    packets::{
        CLIENT_CONNECT_WITH_DB, CLIENT_INTERACTIVE, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41,
        CLIENT_SECURE_CONNECTION,
    },
    SharedDatabase,
};
use axiomdb_sql::{SchemaCache, SessionContext};

struct TestServer {
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

async fn spawn_server(timeouts: LifecycleTimeouts) -> TestServer {
    spawn_server_with_setup_and_connections(timeouts, &[], 1).await
}

async fn spawn_server_with_setup_and_connections(
    timeouts: LifecycleTimeouts,
    setup_sql: &[&str],
    connections: usize,
) -> TestServer {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = SharedDatabase::open(dir.path()).expect("open test db");
    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();
    for sql in setup_sql {
        db.execute_query(sql, &mut session, &mut cache)
            .unwrap_or_else(|e| panic!("setup SQL failed: {sql}\nError: {e:?}"));
    }
    let db = Arc::new(db);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        for conn_id in 1..=connections {
            let (stream, _) = listener.accept().await.expect("accept");
            handle_connection_with_timeouts(stream, Arc::clone(&db), conn_id as u32, timeouts)
                .await;
        }
    });
    TestServer {
        addr,
        task,
        _dir: dir,
    }
}

async fn spawn_server_with_setup(timeouts: LifecycleTimeouts, setup_sql: &[&str]) -> TestServer {
    spawn_server_with_setup_and_connections(timeouts, setup_sql, 1).await
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

async fn authenticate_with_options(
    stream: &mut TcpStream,
    interactive: bool,
    database: Option<&str>,
    collation_id: u8,
) -> std::io::Result<Vec<u8>> {
    let (_seq, greeting) = read_packet(stream).await?;
    assert_eq!(greeting[0], 10, "server must start with HandshakeV10");

    let mut payload = Vec::new();
    let mut caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
    if interactive {
        caps |= CLIENT_INTERACTIVE;
    }
    if database.is_some() {
        caps |= CLIENT_CONNECT_WITH_DB;
    }
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes()); // max_packet_size
    payload.push(collation_id);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(b"root\0");
    payload.push(0u8); // empty auth response
    if let Some(db) = database {
        payload.extend_from_slice(db.as_bytes());
        payload.push(0u8);
    }
    payload.extend_from_slice(b"caching_sha2_password\0");
    write_packet(stream, 1, &payload).await?;

    let (_seq, auth_more) = read_packet(stream).await?;
    assert_eq!(
        auth_more.as_slice(),
        &[0x01, 0x03],
        "expected fast auth success"
    );

    write_packet(stream, 3, &[]).await?;
    let (_seq, final_packet) = read_packet(stream).await?;
    Ok(final_packet)
}

async fn authenticate_with_database(
    stream: &mut TcpStream,
    interactive: bool,
    database: Option<&str>,
) -> std::io::Result<Vec<u8>> {
    authenticate_with_options(stream, interactive, database, 255).await
}

async fn authenticate(stream: &mut TcpStream, interactive: bool) -> std::io::Result<()> {
    let ok = authenticate_with_options(stream, interactive, None, 255).await?;
    assert_eq!(ok[0], 0x00, "expected OK after auth");
    Ok(())
}

async fn authenticate_with_collation(
    stream: &mut TcpStream,
    interactive: bool,
    collation_id: u8,
) -> std::io::Result<()> {
    let ok = authenticate_with_options(stream, interactive, None, collation_id).await?;
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

async fn com_query_single_text(
    stream: &mut TcpStream,
    sql: &str,
) -> std::io::Result<Option<String>> {
    let mut payload = Vec::with_capacity(1 + sql.len());
    payload.push(0x03);
    payload.extend_from_slice(sql.as_bytes());
    write_packet(stream, 0, &payload).await?;

    let (_seq, col_count) = read_packet(stream).await?;
    assert_eq!(col_count[0], 1, "expected a single-column result set");

    let _ = read_packet(stream).await?; // column definition
    let _ = read_packet(stream).await?; // EOF after columns
    let (_seq, row) = read_packet(stream).await?;
    let _ = read_packet(stream).await?; // EOF after rows

    if row[0] == 0xfb {
        return Ok(None);
    }
    let len = row[0] as usize;
    Ok(Some(String::from_utf8_lossy(&row[1..1 + len]).into_owned()))
}

fn read_lenenc_int(row: &[u8], offset: &mut usize) -> std::io::Result<usize> {
    if *offset >= row.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing length-encoded integer",
        ));
    }
    let first = row[*offset];
    *offset += 1;
    match first {
        0xfc => {
            if *offset + 2 > row.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated 0xfc length-encoded integer",
                ));
            }
            let value = u16::from_le_bytes([row[*offset], row[*offset + 1]]) as usize;
            *offset += 2;
            Ok(value)
        }
        0xfd => {
            if *offset + 3 > row.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated 0xfd length-encoded integer",
                ));
            }
            let value = usize::from(row[*offset])
                | (usize::from(row[*offset + 1]) << 8)
                | (usize::from(row[*offset + 2]) << 16);
            *offset += 3;
            Ok(value)
        }
        0xfe => {
            if *offset + 8 > row.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated 0xfe length-encoded integer",
                ));
            }
            let value = u64::from_le_bytes([
                row[*offset],
                row[*offset + 1],
                row[*offset + 2],
                row[*offset + 3],
                row[*offset + 4],
                row[*offset + 5],
                row[*offset + 6],
                row[*offset + 7],
            ]);
            *offset += 8;
            usize::try_from(value).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "length-encoded integer does not fit in usize",
                )
            })
        }
        0xfb => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "NULL marker is not a valid field length",
        )),
        value => Ok(usize::from(value)),
    }
}

fn read_lenenc_text_field(row: &[u8], offset: &mut usize) -> std::io::Result<Option<String>> {
    if *offset >= row.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing row field",
        ));
    }
    if row[*offset] == 0xfb {
        *offset += 1;
        return Ok(None);
    }
    let len = read_lenenc_int(row, offset)?;
    if *offset + len > row.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "truncated length-encoded string",
        ));
    }
    let text = String::from_utf8_lossy(&row[*offset..*offset + len]).into_owned();
    *offset += len;
    Ok(Some(text))
}

async fn com_query_single_row_texts(
    stream: &mut TcpStream,
    sql: &str,
) -> std::io::Result<Vec<Option<String>>> {
    let mut payload = Vec::with_capacity(1 + sql.len());
    payload.push(0x03);
    payload.extend_from_slice(sql.as_bytes());
    write_packet(stream, 0, &payload).await?;

    let (_seq, col_count) = read_packet(stream).await?;
    let expected_columns = usize::from(col_count[0]);
    for _ in 0..expected_columns {
        let _ = read_packet(stream).await?;
    }
    let _ = read_packet(stream).await?;
    let (_seq, row) = read_packet(stream).await?;
    let _ = read_packet(stream).await?;

    let mut offset = 0usize;
    let mut fields = Vec::with_capacity(expected_columns);
    for _ in 0..expected_columns {
        fields.push(read_lenenc_text_field(&row, &mut offset)?);
    }
    Ok(fields)
}

async fn com_init_db(stream: &mut TcpStream, db_name: &str) -> std::io::Result<Vec<u8>> {
    let mut payload = Vec::with_capacity(1 + db_name.len());
    payload.push(0x02);
    payload.extend_from_slice(db_name.as_bytes());
    write_packet(stream, 0, &payload).await?;
    let (_seq, response) = read_packet(stream).await?;
    Ok(response)
}

async fn com_reset_connection(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    write_packet(stream, 0, &[0x1f]).await?;
    let (_seq, response) = read_packet(stream).await?;
    Ok(response)
}

async fn com_change_user(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    write_packet(stream, 0, &[0x11]).await?;
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

#[tokio::test]
async fn test_reset_connection_rolls_back_active_implicit_txn() {
    let server = spawn_server_with_setup(
        LifecycleTimeouts {
            auth_timeout: Duration::from_millis(200),
        },
        &["CREATE TABLE reset_tx (id INT PRIMARY KEY, v INT)"],
    )
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    authenticate(&mut stream, false).await.expect("auth");

    let ok = com_query(&mut stream, "SET autocommit = 0")
        .await
        .expect("SET autocommit");
    assert_eq!(ok[0], 0x00, "SET autocommit must return OK");

    let ok = com_query(&mut stream, "INSERT INTO reset_tx VALUES (1, 10)")
        .await
        .expect("INSERT starts implicit txn");
    assert_eq!(ok[0], 0x00, "INSERT must return OK");

    let ok = com_reset_connection(&mut stream)
        .await
        .expect("COM_RESET_CONNECTION");
    assert_eq!(ok[0], 0x00, "COM_RESET_CONNECTION must return OK");

    let count = com_query_single_text(&mut stream, "SELECT COUNT(*) FROM reset_tx")
        .await
        .expect("SELECT COUNT after reset");
    assert_eq!(
        count.as_deref(),
        Some("0"),
        "reset must roll back implicit txn"
    );

    let ok = com_query(&mut stream, "INSERT INTO reset_tx VALUES (2, 20)")
        .await
        .expect("INSERT after reset");
    assert_eq!(ok[0], 0x00, "connection must remain writable after reset");

    com_quit(&mut stream).await.expect("COM_QUIT");
    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_reset_connection_clears_selected_database_and_session_defaults() {
    let server = spawn_server_with_setup(
        LifecycleTimeouts {
            auth_timeout: Duration::from_millis(200),
        },
        &["CREATE DATABASE analytics"],
    )
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    authenticate(&mut stream, false).await.expect("auth");

    let ok = com_init_db(&mut stream, "analytics")
        .await
        .expect("COM_INIT_DB analytics");
    assert_eq!(ok[0], 0x00, "COM_INIT_DB must return OK");

    let db = com_query_single_text(&mut stream, "SELECT DATABASE()")
        .await
        .expect("SELECT DATABASE before reset");
    assert_eq!(db.as_deref(), Some("analytics"));

    let ok = com_query(&mut stream, "SET autocommit = 0")
        .await
        .expect("SET autocommit");
    assert_eq!(ok[0], 0x00, "SET autocommit must return OK");

    let autocommit = com_query_single_text(&mut stream, "SELECT @@autocommit")
        .await
        .expect("SELECT @@autocommit before reset");
    assert_eq!(autocommit.as_deref(), Some("0"));

    let ok = com_reset_connection(&mut stream)
        .await
        .expect("COM_RESET_CONNECTION");
    assert_eq!(ok[0], 0x00, "COM_RESET_CONNECTION must return OK");

    let db = com_query_single_text(&mut stream, "SELECT DATABASE()")
        .await
        .expect("SELECT DATABASE after reset");
    assert_eq!(db, None, "reset must clear the selected database");

    let autocommit = com_query_single_text(&mut stream, "SELECT @@autocommit")
        .await
        .expect("SELECT @@autocommit after reset");
    assert_eq!(
        autocommit.as_deref(),
        Some("1"),
        "reset must restore autocommit"
    );

    com_quit(&mut stream).await.expect("COM_QUIT");
    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_reset_connection_restores_handshake_charset_baseline() {
    let server = spawn_server(LifecycleTimeouts {
        auth_timeout: Duration::from_millis(200),
    })
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    authenticate_with_collation(&mut stream, false, 8)
        .await
        .expect("auth latin1");

    let charset = com_query_single_text(&mut stream, "SELECT @@character_set_client")
        .await
        .expect("SELECT @@character_set_client before mutation");
    assert_eq!(charset.as_deref(), Some("latin1"));

    let ok = com_query(&mut stream, "SET NAMES utf8mb4")
        .await
        .expect("SET NAMES utf8mb4");
    assert_eq!(ok[0], 0x00, "SET NAMES must return OK");

    let charset = com_query_single_text(&mut stream, "SELECT @@character_set_client")
        .await
        .expect("SELECT @@character_set_client after SET NAMES");
    assert_eq!(charset.as_deref(), Some("utf8mb4"));

    let ok = com_reset_connection(&mut stream)
        .await
        .expect("COM_RESET_CONNECTION");
    assert_eq!(ok[0], 0x00, "COM_RESET_CONNECTION must return OK");

    let client = com_query_single_text(&mut stream, "SELECT @@character_set_client")
        .await
        .expect("SELECT @@character_set_client after reset");
    assert_eq!(client.as_deref(), Some("latin1"));

    let connection = com_query_single_text(&mut stream, "SELECT @@character_set_connection")
        .await
        .expect("SELECT @@character_set_connection after reset");
    assert_eq!(connection.as_deref(), Some("latin1"));

    let results = com_query_single_text(&mut stream, "SELECT @@character_set_results")
        .await
        .expect("SELECT @@character_set_results after reset");
    assert_eq!(results.as_deref(), Some("latin1"));

    com_quit(&mut stream).await.expect("COM_QUIT");
    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_reset_connection_clears_local_status_counters() {
    let server = spawn_server(LifecycleTimeouts {
        auth_timeout: Duration::from_millis(200),
    })
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    authenticate(&mut stream, false).await.expect("auth");

    for _ in 0..3 {
        let value = com_query_single_text(&mut stream, "SELECT 1")
            .await
            .expect("SELECT 1");
        assert_eq!(value.as_deref(), Some("1"));
    }

    let before = com_query_single_row_texts(&mut stream, "SHOW LOCAL STATUS LIKE 'com_select'")
        .await
        .expect("SHOW LOCAL STATUS before reset");
    assert_eq!(before.len(), 2, "status row must have name and value");
    assert_eq!(before[0].as_deref(), Some("Com_select"));
    assert_eq!(before[1].as_deref(), Some("3"));

    let ok = com_reset_connection(&mut stream)
        .await
        .expect("COM_RESET_CONNECTION");
    assert_eq!(ok[0], 0x00, "COM_RESET_CONNECTION must return OK");

    let after = com_query_single_row_texts(&mut stream, "SHOW LOCAL STATUS LIKE 'com_select'")
        .await
        .expect("SHOW LOCAL STATUS after reset");
    assert_eq!(after.len(), 2, "status row must have name and value");
    assert_eq!(after[0].as_deref(), Some("Com_select"));
    assert_eq!(after[1].as_deref(), Some("0"));

    com_quit(&mut stream).await.expect("COM_QUIT");
    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_change_user_resets_session_state_and_preserves_handshake_charset() {
    let server = spawn_server_with_setup(
        LifecycleTimeouts {
            auth_timeout: Duration::from_millis(200),
        },
        &["CREATE DATABASE analytics"],
    )
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    authenticate_with_collation(&mut stream, false, 8)
        .await
        .expect("auth latin1");

    let ok = com_init_db(&mut stream, "analytics")
        .await
        .expect("COM_INIT_DB analytics");
    assert_eq!(ok[0], 0x00, "COM_INIT_DB must return OK");

    let ok = com_query(&mut stream, "SET autocommit = 0")
        .await
        .expect("SET autocommit");
    assert_eq!(ok[0], 0x00, "SET autocommit must return OK");

    let ok = com_query(&mut stream, "SET NAMES utf8mb4")
        .await
        .expect("SET NAMES utf8mb4");
    assert_eq!(ok[0], 0x00, "SET NAMES must return OK");

    let ok = com_change_user(&mut stream).await.expect("COM_CHANGE_USER");
    assert_eq!(ok[0], 0x00, "COM_CHANGE_USER must return OK");

    let db = com_query_single_text(&mut stream, "SELECT DATABASE()")
        .await
        .expect("SELECT DATABASE after change user");
    assert_eq!(db, None, "change user must clear the selected database");

    let autocommit = com_query_single_text(&mut stream, "SELECT @@autocommit")
        .await
        .expect("SELECT @@autocommit after change user");
    assert_eq!(
        autocommit.as_deref(),
        Some("1"),
        "change user must restore autocommit"
    );

    let charset = com_query_single_text(&mut stream, "SELECT @@character_set_client")
        .await
        .expect("SELECT @@character_set_client after change user");
    assert_eq!(
        charset.as_deref(),
        Some("latin1"),
        "change user must restore the handshake charset baseline"
    );

    com_quit(&mut stream).await.expect("COM_QUIT");
    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_com_quit_rolls_back_active_implicit_txn() {
    let server = spawn_server_with_setup_and_connections(
        LifecycleTimeouts {
            auth_timeout: Duration::from_millis(200),
        },
        &["CREATE TABLE quit_tx (id INT PRIMARY KEY, v INT)"],
        2,
    )
    .await;

    let mut stream1 = TcpStream::connect(server.addr)
        .await
        .expect("connect first");
    authenticate(&mut stream1, false).await.expect("auth first");
    let ok = com_query(&mut stream1, "SET autocommit = 0")
        .await
        .expect("SET autocommit first");
    assert_eq!(ok[0], 0x00, "SET autocommit must return OK");
    let ok = com_query(&mut stream1, "INSERT INTO quit_tx VALUES (1, 10)")
        .await
        .expect("INSERT first");
    assert_eq!(ok[0], 0x00, "INSERT must return OK");
    com_quit(&mut stream1).await.expect("COM_QUIT first");
    drop(stream1);

    let mut stream2 = TcpStream::connect(server.addr)
        .await
        .expect("connect second");
    authenticate(&mut stream2, false)
        .await
        .expect("auth second");
    let count = com_query_single_text(&mut stream2, "SELECT COUNT(*) FROM quit_tx")
        .await
        .expect("SELECT COUNT second");
    assert_eq!(
        count.as_deref(),
        Some("0"),
        "COM_QUIT must roll back implicit txn"
    );

    let ok = com_query(&mut stream2, "INSERT INTO quit_tx VALUES (2, 20)")
        .await
        .expect("INSERT second");
    assert_eq!(ok[0], 0x00, "writer must be released after COM_QUIT");

    com_quit(&mut stream2).await.expect("COM_QUIT second");
    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_disconnect_rolls_back_active_implicit_txn() {
    let server = spawn_server_with_setup_and_connections(
        LifecycleTimeouts {
            auth_timeout: Duration::from_millis(200),
        },
        &["CREATE TABLE close_tx (id INT PRIMARY KEY, v INT)"],
        2,
    )
    .await;

    let mut stream1 = TcpStream::connect(server.addr)
        .await
        .expect("connect first");
    authenticate(&mut stream1, false).await.expect("auth first");
    let ok = com_query(&mut stream1, "SET autocommit = 0")
        .await
        .expect("SET autocommit first");
    assert_eq!(ok[0], 0x00, "SET autocommit must return OK");
    let ok = com_query(&mut stream1, "INSERT INTO close_tx VALUES (1, 10)")
        .await
        .expect("INSERT first");
    assert_eq!(ok[0], 0x00, "INSERT must return OK");
    drop(stream1);

    let mut stream2 = TcpStream::connect(server.addr)
        .await
        .expect("connect second");
    authenticate(&mut stream2, false)
        .await
        .expect("auth second");
    let count = com_query_single_text(&mut stream2, "SELECT COUNT(*) FROM close_tx")
        .await
        .expect("SELECT COUNT second");
    assert_eq!(
        count.as_deref(),
        Some("0"),
        "socket disconnect must roll back implicit txn"
    );

    let ok = com_query(&mut stream2, "INSERT INTO close_tx VALUES (2, 20)")
        .await
        .expect("INSERT second");
    assert_eq!(ok[0], 0x00, "writer must be released after disconnect");

    com_quit(&mut stream2).await.expect("COM_QUIT second");
    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_handshake_database_sets_current_database_visible_to_database_function() {
    let server = spawn_server_with_setup(
        LifecycleTimeouts {
            auth_timeout: Duration::from_millis(200),
        },
        &["CREATE DATABASE analytics"],
    )
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");

    let ok = authenticate_with_database(&mut stream, false, Some("analytics"))
        .await
        .expect("auth with initial database");
    assert_eq!(ok[0], 0x00, "auth must finish with OK");

    let db = com_query_single_text(&mut stream, "SELECT DATABASE()")
        .await
        .expect("SELECT DATABASE()");
    assert_eq!(db.as_deref(), Some("analytics"));

    com_quit(&mut stream).await.expect("COM_QUIT");
    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_handshake_unknown_database_returns_1049() {
    let server = spawn_server(LifecycleTimeouts {
        auth_timeout: Duration::from_millis(200),
    })
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");

    let err = authenticate_with_database(&mut stream, false, Some("missing_db"))
        .await
        .expect("auth with unknown database");
    assert_eq!(err[0], 0xff, "unknown database must return ERR");
    assert_eq!(
        u16::from_le_bytes([err[1], err[2]]),
        1049,
        "expected ER_BAD_DB_ERROR"
    );

    server.task.await.expect("server task");
}

#[tokio::test]
async fn test_com_init_db_unknown_database_returns_1049() {
    let server = spawn_server(LifecycleTimeouts {
        auth_timeout: Duration::from_millis(200),
    })
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    authenticate(&mut stream, false).await.expect("auth");

    let err = com_init_db(&mut stream, "missing_db")
        .await
        .expect("COM_INIT_DB response");
    assert_eq!(err[0], 0xff, "unknown database must return ERR");
    assert_eq!(
        u16::from_le_bytes([err[1], err[2]]),
        1049,
        "expected ER_BAD_DB_ERROR"
    );

    com_quit(&mut stream).await.expect("COM_QUIT");
    server.task.await.expect("server task");
}
