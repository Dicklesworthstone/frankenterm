use anyhow::{Context, Error, anyhow};
use async_ossl::AsyncSslStream;
use config::TlsDomainServer;
use frankenterm_mux_server_impl::PKI;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslStream, SslVerifyMode};
use openssl::x509::X509;
use promise::spawn::spawn_into_main_thread;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

struct OpenSSLNetListener {
    acceptor: Arc<SslAcceptor>,
    listener: TcpListener,
    dispatch_config: frankenterm_mux_server_impl::dispatch::DispatchRuntimeConfig,
}

impl OpenSSLNetListener {
    pub fn new(
        listener: TcpListener,
        acceptor: SslAcceptor,
        dispatch_config: frankenterm_mux_server_impl::dispatch::DispatchRuntimeConfig,
    ) -> Self {
        Self {
            listener,
            acceptor: Arc::new(acceptor),
            dispatch_config,
        }
    }

    /// Authenticates the peer.
    /// The requirements are:
    /// * The peer must have a certificate
    /// * The peer certificate must be trusted
    /// * The peer certificate must include a CN string that is
    ///   either an exact match for the unix username of the
    ///   user running this mux server instance, or must match
    ///   a special encoded prefix set up by a proprietary PKI
    ///   infrastructure in an environment used by the author.
    fn verify_peer_cert<T>(stream: &SslStream<T>) -> anyhow::Result<()> {
        let cert = stream
            .ssl()
            .peer_certificate()
            .ok_or_else(|| anyhow!("no peer cert"))?;
        let subject = cert.subject_name();
        let cn = subject
            .entries_by_nid(openssl::nid::Nid::COMMONNAME)
            .next()
            .ok_or_else(|| anyhow!("cert has no CN"))?;
        let cn_str = cn.data().as_utf8()?.to_string();

        let wanted_unix_name = std::env::var("USER")?;

        if wanted_unix_name == cn_str {
            log::trace!(
                "Peer certificate CN `{}` == $USER `{}`",
                cn_str,
                wanted_unix_name
            );
            Ok(())
        } else {
            // Some environments that are used by the author of this
            // program encode the CN in the form `user:unixname/DATA`
            let maybe_encoded = format!("user:{}/", wanted_unix_name);
            if cn_str.starts_with(&maybe_encoded) {
                log::trace!(
                    "Peer certificate CN `{}` matches $USER `{}`",
                    cn_str,
                    wanted_unix_name
                );
                Ok(())
            } else {
                anyhow::bail!("CN `{}` did not match $USER `{}`", cn_str, wanted_unix_name);
            }
        }
    }

    fn accept_tls_with_timeout(
        acceptor: &SslAcceptor,
        stream: std::net::TcpStream,
    ) -> anyhow::Result<SslStream<std::net::TcpStream>> {
        // Per-syscall read/write timeouts protect against a single stalled
        // syscall, but a slow-loris attacker dripping one byte every
        // (TLS_HANDSHAKE_TIMEOUT - 1) seconds would keep the connection alive
        // forever. Add a wall-clock deadline watchdog: if the handshake
        // hasn't completed by TLS_HANDSHAKE_TIMEOUT, forcibly shutdown the
        // cloned TCP handle, which unblocks the in-progress read/write with
        // EOF/error and the handshake aborts.
        stream.set_read_timeout(Some(TLS_HANDSHAKE_TIMEOUT))?;
        stream.set_write_timeout(Some(TLS_HANDSHAKE_TIMEOUT))?;

        let watchdog_stream = stream.try_clone()?;
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watchdog_done = Arc::clone(&done);
        let watchdog = std::thread::Builder::new()
            .name("tls-handshake-watchdog".to_string())
            .spawn(move || {
                let Some(deadline) = Instant::now().checked_add(TLS_HANDSHAKE_TIMEOUT) else {
                    let _ = watchdog_stream.shutdown(std::net::Shutdown::Both);
                    return;
                };
                // Park-with-deadline loop: wake periodically to re-check the flag
                // so a fast handshake doesn't waste the full timeout window.
                while Instant::now() < deadline {
                    if watchdog_done.load(std::sync::atomic::Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                if !watchdog_done.load(std::sync::atomic::Ordering::Acquire) {
                    // Deadline exceeded without completion — forcibly close the
                    // TCP handle so the blocked read/write in acceptor.accept
                    // returns an error and the handshake aborts.
                    let _ = watchdog_stream.shutdown(std::net::Shutdown::Both);
                }
            })
            .context("spawn TLS handshake watchdog thread")?;

        let accept_result = acceptor.accept(stream);
        // Signal watchdog to exit regardless of success/failure.
        done.store(true, std::sync::atomic::Ordering::Release);
        // Don't hold the caller up waiting for the 250ms watchdog tick; let
        // it detach cleanly.
        drop(watchdog);

        let stream = accept_result?;
        stream.get_ref().set_read_timeout(None)?;
        stream.get_ref().set_write_timeout(None)?;
        Ok(stream)
    }

    fn run(&mut self) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    stream.set_nodelay(true).ok();
                    let acceptor = self.acceptor.clone();
                    let dispatch_config = self.dispatch_config;

                    match Self::accept_tls_with_timeout(&acceptor, stream) {
                        Ok(stream) => {
                            if let Err(err) = Self::verify_peer_cert(&stream) {
                                log::error!("problem with peer cert: {}", err);
                                break;
                            }
                            spawn_into_main_thread(async move {
                                log::error!("Making new AsyncSslStream");
                                frankenterm_mux_server_impl::dispatch::process_with_config(
                                    AsyncSslStream::new(stream),
                                    dispatch_config,
                                )
                                .await
                                .map_err(|e| {
                                    log::error!("process: {:?}", e);
                                    e
                                })
                            })
                            .detach();
                        }
                        Err(e) => {
                            log::error!("failed TlsAcceptor: {}", e);
                        }
                    }
                }
                Err(err) => {
                    log::error!("accept failed: {}", err);
                    return;
                }
            }
        }
    }
}

fn build_tls_acceptor(tls_server: &TlsDomainServer) -> Result<SslAcceptor, Error> {
    openssl::init();

    let mut acceptor = SslAcceptor::mozilla_modern(SslMethod::tls())?;

    let cert_file = tls_server
        .pem_cert
        .clone()
        .unwrap_or_else(|| PKI.server_pem());
    acceptor
        .set_certificate_file(&cert_file, SslFiletype::PEM)
        .context(format!(
            "set_certificate_file to {} for TLS listener",
            cert_file.display()
        ))?;

    if let Some(chain_file) = tls_server.pem_ca.as_ref() {
        acceptor
            .set_certificate_chain_file(chain_file)
            .context(format!(
                "set_certificate_chain_file to {} for TLS listener",
                chain_file.display()
            ))?;
    }

    let key_file = tls_server
        .pem_private_key
        .clone()
        .unwrap_or_else(|| PKI.server_pem());
    acceptor
        .set_private_key_file(&key_file, SslFiletype::PEM)
        .context(format!(
            "set_private_key_file to {} for TLS listener",
            key_file.display()
        ))?;

    fn load_cert(name: &Path) -> anyhow::Result<X509> {
        let cert_bytes = std::fs::read(name)?;
        log::trace!("loaded {}", name.display());
        Ok(X509::from_pem(&cert_bytes)?)
    }
    for name in &tls_server.pem_root_certs {
        if name.is_dir() {
            for entry in std::fs::read_dir(name)? {
                if let Ok(cert) = load_cert(&entry?.path()) {
                    acceptor.cert_store_mut().add_cert(cert).ok();
                }
            }
        } else {
            acceptor.cert_store_mut().add_cert(load_cert(name)?)?;
        }
    }

    acceptor
        .cert_store_mut()
        .add_cert(load_cert(&PKI.ca_pem())?)?;

    acceptor.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);

    Ok(acceptor.build())
}

pub fn spawn_tls_listener(
    tls_server: &TlsDomainServer,
    dispatch_config: frankenterm_mux_server_impl::dispatch::DispatchRuntimeConfig,
) -> Result<(), Error> {
    let acceptor = build_tls_acceptor(tls_server)?;

    log::error!("listening with TLS on {:?}", tls_server.bind_address);

    let mut net_listener = OpenSSLNetListener::new(
        TcpListener::bind(&tls_server.bind_address).with_context(|| {
            format!(
                "error binding to mux_server_bind_address {}",
                tls_server.bind_address,
            )
        })?,
        acceptor,
        dispatch_config,
    );
    std::thread::Builder::new()
        .name("tls-listener".to_string())
        .spawn(move || {
            net_listener.run();
        })
        .context("spawn TLS listener thread")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn tls_handshake_timeout_drops_silent_client_within_six_seconds() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind_address = listener.local_addr().unwrap();
        let tls_server = TlsDomainServer {
            bind_address: bind_address.to_string(),
            ..TlsDomainServer::default()
        };
        let acceptor = build_tls_acceptor(&tls_server).unwrap();
        let (tx, rx) = mpsc::channel();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let started = Instant::now();
            let result = OpenSSLNetListener::accept_tls_with_timeout(&acceptor, stream);
            tx.send((started.elapsed(), result.is_err())).unwrap();
        });

        let mut client = TcpStream::connect(bind_address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        let (elapsed, timed_out) = rx.recv_timeout(Duration::from_secs(8)).unwrap();
        assert!(timed_out);
        // Upper bound: the wall-clock watchdog fires at TLS_HANDSHAKE_TIMEOUT
        // plus at most one 250ms poll tick plus thread-scheduling slack.
        assert!(
            elapsed <= Duration::from_secs(7),
            "TLS handshake timeout took too long: {elapsed:?}"
        );
        // Lower bound: a bug that closes the connection instantly (e.g. the
        // watchdog firing immediately, or the accept returning Err before
        // waiting) would slip past the upper bound alone. Require at least
        // most of the timeout to elapse so we know the wall-clock path was
        // actually exercised.
        assert!(
            elapsed >= Duration::from_secs(4),
            "TLS handshake timeout fired too early (watchdog regression?): {elapsed:?}"
        );

        let mut buf = [0u8; 1];
        match client.read(&mut buf) {
            Ok(0) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof
                ) => {}
            other => panic!("expected closed silent TLS connection, got {other:?}"),
        }

        server.join().unwrap();
    }
}
