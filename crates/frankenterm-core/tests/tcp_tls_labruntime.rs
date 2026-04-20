//! Concurrency and shutdown checks for asupersync TCP/TLS (wa-1u55z).
//!
//! These tests exercise multi-client handshakes and shutdown races on real
//! sockets using the asupersync runtime.

#![cfg(all(feature = "asupersync-runtime", feature = "distributed"))]

use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::{TcpListener, TcpStream};
use asupersync::tls::{TlsAcceptor, TlsConnector};
use frankenterm_core::config::{DistributedAuthMode, DistributedConfig};
use frankenterm_core::distributed::build_tls_bundle;
use frankenterm_core::runtime_compat::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::runtime_compat::task;
use rcgen::{Certificate, CertificateParams, DnType, IsCa, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
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

fn ca_cert() -> Certificate {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "wa-test-ca");
    params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    Certificate::from_params(params).expect("ca cert")
}

fn signed_cert(ca: &Certificate, cn: &str) -> (Certificate, CertificateDer<'static>) {
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, cn);
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params.key_usages.push(KeyUsagePurpose::KeyEncipherment);
    let cert = Certificate::from_params(params).expect("leaf cert");
    let der = cert
        .serialize_der_with_signer(ca)
        .expect("sign leaf")
        .into();
    (cert, der)
}

fn tls_bundle(mtls: bool) -> (TlsAcceptor, TlsConnector) {
    let ca = ca_cert();
    let ca_pem = ca.serialize_pem().expect("ca pem");
    let (server_cert, server_der) = signed_cert(&ca, "localhost");
    let server_key = PrivateKeyDer::try_from(server_cert.serialize_private_key_der()).unwrap();

    let mut ca_file = NamedTempFile::new().expect("ca temp");
    let ca_path = write_pem(&mut ca_file, ca_pem.as_bytes());

    let mut cert_file = NamedTempFile::new().expect("cert temp");
    let cert_path = write_pem(&mut cert_file, server_der.as_ref());
    let mut key_file = NamedTempFile::new().expect("key temp");
    let key_path = write_pem(&mut key_file, server_key.secret_der());

    let mut _client_cert_path = None;
    let mut _client_key_path = None;

    if mtls {
        let (client_cert, client_der) = signed_cert(&ca, "wa-client");
        let client_key = PrivateKeyDer::try_from(client_cert.serialize_private_key_der()).unwrap();
        let mut cc = NamedTempFile::new().expect("client cert temp");
        _client_cert_path = Some(write_pem(&mut cc, client_der.as_ref()));
        let mut ck = NamedTempFile::new().expect("client key temp");
        _client_key_path = Some(write_pem(&mut ck, client_key.secret_der()));
        // keep files alive until end of scope
        Box::leak(Box::new(cc));
        Box::leak(Box::new(ck));
    }

    // keep server files alive
    Box::leak(Box::new(ca_file));
    Box::leak(Box::new(cert_file));
    Box::leak(Box::new(key_file));

    let mut cfg = DistributedConfig::default();
    cfg.enabled = true;
    cfg.auth_mode = if mtls {
        DistributedAuthMode::Mtls
    } else {
        DistributedAuthMode::Token
    };
    cfg.tls.enabled = true;
    cfg.tls.cert_path = Some(cert_path);
    cfg.tls.key_path = Some(key_path);
    if mtls {
        cfg.tls.client_ca_path = Some(ca_path.clone());
    }

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
                    let accept = frankenterm_core::runtime_compat::timeout(
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
