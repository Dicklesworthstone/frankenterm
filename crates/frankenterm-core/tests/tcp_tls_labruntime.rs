//! Concurrency and shutdown checks for asupersync TCP/TLS (wa-1u55z).
//!
//! These tests exercise multi-client handshakes and shutdown races on real
//! sockets using the asupersync runtime.

#![cfg(all(feature = "asupersync-runtime", feature = "distributed"))]

use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::{TcpListener, TcpStream};
use asupersync::tls::{TlsAcceptor, TlsConnector};
use frankenterm_core::config::{DistributedAuthMode, DistributedConfig, DistributedTlsConfig};
use frankenterm_core::distributed::build_tls_bundle;
use frankenterm_core::runtime_async::task;
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use rcgen::{Certificate, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::pki_types::CertificateDer;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tempfile::NamedTempFile;

fn write_pem(file: &mut NamedTempFile, bytes: &[u8]) -> String {
    use std::io::Write;
    file.write_all(bytes).expect("write pem");
    file.flush().expect("flush pem");
    file.path().display().to_string()
}

struct GeneratedCert {
    cert: Certificate,
    key: KeyPair,
}

fn ca_cert() -> (CertificateParams, GeneratedCert) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "wa-test-ca");
    params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    let key = KeyPair::generate().expect("ca key");
    let cert = params.self_signed(&key).expect("ca cert");
    (params, GeneratedCert { cert, key })
}

fn signed_cert(
    ca_params: &CertificateParams,
    ca_key: &KeyPair,
    cn: &str,
) -> (GeneratedCert, CertificateDer<'static>) {
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, cn);
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params.key_usages.push(KeyUsagePurpose::KeyEncipherment);
    let issuer = Issuer::from_params(ca_params, ca_key);
    let key = KeyPair::generate().expect("leaf key");
    let cert = params.signed_by(&key, &issuer).expect("sign leaf");
    let der = cert.der().clone();
    (GeneratedCert { cert, key }, der)
}

fn tls_bundle(mtls: bool) -> (TlsAcceptor, TlsConnector) {
    let (ca_params, ca) = ca_cert();
    let ca_pem = ca.cert.pem();
    let (server_cert, _server_der) = signed_cert(&ca_params, &ca.key, "localhost");

    let mut ca_file = NamedTempFile::new().expect("ca temp");
    let ca_path = write_pem(&mut ca_file, ca_pem.as_bytes());

    let mut cert_file = NamedTempFile::new().expect("cert temp");
    let cert_path = write_pem(&mut cert_file, server_cert.cert.pem().as_bytes());
    let mut key_file = NamedTempFile::new().expect("key temp");
    let key_path = write_pem(&mut key_file, server_cert.key.serialize_pem().as_bytes());

    let mut _client_cert_path = None;
    let mut _client_key_path = None;

    if mtls {
        let (client_cert, _client_der) = signed_cert(&ca_params, &ca.key, "wa-client");
        let mut cc = NamedTempFile::new().expect("client cert temp");
        _client_cert_path = Some(write_pem(&mut cc, client_cert.cert.pem().as_bytes()));
        let mut ck = NamedTempFile::new().expect("client key temp");
        _client_key_path = Some(write_pem(
            &mut ck,
            client_cert.key.serialize_pem().as_bytes(),
        ));
        // keep files alive until end of scope
        Box::leak(Box::new(cc));
        Box::leak(Box::new(ck));
    }

    // keep server files alive
    Box::leak(Box::new(ca_file));
    Box::leak(Box::new(cert_file));
    Box::leak(Box::new(key_file));

    let auth_mode = if mtls {
        DistributedAuthMode::Mtls
    } else {
        DistributedAuthMode::Token
    };
    let cfg = DistributedConfig {
        enabled: true,
        auth_mode,
        tls: DistributedTlsConfig {
            enabled: true,
            cert_path: Some(cert_path),
            key_path: Some(key_path),
            client_ca_path: mtls.then(|| ca_path.clone()),
            ..Default::default()
        },
        ..Default::default()
    };

    let bundle = build_tls_bundle(&cfg, Some(std::path::Path::new(&ca_path))).expect("tls bundle");
    (
        TlsAcceptor::new((*bundle.server).clone()),
        TlsConnector::new((*bundle.client).clone()),
    )
}

#[test]
fn concurrent_tls_clients_all_handshake() {
    let (acceptor, connector) = tls_bundle(true);
    let rt = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let remaining = Arc::new(AtomicUsize::new(8));

        let acceptor_task = {
            let acceptor = acceptor.clone();
            let remaining = Arc::clone(&remaining);
            task::spawn(async move {
                while remaining.load(Ordering::SeqCst) > 0 {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut tls = acceptor.accept(stream).await.unwrap();
                    let mut buf = [0u8; 1];
                    tls.read_exact(&mut buf).await.unwrap();
                    remaining.fetch_sub(1, Ordering::SeqCst);
                }
            })
        };

        let mut joins = Vec::new();
        for i in 0..8u8 {
            let connector = connector.clone();
            joins.push(task::spawn(async move {
                let stream = TcpStream::connect(addr).await.unwrap();
                let mut tls = connector.connect("localhost", stream).await.unwrap();
                tls.write_all(&[i]).await.unwrap();
            }));
        }

        for j in joins {
            j.await.unwrap();
        }
        acceptor_task.await.unwrap();
        assert_eq!(remaining.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn listener_shutdown_race() {
    let (acceptor, connector) = tls_bundle(false);
    let rt = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicUsize::new(0));

        let acceptor_task = {
            let shutdown = Arc::clone(&shutdown);
            let acceptor = acceptor.clone();
            task::spawn(async move {
                loop {
                    if shutdown.load(Ordering::SeqCst) > 0 {
                        break;
                    }
                    let accept = frankenterm_core::runtime_async::timeout(
                        Duration::from_millis(50),
                        listener.accept(),
                    )
                    .await;
                    match accept {
                        Ok(Ok((stream, _))) => {
                            let mut tls = acceptor.accept(stream).await.unwrap();
                            let mut buf = [0u8; 1];
                            let _ = tls.read_exact(&mut buf).await;
                        }
                        Ok(Err(_)) => break,
                        Err(_) => {}
                    }
                }
            })
        };

        let client_task = {
            let shutdown = Arc::clone(&shutdown);
            task::spawn(async move {
                let stream = TcpStream::connect(addr).await.unwrap();
                let mut tls = connector.connect("localhost", stream).await.unwrap();
                tls.write_all(&[1]).await.unwrap();
                shutdown.store(1, Ordering::SeqCst);
            })
        };

        let _ = client_task.await;
        acceptor_task.await.unwrap();
    });
}
