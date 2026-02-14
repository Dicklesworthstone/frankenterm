#![cfg(unix)]

use frankenterm_core::runtime_compat::{self, unix};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn socket_path(test_name: &str) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("frankenterm-{test_name}-{ts}.sock"))
}

#[tokio::test]
async fn bind_replaces_stale_socket_path() -> io::Result<()> {
    let socket_path = socket_path("runtime-compat-stale-bind");
    std::fs::write(&socket_path, b"stale-file")?;

    let listener = unix::bind(&socket_path).await?;

    let server = async {
        let (_stream, _addr) = listener.accept().await?;
        Ok::<(), io::Error>(())
    };
    let client_path = socket_path.clone();
    let client = async move {
        let _stream = unix::connect(&client_path).await?;
        Ok::<(), io::Error>(())
    };

    let (server_res, client_res) = tokio::join!(server, client);
    server_res?;
    client_res?;
    Ok(())
}

#[tokio::test]
async fn unix_socket_round_trip_read_write() -> io::Result<()> {
    let socket_path = socket_path("runtime-compat-round-trip");
    let listener = unix::bind(&socket_path).await?;

    let server = async {
        let (mut stream, _addr) = listener.accept().await?;
        let mut inbound = [0_u8; 4];
        stream.read_exact(&mut inbound).await?;
        assert_eq!(&inbound, b"ping");
        stream.write_all(b"pong").await?;
        Ok::<(), io::Error>(())
    };

    let client_path = socket_path.clone();
    let client = async move {
        let mut stream = unix::connect(&client_path).await?;
        stream.write_all(b"ping").await?;
        let mut outbound = [0_u8; 4];
        stream.read_exact(&mut outbound).await?;
        assert_eq!(&outbound, b"pong");
        Ok::<(), io::Error>(())
    };

    let (server_res, client_res) = tokio::join!(server, client);
    server_res?;
    client_res?;
    Ok(())
}

#[tokio::test]
async fn unix_socket_line_delimited_reading() -> io::Result<()> {
    let socket_path = socket_path("runtime-compat-lines");
    let listener = unix::bind(&socket_path).await?;

    let server = async {
        let (mut stream, _addr) = listener.accept().await?;
        stream.write_all(b"alpha\nbeta\r\ngamma").await?;
        Ok::<(), io::Error>(())
    };

    let client_path = socket_path.clone();
    let client = async move {
        let stream = unix::connect(&client_path).await?;
        let mut lines = unix::lines(unix::buffered(stream));

        assert_eq!(
            unix::next_line(&mut lines).await?,
            Some("alpha".to_string())
        );
        assert_eq!(unix::next_line(&mut lines).await?, Some("beta".to_string()));
        assert_eq!(
            unix::next_line(&mut lines).await?,
            Some("gamma".to_string())
        );
        assert_eq!(unix::next_line(&mut lines).await?, None);
        Ok::<(), io::Error>(())
    };

    let (server_res, client_res) = tokio::join!(server, client);
    server_res?;
    client_res?;
    Ok(())
}

#[tokio::test]
async fn unix_socket_read_timeout_is_enforced() -> io::Result<()> {
    let socket_path = socket_path("runtime-compat-timeout");
    let listener = unix::bind(&socket_path).await?;

    let server = async {
        let (_stream, _addr) = listener.accept().await?;
        runtime_compat::sleep(Duration::from_millis(150)).await;
        Ok::<(), io::Error>(())
    };

    let client_path = socket_path.clone();
    let client = async move {
        let mut stream = unix::connect(&client_path).await?;
        let mut byte = [0_u8; 1];
        let timed =
            runtime_compat::timeout(Duration::from_millis(30), stream.read_exact(&mut byte)).await;
        assert!(
            timed.is_err(),
            "expected timeout error when peer stays idle, got: {timed:?}"
        );
        Ok::<(), io::Error>(())
    };

    let (server_res, client_res) = tokio::join!(server, client);
    server_res?;
    client_res?;
    Ok(())
}
