#![cfg(feature = "distributed")]

use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::{TcpListener, TcpStream};
use asupersync::tls::TlsConnector;
use frankenterm_core::config::{DistributedAuthMode, DistributedConfig, DistributedTlsConfig};
use frankenterm_core::distributed::build_tls_bundle;
use frankenterm_core::runtime_async::{self, CompatRuntime, RuntimeBuilder, task};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, IsCa, Issuer,
    KeyPair, KeyUsagePurpose,
};
use rustls::{ClientConfig, RootCertStore};
use rustls_pemfile::certs;
use std::error::Error;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;
use tempfile::NamedTempFile;

type BoxError = Box<dyn Error + Send + Sync + 'static>;
type TestResult<T = ()> = Result<T, BoxError>;

struct PemFile {
    path: String,
    _file: NamedTempFile,
}

struct TestPki {
    ca: PemFile,
    alt_ca: PemFile,
    server_cert: PemFile,
    server_key: PemFile,
    client_cert: PemFile,
    client_key: PemFile,
}

struct GeneratedPem {
    cert_pem: String,
    key_pem: String,
}

fn write_pem(contents: &str) -> TestResult<PemFile> {
    let mut file = NamedTempFile::new()?;
    file.write_all(contents.as_bytes())?;
    file.flush()?;
    let path = file.path().display().to_string();
    Ok(PemFile { path, _file: file })
}

fn ca_cert(common_name: &str) -> TestResult<(CertificateParams, KeyPair, Certificate)> {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
    params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok((params, key, cert))
}

fn signed_cert(
    ca_params: &CertificateParams,
    ca_key: &KeyPair,
    common_name: &str,
    subject_alt_names: &[&str],
) -> TestResult<GeneratedPem> {
    let mut params = CertificateParams::new(
        subject_alt_names
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>(),
    )?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    params.distinguished_name = dn;
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params.key_usages.push(KeyUsagePurpose::KeyEncipherment);
    let issuer = Issuer::from_params(ca_params, ca_key);
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, &issuer)?;
    Ok(GeneratedPem {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

impl TestPki {
    fn new() -> TestResult<Self> {
        let (ca_params, ca_key, ca_cert) = ca_cert("ft-test-ca")?;
        let (_alt_ca_params, _alt_ca_key, alt_ca_cert) = ca_cert("ft-test-alt-ca")?;
        let server = signed_cert(&ca_params, &ca_key, "localhost", &["localhost"])?;
        let client = signed_cert(&ca_params, &ca_key, "wa-client", &["wa-client"])?;

        Ok(Self {
            ca: write_pem(&ca_cert.pem())?,
            alt_ca: write_pem(&alt_ca_cert.pem())?,
            server_cert: write_pem(&server.cert_pem)?,
            server_key: write_pem(&server.key_pem)?,
            client_cert: write_pem(&client.cert_pem)?,
            client_key: write_pem(&client.key_pem)?,
        })
    }

    fn token_config(&self) -> DistributedConfig {
        DistributedConfig {
            enabled: true,
            auth_mode: DistributedAuthMode::Token,
            tls: DistributedTlsConfig {
                enabled: true,
                cert_path: Some(self.server_cert.path.clone()),
                key_path: Some(self.server_key.path.clone()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn mtls_server_config(&self, allow_agent_ids: Vec<String>) -> DistributedConfig {
        DistributedConfig {
            enabled: true,
            auth_mode: DistributedAuthMode::Mtls,
            allow_agent_ids,
            tls: DistributedTlsConfig {
                enabled: true,
                cert_path: Some(self.server_cert.path.clone()),
                key_path: Some(self.server_key.path.clone()),
                client_ca_path: Some(self.ca.path.clone()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn mtls_client_config(&self) -> DistributedConfig {
        DistributedConfig {
            enabled: true,
            auth_mode: DistributedAuthMode::Mtls,
            tls: DistributedTlsConfig {
                enabled: true,
                cert_path: Some(self.client_cert.path.clone()),
                key_path: Some(self.client_key.path.clone()),
                client_ca_path: Some(self.ca.path.clone()),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

fn io_other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn load_root_store(ca_path: &Path) -> TestResult<RootCertStore> {
    let mut reader = io::BufReader::new(std::fs::File::open(ca_path)?);
    let ca_certs = certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    let mut roots = RootCertStore::empty();
    let _ = roots.add_parsable_certificates(ca_certs);
    Ok(roots)
}

fn connector_without_client_cert(ca_path: &Path) -> TestResult<TlsConnector> {
    let versions = [&rustls::version::TLS13, &rustls::version::TLS12];
    let roots = load_root_store(ca_path)?;
    let client_config = ClientConfig::builder_with_protocol_versions(&versions)
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::new(client_config))
}

fn run_async_test<F>(future: F) -> TestResult
where
    F: std::future::Future<Output = TestResult>,
{
    let runtime = RuntimeBuilder::current_thread().enable_all().build()?;
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(future)));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        frankenterm_core::runtime_async::clear_runtime_handle();
    }));
    match result {
        Ok(inner) => inner,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

async fn assert_tls_accept_fails(server_task: task::JoinHandle<TestResult<bool>>) -> TestResult {
    let server_join = runtime_async::timeout(Duration::from_secs(2), server_task)
        .await
        .map_err(|_| io_other("server TLS accept timed out"))?;
    let server_failed = server_join.map_err(|err| io_other(format!("join failed: {err}")))??;
    assert!(server_failed);
    Ok(())
}

#[test]
fn tls_handshake_succeeds() -> TestResult {
    run_async_test(async {
        let pki = TestPki::new()?;
        let bundle = build_tls_bundle(&pki.token_config(), Some(Path::new(&pki.ca.path)))?;
        let acceptor = bundle.acceptor();
        let connector = bundle.connector();

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_task = task::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut tls_stream = acceptor.accept(stream).await?;
            let mut buf = [0u8; 4];
            tls_stream.read_exact(&mut buf).await?;
            Ok::<_, BoxError>(buf)
        });

        let mut stream = connector
            .connect("localhost", TcpStream::connect(addr).await?)
            .await?;
        stream.write_all(b"ping").await?;
        stream.shutdown().await?;

        let received = server_task
            .await
            .map_err(|err| io_other(format!("join failed: {err}")))??;
        assert_eq!(&received, b"ping");
        Ok(())
    })
}

#[test]
fn mtls_handshake_succeeds() -> TestResult {
    run_async_test(async {
        let pki = TestPki::new()?;
        let server_bundle = build_tls_bundle(
            &pki.mtls_server_config(vec!["wa-client".to_string()]),
            Some(Path::new(&pki.ca.path)),
        )?;
        let client_bundle =
            build_tls_bundle(&pki.mtls_client_config(), Some(Path::new(&pki.ca.path)))?;
        let acceptor = server_bundle.acceptor();
        let connector = client_bundle.connector();

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_task = task::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut tls_stream = acceptor.accept(stream).await?;
            let mut buf = [0u8; 2];
            tls_stream.read_exact(&mut buf).await?;
            Ok::<_, BoxError>(buf)
        });

        let mut stream = connector
            .connect("localhost", TcpStream::connect(addr).await?)
            .await?;
        stream.write_all(b"ok").await?;
        stream.shutdown().await?;

        let received = server_task
            .await
            .map_err(|err| io_other(format!("join failed: {err}")))??;
        assert_eq!(&received, b"ok");
        Ok(())
    })
}

#[test]
fn tls_handshake_rejects_untrusted_server() -> TestResult {
    run_async_test(async {
        let pki = TestPki::new()?;
        let server_bundle = build_tls_bundle(&pki.token_config(), Some(Path::new(&pki.ca.path)))?;
        let client_bundle =
            build_tls_bundle(&pki.token_config(), Some(Path::new(&pki.alt_ca.path)))?;
        let acceptor = server_bundle.acceptor();
        let connector = client_bundle.connector();

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_task = task::spawn(async move {
            let (stream, _) = listener.accept().await?;
            Ok::<_, BoxError>(acceptor.accept(stream).await.is_err())
        });

        let client_result = connector
            .connect("localhost", TcpStream::connect(addr).await?)
            .await;
        let server_join = runtime_async::timeout(Duration::from_secs(2), server_task)
            .await
            .map_err(|_| io_other("server TLS accept timed out"))?;
        let server_failed =
            server_join.map_err(|err| io_other(format!("join failed: {err}")))??;
        assert!(server_failed);
        assert!(client_result.is_err());
        Ok(())
    })
}

#[test]
fn mtls_handshake_rejects_missing_client_cert() -> TestResult {
    run_async_test(async {
        let pki = TestPki::new()?;
        let server_bundle = build_tls_bundle(
            &pki.mtls_server_config(Vec::new()),
            Some(Path::new(&pki.ca.path)),
        )?;
        let connector = connector_without_client_cert(Path::new(&pki.ca.path))?;
        let acceptor = server_bundle.acceptor();

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_task = task::spawn(async move {
            let (stream, _) = listener.accept().await?;
            Ok::<_, BoxError>(acceptor.accept(stream).await.is_err())
        });

        let client_result = connector
            .connect("localhost", TcpStream::connect(addr).await?)
            .await;
        assert_tls_accept_fails(server_task).await?;
        assert!(client_result.is_err());
        Ok(())
    })
}

#[test]
fn mtls_handshake_rejects_disallowed_client() -> TestResult {
    run_async_test(async {
        let pki = TestPki::new()?;
        let server_bundle = build_tls_bundle(
            &pki.mtls_server_config(vec!["not-allowed".to_string()]),
            Some(Path::new(&pki.ca.path)),
        )?;
        let client_bundle =
            build_tls_bundle(&pki.mtls_client_config(), Some(Path::new(&pki.ca.path)))?;
        let acceptor = server_bundle.acceptor();
        let connector = client_bundle.connector();

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_task = task::spawn(async move {
            let (stream, _) = listener.accept().await?;
            Ok::<_, BoxError>(acceptor.accept(stream).await.is_err())
        });

        let client_result = connector
            .connect("localhost", TcpStream::connect(addr).await?)
            .await;
        assert_tls_accept_fails(server_task).await?;
        assert!(client_result.is_err());
        Ok(())
    })
}

#[test]
fn tls_rejects_plaintext_client() -> TestResult {
    run_async_test(async {
        let pki = TestPki::new()?;
        let bundle = build_tls_bundle(&pki.token_config(), Some(Path::new(&pki.ca.path)))?;
        let acceptor = bundle.acceptor();

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_task = task::spawn(async move {
            let (stream, _) = listener.accept().await?;
            Ok::<_, BoxError>(acceptor.accept(stream).await.is_err())
        });

        let mut client = TcpStream::connect(addr).await?;
        client.write_all(b"not tls").await?;
        let _ = client.shutdown(std::net::Shutdown::Both);

        assert_tls_accept_fails(server_task).await?;
        Ok(())
    })
}

#[test]
fn bundle_acceptor_connector_handshake() -> TestResult {
    run_async_test(async {
        let pki = TestPki::new()?;
        let bundle = build_tls_bundle(&pki.token_config(), Some(Path::new(&pki.ca.path)))?;
        let acceptor = bundle.acceptor();
        let connector = bundle.connector();

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_task = task::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut tls = acceptor.accept(stream).await?;
            let mut buf = [0u8; 5];
            tls.read_exact(&mut buf).await?;
            Ok::<_, BoxError>(buf)
        });

        let mut client = connector
            .connect("localhost", TcpStream::connect(addr).await?)
            .await?;
        client.write_all(b"hello").await?;
        client.shutdown().await?;

        let received = server_task
            .await
            .map_err(|err| io_other(format!("join failed: {err}")))??;
        assert_eq!(&received, b"hello");
        Ok(())
    })
}

#[test]
fn bundle_acceptor_connector_mtls() -> TestResult {
    run_async_test(async {
        let pki = TestPki::new()?;
        let server_bundle = build_tls_bundle(
            &pki.mtls_server_config(vec!["wa-client".to_string()]),
            Some(Path::new(&pki.ca.path)),
        )?;
        let client_bundle =
            build_tls_bundle(&pki.mtls_client_config(), Some(Path::new(&pki.ca.path)))?;
        let acceptor = server_bundle.acceptor();
        let connector = client_bundle.connector();

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_task = task::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut tls = acceptor.accept(stream).await?;
            let mut buf = [0u8; 4];
            tls.read_exact(&mut buf).await?;
            Ok::<_, BoxError>(buf)
        });

        let mut client = connector
            .connect("localhost", TcpStream::connect(addr).await?)
            .await?;
        client.write_all(b"mtls").await?;
        client.shutdown().await?;

        let received = server_task
            .await
            .map_err(|err| io_other(format!("join failed: {err}")))??;
        assert_eq!(&received, b"mtls");
        Ok(())
    })
}

#[test]
fn bundle_clone_produces_working_tls() -> TestResult {
    let pki = TestPki::new()?;
    let bundle = build_tls_bundle(&pki.token_config(), Some(Path::new(&pki.ca.path)))?;
    let cloned = bundle.clone();
    let _a1 = bundle.acceptor();
    let _c1 = bundle.connector();
    let _a2 = cloned.acceptor();
    let _c2 = cloned.connector();
    Ok(())
}

#[test]
fn bundle_tls_bidirectional_exchange() -> TestResult {
    run_async_test(async {
        let pki = TestPki::new()?;
        let bundle = build_tls_bundle(&pki.token_config(), Some(Path::new(&pki.ca.path)))?;
        let acceptor = bundle.acceptor();
        let connector = bundle.connector();

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_task = task::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut tls = acceptor.accept(stream).await?;
            let mut buf = [0u8; 7];
            tls.read_exact(&mut buf).await?;
            assert_eq!(&buf, b"request");
            tls.write_all(b"response").await?;
            Ok::<_, BoxError>(())
        });

        let mut client = connector
            .connect("localhost", TcpStream::connect(addr).await?)
            .await?;
        client.write_all(b"request").await?;
        let mut buf = [0u8; 8];
        client.read_exact(&mut buf).await?;
        assert_eq!(&buf, b"response");
        client.shutdown().await?;

        server_task
            .await
            .map_err(|err| io_other(format!("join failed: {err}")))??;
        Ok(())
    })
}

#[test]
fn bundle_tls_large_payload() -> TestResult {
    run_async_test(async {
        let pki = TestPki::new()?;
        let bundle = build_tls_bundle(&pki.token_config(), Some(Path::new(&pki.ca.path)))?;
        let acceptor = bundle.acceptor();
        let connector = bundle.connector();
        let payload_size = 256 * 1024;
        let payload = vec![0xABu8; payload_size];

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_task = task::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut tls = acceptor.accept(stream).await?;
            let mut received = Vec::new();
            let mut buf = [0u8; 16 * 1024];
            loop {
                let n = tls.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                let chunk = buf
                    .get(..n)
                    .ok_or_else(|| io_other(format!("read length {n} exceeded buffer")))?;
                received.extend_from_slice(chunk);
            }
            Ok::<_, BoxError>(received.len())
        });

        let mut client = connector
            .connect("localhost", TcpStream::connect(addr).await?)
            .await?;
        client.write_all(&payload).await?;
        client.shutdown().await?;

        let received_len = server_task
            .await
            .map_err(|err| io_other(format!("join failed: {err}")))??;
        assert_eq!(received_len, payload_size);
        Ok(())
    })
}
