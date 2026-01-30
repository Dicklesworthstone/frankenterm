use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::config as wa_config;
use codec::{
    CODEC_VERSION, DecodedPdu, GetCodecVersion, GetCodecVersionResponse, ListPanes,
    ListPanesResponse, Pdu, SetClientId, UnitResponse,
};
use config as wezterm_config;
use mux::client::ClientId;

const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_READ_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_WRITE_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DirectMuxClientConfig {
    pub socket_path: Option<PathBuf>,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub max_frame_bytes: usize,
}

impl DirectMuxClientConfig {
    pub fn from_wa_config(config: &wa_config::Config) -> Self {
        let mut cfg = Self::default();
        if let Some(path) = &config.vendored.mux_socket_path {
            if !path.trim().is_empty() {
                cfg.socket_path = Some(PathBuf::from(path));
            }
        }
        cfg
    }

    #[must_use]
    pub fn with_socket_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket_path = Some(path.into());
        self
    }
}

impl Default for DirectMuxClientConfig {
    fn default() -> Self {
        Self {
            socket_path: None,
            connect_timeout: Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS),
            read_timeout: Duration::from_millis(DEFAULT_READ_TIMEOUT_MS),
            write_timeout: Duration::from_millis(DEFAULT_WRITE_TIMEOUT_MS),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DirectMuxError {
    #[error("mux socket path not found; set WEZTERM_UNIX_SOCKET or wa vendored.mux_socket_path")]
    SocketPathMissing,
    #[error("mux socket not found at {0}")]
    SocketNotFound(PathBuf),
    #[error("mux proxy command not supported for direct client")]
    ProxyUnsupported,
    #[error("connect to mux socket timed out: {0}")]
    ConnectTimeout(PathBuf),
    #[error("read from mux socket timed out")]
    ReadTimeout,
    #[error("write to mux socket timed out")]
    WriteTimeout,
    #[error("mux socket disconnected")]
    Disconnected,
    #[error("frame exceeded max size ({max_bytes} bytes)")]
    FrameTooLarge { max_bytes: usize },
    #[error("codec error: {0}")]
    Codec(String),
    #[error("remote error: {0}")]
    RemoteError(String),
    #[error("unexpected response: expected {expected}, got {got}")]
    UnexpectedResponse { expected: String, got: String },
    #[error("codec version mismatch: local {local} != remote {remote} (version {remote_version})")]
    IncompatibleCodec {
        local: usize,
        remote: usize,
        remote_version: String,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct DirectMuxClient {
    stream: UnixStream,
    socket_path: PathBuf,
    read_buf: Vec<u8>,
    serial: u64,
    config: DirectMuxClientConfig,
}

impl DirectMuxClient {
    pub async fn connect(config: DirectMuxClientConfig) -> Result<Self, DirectMuxError> {
        let socket_path = resolve_socket_path(&config)?;
        if !socket_path.exists() {
            return Err(DirectMuxError::SocketNotFound(socket_path));
        }

        let stream =
            tokio::time::timeout(config.connect_timeout, UnixStream::connect(&socket_path))
                .await
                .map_err(|_| DirectMuxError::ConnectTimeout(socket_path.clone()))??;

        let mut client = Self {
            stream,
            socket_path,
            read_buf: Vec::new(),
            serial: 0,
            config,
        };

        client.verify_codec_version().await?;
        client.register_client().await?;

        Ok(client)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn list_panes(&mut self) -> Result<ListPanesResponse, DirectMuxError> {
        let response = self.send_request(Pdu::ListPanes(ListPanes {})).await?;
        match response {
            Pdu::ListPanesResponse(payload) => Ok(payload),
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "ListPanesResponse".to_string(),
                got: other.pdu_name().to_string(),
            }),
        }
    }

    async fn verify_codec_version(&mut self) -> Result<GetCodecVersionResponse, DirectMuxError> {
        let response = self
            .send_request(Pdu::GetCodecVersion(GetCodecVersion {}))
            .await?;
        match response {
            Pdu::GetCodecVersionResponse(payload) => {
                if payload.codec_vers != CODEC_VERSION {
                    return Err(DirectMuxError::IncompatibleCodec {
                        local: CODEC_VERSION,
                        remote: payload.codec_vers,
                        remote_version: payload.version_string.clone(),
                    });
                }
                Ok(payload)
            }
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "GetCodecVersionResponse".to_string(),
                got: other.pdu_name().to_string(),
            }),
        }
    }

    async fn register_client(&mut self) -> Result<UnitResponse, DirectMuxError> {
        let client_id = ClientId::new();
        let response = self
            .send_request(Pdu::SetClientId(SetClientId {
                client_id,
                is_proxy: false,
            }))
            .await?;
        match response {
            Pdu::UnitResponse(payload) => Ok(payload),
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "UnitResponse".to_string(),
                got: other.pdu_name().to_string(),
            }),
        }
    }

    async fn send_request(&mut self, pdu: Pdu) -> Result<Pdu, DirectMuxError> {
        self.serial = self.serial.wrapping_add(1).max(1);
        let serial = self.serial;

        let mut buf = Vec::new();
        pdu.encode(&mut buf, serial)
            .map_err(|err| DirectMuxError::Codec(err.to_string()))?;

        tokio::time::timeout(self.config.write_timeout, self.stream.write_all(&buf))
            .await
            .map_err(|_| DirectMuxError::WriteTimeout)??;

        self.await_response(serial).await
    }

    async fn await_response(&mut self, serial: u64) -> Result<Pdu, DirectMuxError> {
        loop {
            let decoded = self.read_next_pdu().await?;
            if decoded.serial != serial {
                continue;
            }
            return match decoded.pdu {
                Pdu::ErrorResponse(err) => Err(DirectMuxError::RemoteError(err.reason)),
                other => Ok(other),
            };
        }
    }

    async fn read_next_pdu(&mut self) -> Result<DecodedPdu, DirectMuxError> {
        loop {
            if let Some(decoded) =
                decode_from_buffer(&mut self.read_buf, self.config.max_frame_bytes)?
            {
                return Ok(decoded);
            }

            let mut temp = vec![0u8; 4096];
            let read = tokio::time::timeout(self.config.read_timeout, self.stream.read(&mut temp))
                .await
                .map_err(|_| DirectMuxError::ReadTimeout)??;
            if read == 0 {
                return Err(DirectMuxError::Disconnected);
            }
            self.read_buf.extend_from_slice(&temp[..read]);
            if self.read_buf.len() > self.config.max_frame_bytes {
                return Err(DirectMuxError::FrameTooLarge {
                    max_bytes: self.config.max_frame_bytes,
                });
            }
        }
    }
}

fn decode_from_buffer(
    buffer: &mut Vec<u8>,
    max_frame_bytes: usize,
) -> Result<Option<DecodedPdu>, DirectMuxError> {
    if buffer.len() > max_frame_bytes {
        return Err(DirectMuxError::FrameTooLarge {
            max_bytes: max_frame_bytes,
        });
    }
    codec::Pdu::stream_decode(buffer).map_err(|err| DirectMuxError::Codec(err.to_string()))
}

fn resolve_socket_path(config: &DirectMuxClientConfig) -> Result<PathBuf, DirectMuxError> {
    if let Some(path) = &config.socket_path {
        return Ok(path.clone());
    }

    if let Some(path) = std::env::var_os("WEZTERM_UNIX_SOCKET") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }

    let handle = wezterm_config::configuration_result()
        .unwrap_or_else(|_| wezterm_config::ConfigHandle::default_config());
    if let Some(domain) = handle.unix_domains.first() {
        if domain.proxy_command.is_some() {
            return Err(DirectMuxError::ProxyUnsupported);
        }
        return Ok(domain.socket_path());
    }

    let mut default_domains = wezterm_config::UnixDomain::default_unix_domains();
    if let Some(domain) = default_domains.pop() {
        return Ok(domain.socket_path());
    }

    Err(DirectMuxError::SocketPathMissing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn decode_from_buffer_roundtrip() {
        let mut buf = Vec::new();
        let pdu = Pdu::Ping(codec::Ping {});
        pdu.encode(&mut buf, 42).expect("encode should succeed");

        let mut partial = buf[..buf.len() / 2].to_vec();
        let result = decode_from_buffer(&mut partial, 1024).expect("decode should not error");
        assert!(result.is_none());

        partial.extend_from_slice(&buf[buf.len() / 2..]);
        let decoded = decode_from_buffer(&mut partial, 1024)
            .expect("decode should succeed")
            .expect("should decode");
        assert_eq!(decoded.serial, 42);
    }

    #[test]
    fn decode_from_buffer_rejects_oversize() {
        let mut buf = vec![0u8; 10];
        let err = decode_from_buffer(&mut buf, 4).expect_err("should reject oversize buffer");
        match err {
            DirectMuxError::FrameTooLarge { .. } => {}
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn list_panes_roundtrip() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = temp_dir.path().join("mux.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind listener");

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut read_buf = Vec::new();
            let mut responses: HashMap<u64, Pdu> = HashMap::new();
            loop {
                let mut temp = vec![0u8; 4096];
                let read = stream.read(&mut temp).await.expect("read");
                if read == 0 {
                    break;
                }
                read_buf.extend_from_slice(&temp[..read]);
                while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                    let response = match decoded.pdu {
                        Pdu::GetCodecVersion(_) => {
                            let payload = GetCodecVersionResponse {
                                codec_vers: CODEC_VERSION,
                                version_string: "wezterm-test".to_string(),
                                executable_path: PathBuf::from("/bin/wezterm"),
                                config_file_path: None,
                            };
                            Pdu::GetCodecVersionResponse(payload)
                        }
                        Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                        Pdu::ListPanes(_) => {
                            let payload = ListPanesResponse {
                                tabs: Vec::new(),
                                tab_titles: Vec::new(),
                                window_titles: HashMap::new(),
                            };
                            Pdu::ListPanesResponse(payload)
                        }
                        _ => continue,
                    };
                    responses.insert(decoded.serial, response);
                }

                for (serial, pdu) in responses.drain() {
                    let mut out = Vec::new();
                    pdu.encode(&mut out, serial).expect("encode response");
                    stream.write_all(&out).await.expect("write response");
                }
            }
        });

        let mut config = DirectMuxClientConfig::default();
        config.socket_path = Some(socket_path);
        let mut client = DirectMuxClient::connect(config).await.expect("connect");
        let panes = client.list_panes().await.expect("list panes");
        assert!(panes.tabs.is_empty());
    }
}
