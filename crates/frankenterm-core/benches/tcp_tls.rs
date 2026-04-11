#![cfg(all(feature = "asupersync-runtime", feature = "distributed"))]

//! TCP/TLS microbenchmarks for asupersync (wa-1u55z).

use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::{TcpListener, TcpStream};
use asupersync::tls::{TlsAcceptor, TlsConnector};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use frankenterm_core::runtime_compat::{CompatRuntime, Runtime, RuntimeBuilder};
use futures::future;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use rustls_pemfile::{certs, private_key};
use std::io;
use std::sync::Arc;

const THROUGHPUT_BYTES: usize = 4 * 1024 * 1024;

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn load_cert_chain(pem: &str) -> io::Result<Vec<CertificateDer<'static>>> {
    let mut reader = std::io::Cursor::new(pem);
    certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn load_private_key(pem: &str) -> io::Result<PrivateKeyDer<'static>> {
    let mut reader = std::io::Cursor::new(pem);
    private_key(&mut reader)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing key"))
}

fn tls_materials(mtls: bool) -> io::Result<(TlsAcceptor, TlsConnector)> {
    let server_chain = load_cert_chain(SERVER_CERT)?;
    let server_key = load_private_key(SERVER_KEY)?;
    let ca_chain = load_cert_chain(CA_CERT)?;

    let mut roots = RootCertStore::empty();
    let _ = roots.add_parsable_certificates(ca_chain.clone());

    let versions = [&rustls::version::TLS13, &rustls::version::TLS12];
    let server_cfg = ServerConfig::builder_with_protocol_versions(&versions)
        .with_no_client_auth()
        .with_single_cert(server_chain, server_key.clone_key())
        .map_err(to_io)?;

    let client_cfg = ClientConfig::builder_with_protocol_versions(&versions)
        .with_root_certificates(roots)
        .with_no_client_auth();

    let acceptor = TlsAcceptor::new(Arc::new(server_cfg));
    let connector = if mtls {
        let client_chain = load_cert_chain(CLIENT_CERT)?;
        let client_key = load_private_key(CLIENT_KEY)?;
        let cfg = client_cfg
            .with_client_auth_cert(client_chain, client_key.clone_key())
            .map_err(to_io)?;
        TlsConnector::new(Arc::new(cfg))
    } else {
        TlsConnector::new(Arc::new(client_cfg))
    };

    Ok((acceptor, connector))
}

fn to_io<E: std::fmt::Display>(err: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err.to_string())
}

fn bench_tls_handshake(c: &mut Criterion) {
    let rt = runtime();
    let (acceptor, connector) = tls_materials(false).expect("tls materials");

    c.bench_function("tls13/handshake", |b| {
        b.iter(|| {
            let acceptor = acceptor.clone();
            let connector = connector.clone();
            rt.block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();

                let accept = async {
                    let (stream, _) = listener.accept().await.unwrap();
                    acceptor.accept(stream).await.unwrap()
                };
                let connect = async {
                    let stream = TcpStream::connect(addr).await.unwrap();
                    connector.connect("localhost", stream).await.unwrap()
                };

                let (mut server_tls, mut client_tls) = future::join(accept, connect).await;
                client_tls.write_all(b"ok").await.unwrap();
                let mut buf = [0u8; 2];
                server_tls.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"ok");
            });
        });
    });
}

fn bench_tcp_throughput(c: &mut Criterion) {
    let rt = runtime();
    let payload = vec![0xABu8; THROUGHPUT_BYTES];
    let mut group = c.benchmark_group("tcp/throughput");
    group.throughput(Throughput::Bytes(THROUGHPUT_BYTES as u64));
    group.bench_function("plain", |b| {
        b.iter(|| {
            let payload = payload.clone();
            rt.block_on(async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();

                let server = async {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut received = 0usize;
                    let mut buf = [0u8; 16 * 1024];
                    while received < THROUGHPUT_BYTES {
                        let n = stream.read(&mut buf).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        received += n;
                    }
                    assert_eq!(received, THROUGHPUT_BYTES);
                };

                let client = async move {
                    let mut stream = TcpStream::connect(addr).await.unwrap();
                    stream.write_all(&payload).await.unwrap();
                    let _ = stream.shutdown(asupersync::net::Shutdown::Both);
                };

                future::join(server, client).await;
            });
        });
    });
    group.finish();
}

fn bench_tls_throughput(c: &mut Criterion) {
    let rt = runtime();
    let payload = vec![0xCDu8; THROUGHPUT_BYTES];
    let (acceptor, connector) = tls_materials(false).expect("tls materials");
    let mut group = c.benchmark_group("tls13/throughput");
    group.throughput(Throughput::Bytes(THROUGHPUT_BYTES as u64));
    group.bench_function("tls13", |b| {
        b.iter(|| {
            let payload = payload.clone();
            let acceptor = acceptor.clone();
            let connector = connector.clone();
            rt.block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();

                let server = async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut tls = acceptor.accept(stream).await.unwrap();
                    let mut received = 0usize;
                    let mut buf = [0u8; 16 * 1024];
                    while received < THROUGHPUT_BYTES {
                        let n = tls.read(&mut buf).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        received += n;
                    }
                    assert_eq!(received, THROUGHPUT_BYTES);
                };

                let client = async move {
                    let stream = TcpStream::connect(addr).await.unwrap();
                    let mut tls = connector.connect("localhost", stream).await.unwrap();
                    tls.write_all(&payload).await.unwrap();
                    tls.shutdown().await.unwrap();
                };

                future::join(server, client).await;
            });
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_tls_handshake,
    bench_tcp_throughput,
    bench_tls_throughput
);
criterion_main!(benches);

// ── PEM fixtures (copied from distributed tests) ─────────────────────────────
const CA_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIDCzCCAfOgAwIBAgIUSBlBg5d5TgsxHl8LEBJNtT90RlkwDQYJKoZIhvcNAQEL
BQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAzMjEwOTAwNDNaFw0zNjAz
MTgwOTAwNDNaMBUxEzARBgNVBAMMCndhLXRlc3QtY2EwggEiMA0GCSqGSIb3DQEB
AQUAA4IBDwAwggEKAoIBAQCFJpJndMHirHU/O2h7+PLhPUdk7J+Bph+l6HpRRSZd
YMms/0PMgIUkR8TPWCb10L0wKySrnalD6/iMx61m/zdQhra2xGItlnvYHujB0gQD
na70y36sRahTtEy2e1BTTNp8E0qVfTgEpkI8FeUnxZOaSoIFzmq1Jss/2y6T4pc5
bVOvtFKBGROJ/uyCtD/IZSUUtAh3UjNI6XnfLr0z8QwJPqBmGZDCqekBqSnRtSQ1
UBw1qbc5hzEFVM2wvz4vE4ug7ERwrlYt0ifNfACnvMSnkSYwGTdtbyRwuaeqRIgt
uJEHjkoPIPYtKYiv8zyyjEMjH3twCH7nNNZzzI9vKmn1AgMBAAGjUzBRMB0GA1Ud
DgQWBBS739cePr8uEC6o5+fXYQQjRpt/wjAfBgNVHSMEGDAWgBS739cePr8uEC6o
5+fXYQQjRpt/wjAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQAo
nRDUW5iayGY+79YO2qKprWgF+UCH1DQe0OgpDKoQlQVGswC7xlyZlg+LNNVwku6x
E0YZRp4+yGJIlPdSBfjhd/OFKOrCEkcsiAGvKpv6nPuxQwvK6I3SncC75OnLIhHP
V5IUbIevzYOfpJ/jCwYxGHDNExdaq5s1Kq3L64G8xgPAcK2aokc4Ym86WSkMmYlG
QTXXsx3WkXcRflPpzo/eRsXyuoteQNs3zcnUb1FO//XhfpIk32MNvyBQJMnWLAv4
Fwm3d+jMp9NpZqfyFFXtPaEL+Vna/3fqHD2zbSSy0NGTIhMnm+hxFXOAU2oietRA
sJkfmGf1F0WhXlOzalTz
-----END CERTIFICATE-----
";

const SERVER_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIDDzCCAfegAwIBAgIUSSJw3sCpNmZ66bOs6sC3RcilA7UwDQYJKoZIhvcNAQEL
BQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAzMjEwOTAwNDNaFw0zNjAz
MTgwOTAwNDNaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcNAQEB
BQADggEPADCCAQoCggEBAKGm2eAiS4yQPXcXvYAebOVzgeo9x2FQD26Yg1xAlvsJ
enlbTSJoXZhFxYH4MpFRvUXyv/REv8zdGWwjLsbkSYj3aBqTq6f5FFTX6Kw0Z9pf
cm3r1bpIqYyoannTIYMkJ1pyZ9cL21+QwSt4FKIU6oCqY4wGG8LyQI6Wny9wwxHi
z2wNXd0KBCUWFKZ76S504LUJj8oUYx1g0LE/ycmGSTeKLRLPP+sSXMYkNsBdegtT
EhQggZY/Ju2GaUnSJxFwhOrMW9zSRusZ5+wQTzm4DyLo4zz0S4yd3L5zSc6KwTo/
EjGScgo/QvEXt5jwZ3lCqlHkanT3AcSNfxSn2NWH+50CAwEAAaNYMFYwFAYDVR0R
BA0wC4IJbG9jYWxob3N0MB0GA1UdDgQWBBTmUMc5j/Ono3+QzhSoaY9wCvdbMDAf
BgNVHSMEGDAWgBS739cePr8uEC6o5+fXYQQjRpt/wjANBgkqhkiG9w0BAQsFAAOC
AQEASYMoNycM5Nu99QkhKShHxbMHIr2+Uh5HxL28I3gNqtF9rCuMX7DCX0pTKnIh
zRdEEvOc1EE1/hxqgkfZrPfK2fyWJI0H/lcE/phuvVkZo5YNxqvqfZDYrGjIL1VR
Ov2TZnb7wxbwCwfwJBY6WVOXIMBkOQZujuwPnqY5LrC/4UPLWe3ll4wdxAqk6LjY
bwpmnddK2XLbOmXKrsfXyWqK30L43U5cOtiEuEGWGHw8y2u6yv5sxkQrEy2aIKKv
FXGJzUGW1U+9Xv0npHkqYZ8TYUXMdyl3esHifiOo66Hgwi272ZEdLCTqvVmlUMHH
jpaHbLcEMTbShLFW++UYM6OPdA==
-----END CERTIFICATE-----
";

const SERVER_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQChptngIkuMkD13
F72AHmzlc4HqPcdhUA9umINcQJb7CXp5W00iaF2YRcWB+DKRUb1F8r/0RL/M3Rls
Iy7G5EmI92gak6un+RRU1+isNGfaX3Jt69W6SKmMqGp50yGDJCdacmfXC9tfkMEr
eBSiFOqAqmOMBhvC8kCOlp8vcMMR4s9sDV3dCgQlFhSme+kudOC1CY/KFGMdYNCx
P8nJhkk3ii0Szz/rElzGJDbAXXoLUxIUIIGWPybthmlJ0icRcITqzFvc0kbrGefs
EE85uA8i6OM89EuMndy+c0nOisE6PxIxknIKP0LxF7eY8Gd5QqpR5Gp09wHEjX8U
p9jVh/udAgMBAAECggEADoeDLMCYXsV2KNai6zmQ2xzDMA0mdwotoOinYerSRzUG
Y5L/v1h3FSEsS+7FiMc6hmd3tlpZjO3Qg6Yz5Z+ONnfaTQ050Aq0t52CZbv+G6QZ
kmSwnKI8Tw6yJ0oBSJq+yMPgrnT73j6SBjiwThMoMrFd6i+AXkjM4aQLIcX2WoyO
O3SBa4p9bybfBxejz0XsN6bL6xaKg3pwMLj/Qgyi1lN+WxmFbt43Lhs4ohBrWZyi
/fYry9veWQQQch9LfYfmR6bAsTH/gzhTltEQcywWQh+SeDnzwAmj9w0Gxmms6ON5
QePCrqm895mr86Dhnu7qFJE2y40RSJet/tgaOXH/BwKBgQDMO1tEe9M+ehZWGkLb
UITfVGCVQm9nYI3DgUmkL+pxajvk7nY7p21Zt8u3n/wiAJgYfgMGKOurZE88noIm
LaiXQui8xT5R2EI9F5HTd0aU+Qbarw4BvSURHQf62L+zWCPbp88fNVvlkcgzByMU
2NtcUKynG/tuYbR5smJ06kHf9wKBgQDKoHbILE44Q+u5KQpRQVHrtaq2x/HsrYzm
aGmTAhGq3b2u4H2WL3N3NcotUp4VLXog3qoKq1YJp5N2ivI76m9jIB6Ju96XK1E7
kRTMoMKkM6s0b4vZd59jO1UbqJE3iWNfKEKXrN14Y7zLl2/104K/vWhg/Iy3iaiw
nU6UPoiECwKBgQCaJzdVcs1Y/Bf996Z9GcKhO3QHVXT3J6b5aY3nMw+XeaMpwmBV
2KMuNA+9UzGhjKdA3WR08tAntvgj/lSocpAtVCCN06edaUleCXtVjVMmQO1OhRFi
eJ0Q1MPgMFhKC35NXtV0bfcmSao98eYl5yV0AaTAIdvfTjpGHUI5k1QTswKBgHXw
hHLq5vR1BEWYD7tP6+Dost8E7lm2gqay3bPFpobv3jJl1HOQVwLyOiW7SuxEtitf
r1XaeI/SDFEZevlI8WCfF2dQBLW0rume/p5EjEaLFIHG033W1N0rcdRRf4T14PNI
OcqTAa3LT96o3LAXVqlIE/MvzLAf3iI+zbgX1doVAoGAdtYC2zsX+wmahqFWVPaK
uxJloJa49RkOsfIoxNYIGZ6aHiIQIiWRO0UzJnMVK9JO0qZJYeDHfbA5DCkLRSBX
c63fvIhipkeGEwI/5SK4oAZPgv3tx0kWFq/LufUImkH/ptL+PAX+j53DNfWEKPTY
ukR51Vo2Dv5OO1Iz1PTzmMk=
-----END PRIVATE KEY-----
";

const CLIENT_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIDDzCCAfegAwIBAgIUSSJw3sCpNmZ66bOs6sC3RcilA7YwDQYJKoZIhvcNAQEL
BQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAzMjEwOTAwNDNaFw0zNjAz
MTgwOTAwNDNaMBQxEjAQBgNVBAMMCXdhLWNsaWVudDCCASIwDQYJKoZIhvcNAQEB
BQADggEPADCCAQoCggEBALGjt9y5RErRGqRjdA81tEjQ9wlaHhVWOpTaZbHQWZCk
H/BrYMDofEgm9UOB5aLXyD6ok91InBS8+L9ZTRPVsMTbESxghSVCeLmA0+wzXX8j
PNDi56nBQ9MK1zNnizSTInXJ7O0ldbVl37GXdCm9d3/jCLo1zc2ZsYUsYULIDyzt
r6AW8EUgs6iNOGyyrr73UEX0qwJv/dZfXV/bt3DoiMHbwwY5xCqWAPI1jk7ZLYbB
CsNE43bOCW/O5k4t0awYKufzC7HKepk8/2wNbJeW2jjpRyUgZEYOWroiZiz80kHz
bmNEaR9RdNhqDoODm1XhxLdCzW0wrUs0J8W4f2LUCAwEAAaNYMFYwFAYDVR0RBA0w
C4IJd2EtY2xpZW50MB0GA1UdDgQWBBTt0Lh4XbXKoKb7EVJCKBqjNjewzzAfBgNV
HSMEGDAWgBS739cePr8uEC6o5+fXYQQjRpt/wjANBgkqhkiG9w0BAQsFAAOCAQEA
Ax2ln/+jo/+u/TmKkXFH5f9rvF4IWu/qEdsDNM1EEMEPsTJEcYuNYy5cvfWF3cfg
ugSDjJauk9e3ybZ2IrvTG3Q0bOkkAH7K3ZuXp1NlEwf3czznhukDwgTUeSlyD48s
88z+6mmb/2DN9d8WJaPQMKkXH3xUMmc0c/jTpzXHnu7Gi8hMWQ+6ckifkzWIM/O/
QP403C4n9wV0g+2kkTegMH71J7HkXHLSfk0GILK3JTuJ+4x6fpuyHOBbk4KWRv3c
RT+B9FuwyzHLtYH8zHVnHtRMOnDtKqZd+aZ9f2H4XC7c1gfkrUOmdm1uLpeoXFOF
5qMyV38UeVHu1E1E1rAe/g==
-----END CERTIFICATE-----
";

const CLIENT_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCxo7fcuURK0Rqk
Y3QPNbRI0PcJWh4VVjqU2mWx0FmQpAkf8GtgwOh8SCb1Q4DlotfIPqiT3UicFLz4
v1lNE9WwxNsRLGCFJUJ4uYDT7DFdfyM80OLnqcFD0wrXM2eLNJMidcns7SV1tWXf
sZd0Kb13f+MIujXNzZmxhSxhQsgPLO2voBbwRSCzqI04bLKvvvdQRfSrAm/91l9d
X9u3cOiIwdvDBjnEKpYA8jWOTtkthsEKw0Tjds4Jb87mTi3RrBgq4fMLscp6mTz/
bA1sl5baOOlHJSBsRhJ5auiWYvPNTQfNuY0RpH1F02GoOg4LptV4cS3Qs1tMK1LN
CfFuH9i1AgMBAAECggEABZIRQiTggooeeu460FP0+d2XtEg1A6KccVjh8dq18aZl
5NsDZa7W+41rBKufv1PFJDHsKRWPi9l4ouz5OMS49vNNaAiguZGADUzKlfAPt5Nf
la4lQhzdgyol6mFhy13piUib/Z04MgHva72OzCVCRvI7OpBVqe+wdKks9EXbA+LC
53P3vM2FR1OqonBWWEo0B6dXi4A5vYczaaPmmb9SjEaHsoKzVmJ9LM4CdpwxU10U
O0hSkx8Bk6y9YKHIyYrbsam9eRiIVPlumwigPrCnVLJIrJyuOQT6N4OPtu9RDF9y
z0/+y0ROp0RhubfLNlcSd3cgz89ajW/pkXnYUykIsQKBgQDheATBiSsbLfkK1dfy
OJWOawBiAqPbEr8qSuVcIhVQi238554D5PKoJvBgNCJNQ8gPeODnQTky4vfpBsvk
kva4PXTkeYehOWgAyvo7S+2yguu2xU/9VrkaEP8j3WQu0vxsjpVA8EocFgTKvyCf
D5ABVVkTliDKqn5CaRYZO5iqiQKBgQDJsay/tIxhzTvV6bdDI12akq5Tywwxq+G2
40VswXvcTP9WH0QF0jPPoQRP4zzExQKm+nhGk12tavsOtF5r9SfxuiJyY1PwzjUg
02at3F+mLnN0X33hNNU1d1WBWszSQwTpNkfOPxHaC9MIgCbVNuO4GwGlenlbnTIz
nU3ojl7BzQKBgDysc3sxUmxJ/s6vpSEFoRlmKgA1/aoibVcQOJCGi33VR4/bNGaP
4czmTaFV5jUsnFWtjbgtkRrkgRowPgYQllwWDbK+EYWNUTOFa7kxQZHcMVpJ1rCx
+bXOBRq9pQwEsvDzna6P+yF7u2Zj8H9dTL9PHF1s9P4Uy01LwiqgIwEhAoGAFddo
xqXNofWwqhySHPIie8+wkyBk5KghXEXGSd22BQhNikz+d8bol25vYhtQhFp1TBHJ
npLszQ/NuizsILK+rZ2jh1GcUHJ0LGbYMrGvpfZXyF1i61VmVVDj8IsdrRNW385h
/kK0MzGem8gM7H/yLwi1p+7YX4RpYE+DlVB9kG0CgYBq8KImZb1bTxSoyV38dU/L
WTHph8hsoq2Vv63dR3g6kP+hRbtEOAWizVAhYBdn0ODRj08gP2IJ/vTqP04VwUsL
niSVI9+dOzAFsfObCkRbEseYAr4weYcUL/MpqClOeS6jVYjAOXPm2c2kYkZQ9ke+
KBAhs4snj5QspGFqkazmIw==
-----END PRIVATE KEY-----
";
