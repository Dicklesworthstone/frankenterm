//! LabRuntime port of `#[tokio::test]` async tests from `distributed.rs`.
//!
//! Each test that previously used `#[tokio::test]` is wrapped in
//! `RuntimeFixture::current_thread()` + `rt.block_on(async { … })`.
//! Feature-gated behind `asupersync-runtime` and `distributed`.
//!
//! Test certificate constants and `temp_pem()` helper are duplicated here
//! since the originals are `#[cfg(test)]`-gated in the source module.
//!
//! Uses the public `build_tls_bundle` API to construct TLS configs.

#![cfg(all(feature = "asupersync-runtime", feature = "distributed"))]

mod common;

use common::fixtures::RuntimeFixture;

use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::{TcpListener, TcpStream};
use asupersync::tls::{TlsAcceptor, TlsConnector};

use frankenterm_core::config::{DistributedAuthMode, DistributedConfig};
use frankenterm_core::distributed::build_tls_bundle;

use std::time::Duration;

// ===========================================================================
// Test certificate constants (mirrored from distributed.rs #[cfg(test)])
// ===========================================================================

const CA_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDGzCCAgOgAwIBAgIUR8JHXom3tZxZAwXcBF09FctZBXUwDQYJKoZIhvcNAQEL\nBQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAxMzExOTUwNDFaFw0yNjAz\nMDIxOTUwNDFaMBUxEzARBgNVBAMMCndhLXRlc3QtY2EwggEiMA0GCSqGSIb3DQEB\nAQUAA4IBDwAwggEKAoIBAQCLsfmpPVqsXx4W3mJhOSonFeARj9j9jZ2z7HKq5DwF\nt40XW9aBTJ3tAyEf+96so/196v2dwNL/GF2c/NLFDYblpVKWKEBpbIxsFeimquz/\nBP+biMAXHK18/r2Sotad5FNb3jLGmeZ5q9jjC2T+Mvw7KFc0ptz/m7yivBgECQgS\n3qfaKfeYwdPVtRT9BHLXtVi0y1r7E+7bvfnWBkIJ5Jz/LIDOQBoEd/ofwuvWx/as\n3Pnz4jbN8Rz5/x8GmgVni5ryaoJv0nmNavoZScIGgVOua3Cro8Nf47lW67HQ7QTl\ngWbTURQzjRznD2KWQKclNt8LMfhaTPWCwWv5m99wibDDAgMBAAGjYzBhMB0GA1Ud\nDgQWBBRuIqT4PRnABam0DRoUTFnTmT0rozAfBgNVHSMEGDAWgBRuIqT4PRnABam0\nDRoUTFnTmT0rozAPBgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIBBjANBgkq\nhkiG9w0BAQsFAAOCAQEAIrtQ1+ykRNoqpYuvcuMa5s3inzpCkmtXfrhXAIclroAW\nhxkZ8YobU381HSjq9CoOmcEwvj/SESqCD21u3qH4iqAPXEMSdi7sfXznc41Xmm+Z\nK5gXwmeqmO+VX7t2XtSvAeBEhOTpgtFcOCt2UoSVD38Qq8yJGcE7zS5d2B2rncTz\nhtHaFr21HeGSpn+Jz91CgPBCdhHuVrruZOr61lhfHfaNH8E7pPS63GXbo58yrOfX\nw/w5gkbPZVMkxLFn1OQt2Ah4uud4VbJ76JOylfyKwWJH3VrYw8ZE98M3CWRh6mGq\nhLXdOswkuXOAIL5kTVIpJzkXRxW+owwW5pHvCs0DiA==\n-----END CERTIFICATE-----\n";

const CA_CERT_ALT: &str = "-----BEGIN CERTIFICATE-----\nMIIDIzCCAgugAwIBAgIUZEO9mhldKaM+vYQlBxRzbx4NDOYwDQYJKoZIhvcNAQEL\nBQAwGTEXMBUGA1UEAwwOd2EtdGVzdC1jYS1hbHQwHhcNMjYwMTMxMTk1MTIwWhcN\nMjYwMzAyMTk1MTIwWjAZMRcwFQYDVQQDDA53YS10ZXN0LWNhLWFsdDCCASIwDQYJ\nKoZIhvcNAQEBBQADggEPADCCAQoCggEBAKfmzBFOOLB68UYCpAkvLuFebPm8vi5g\nFOAFTNA15bSOOHV1NAidEnvRxRr1BBbSeZDkiL3ucCaApMWZUfceOY+qkbiRSQdv\nLWRLt8b4UhuU/jV5wYbVrLaQ6+v6AneVMAHEdto3rcth/lZH/snRGzkReFF+uWG2\nat+GcyGHGQkpseK6bYaE/NgjawVqU4UdCf9OlgFHdrbKKjpnOwULv2t6THeqv36X\nm0G2m6aaFLG/23VWA/l0wKHP2slpBcLizZEwuQL4vY3SQYEI9Iw53tb8fh6hEANj\n9scTDoyW0AO/KSH8adPnX6KoJg6c2I7jkWXxbBlVXJtU9wfkd1D0RikCAwEAAaNj\nMGEwHQYDVR0OBBYEFBmwJJCWc0HPjfJkWiOq0/9038ySMB8GA1UdIwQYMBaAFBmw\nJJCWc0HPjfJkWiOq0/9038ySMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQD\nAgEGMA0GCSqGSIb3DQEBCwUAA4IBAQB0s7vQNAudWKupjWP97II5X31y8GUKKgAh\nQqoCl9OUhqTvmaWLSj1d4+8YSO6F34ZW0QNuHQZ/6gzuHIyLpaOUC2V/PMaFuC3O\nZJv3K/udxXsMH2otFo4iT0FFFUigFynXu/0//iD850/g6jHk8YMLeOGWZQkDKOae\nTlfh3IYE7kWZQUBUYPzuLZc4gYvPYVMdIfY8+5IPxOJxC7brFrViRMcbp4xW7Jfu\nkZz8vfzmY+hjQFgOsdcFVzQenRtTxr8eMdowJ++phHJs4gtQyEY15+zkYpg7B5iZ\nIX6nxMJcVfMJb4OPECWPjjwJTPSH8yiIOmw24/dbJZ4ZKjcpP3FH\n-----END CERTIFICATE-----\n";

const SERVER_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDSjCCAjKgAwIBAgIUJCkA/YZgClbfb2uy8x2u/esjLQswDQYJKoZIhvcNAQEL\nBQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAxMzExOTUwNDFaFw0yNjAz\nMDIxOTUwNDFaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcNAQEB\nBQADggEPADCCAQoCggEBAJCazMUTdFnCMXolx/7uXzPMWX5CVxXTKL/tFuisXo3m\nPuxdT+gbaHOsDSwuOAm1jojUtQblCr1NSHNdvJoIMdOmZ2Z4wOexaqb+d25p6QcZ\n2yyILjmEWUhGu/OKT95rxH0t+rwidMnfh4MT7qkrE/ybjzaYuxH18qLIRAbKy/xp\nsrOO7loBCS3PUqrXwj9eDXqm7WzzN1PcqqVqGzEJCOJJVJGN4qW3F7xXrVZQ3UYo\n25Ve/W3w27qOF7szrGpdT3j6ZBeDuCkzVba1jbTfwDJ+azo5Hc4wtuFkb1izQItd\no+D3ChXP4kF1fxb7MLIHJ4ICpNNjsAeaWzY5wkEXskkCAwEAAaOBkjCBjzAMBgNV\nHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIFoDATBgNVHSUEDDAKBggrBgEFBQcDATAa\nBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwHQYDVR0OBBYEFHB089XTOjeLi+KX\niGzgJbz6vyUXMB8GA1UdIwQYMBaAFG4ipPg9GcAFqbQNGhRMWdOZPSujMA0GCSqG\nSIb3DQEBCwUAA4IBAQBRXt2g280K7U5bsLUO5rMhTgDw3OfaGul6FYCH0Cfah1jC\n/DlTQ+bWHnK+zz2Jqvh2zYw8wHEUGD+aCWIK2B9+9B6oOUAMIzWhQovIro11AAut\n8FKYpdNT32UWbWSv0hKU5H5HBetfM+7ZEA3ZAdGgblBvnW3h6LZfmCMgUAuzbsdq\n4WrgpDiNArSxLC+ZFdsNWfIztntg4IDRGnbpd59dnuL3sznB2ggXJq6MW9wnfbtu\njzteJfIE4m2SU7zlsZY6mDGLx8u7Hz22WfCrdhxq6vomYyrxlDJTNR1kudOcwwFB\nquZGgDxcDu64rrmVno3xYqfPMUeA8/NpwKYI2y2+\n-----END CERTIFICATE-----\n";

const SERVER_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCQmszFE3RZwjF6\nJcf+7l8zzFl+QlcV0yi/7RborF6N5j7sXU/oG2hzrA0sLjgJtY6I1LUG5Qq9TUhz\nXbyaCDHTpmdmeMDnsWqm/nduaekHGdssiC45hFlIRrvzik/ea8R9Lfq8InTJ34eD\nE+6pKxP8m482mLsR9fKiyEQGysv8abKzju5aAQktz1Kq18I/Xg16pu1s8zdT3Kql\nahsxCQjiSVSRjeKltxe8V61WUN1GKNuVXv1t8Nu6jhe7M6xqXU94+mQXg7gpM1W2\ntY2038Ayfms6OR3OMLbhZG9Ys0CLXaPg9woVz+JBdX8W+zCyByeCAqTTY7AHmls2\nOcJBF7JJAgMBAAECggEAHnAnODiPHjGtPnvjbDr62SljkRsfv51SD4w1bUaTJKVZ\ni2Fc54uVYfvOTgVwkEKiPRUhAdGGgDBbVsVdZMLi0h1N2JkEagDDZWFc/GXYwkDk\nDKyhpkPAk2EoQOxVQYlHs93Q0HckRDYEDUhNzVge/eY0sBZYEkDGERO8lf1sELZS\nAkgUNl+jwsGkpTuDXd87dN0cQ5DgORsj8LiCbCMSMyL/sFv58CUgiwzQyi6hQSTw\ngBvLe8snAf65B+M63WTs5UBoD5U52Lpr98jqdY/U+B0SRB0xluQfYeMegJkab+H8\nOy+/nWeih6gtWXvco+OlUAabPCOUpwaETxx4QIUjPQKBgQDBFYDnq22wHuW15kBS\nKoK9kXtYGxiJ+nAbtRYorres+fd6VFH9CBUslUDpHfiEZ4qI1FBRhrx0mMDHs/hS\nQdCnUhZaDAOjmNLwNImPwZM9YEVRDwWlmzy/0/l4O/HM+1Rs2dakASoH+/+PDrLZ\nFd0+RawX34drfILHWeZsS2p/twKBgQC/uUulbrjeWVuHcp7QBC5VAyihWdmRTzEx\nNSruxFrHqq/P5WOkN5C4upOt/QJYBSietXjT4i6w26jrxQOXdetZoc9JRTVqbh1R\nJapFWb/HsFreps2+O7eqtPa21aad37a+WHbX0QBXBxN0ACtHafqkOgUY3KYCd7JI\n6fzoMUtd/wKBgEKGWid31Q79Vj/Z2Qd2Rh1yZoDwtP+1HbMuLThPGlGqvi2Tp7v6\ncPEva3HmNZ3I3t5N6G5ucbfqeWFVDJWqv20mxzS3NvnCycqhD1RMaaKX7MoE1vk8\nBy5Apo9ad/EcFvZ6B43yKL0fgemUMuLAub2e27BN/6Z0+8obm1xsj4D5AoGBALyf\nc4IN3cm7xiYLKZ3kDyVKV0XvHPMuI2qTMWr5OYrpLdFukEp29GYaAcMSgaTRZnZG\nedqT03Xill1nVjJELEjhvgsLERNlxGgak1tpghnXMn+NQivfmsJTCcs1hZgbCjJY\n3ItVr2zvpD7jD7FR3eqGvo8IPjd9RaUgt9ZE8S5HAoGALZDIV3SPPBPAY0ihfYWa\nJvqq4q+r44NMxk3yksr6yypuX3oZZM6HDERlRvhARYhIA+LIY5uK9tlZRsBmL7Ka\nVbhuUjmV7CF3lfyni4cvVM3D8fv05gSc5v4fnhrzAI2WZ53Vr/6f8k5avXYEocjn\nkxlgLg6xndsSmoukN3i0FrI=\n-----END PRIVATE KEY-----\n";

const CLIENT_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDLDCCAhSgAwIBAgIUJCkA/YZgClbfb2uy8x2u/esjLQwwDQYJKoZIhvcNAQEL\nBQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAxMzExOTUwNDFaFw0yNjAz\nMDIxOTUwNDFaMBQxEjAQBgNVBAMMCXdhLWNsaWVudDCCASIwDQYJKoZIhvcNAQEB\nBQADggEPADCCAQoCggEBAKgARf2gerf4yMQqHoZ0YfaRbYTjL6HEoyC3ZHrMLmLx\nUsHt7ELB/KiX+mYLQ7J+JW+ZYyOBETq9vqBZCT8+pGc/8c2KuUasVldzTpU7JneT\ny6x0Pld9TvoXZVqFDHA+O4yqwsmPWqm57XWTcTFjLyrWaEAdTSD0NdsxStlv2xgN\nbjelUl/1CNhYGeOVmYNZnz0tx4KGdO85LkafDltc3C55tTe3U0yitKS14GrKe/Xz\no0VGB5htkxQbGSMhVSmt5VnpheERiQ+mLDc9U2KlJ2euSDVvmFiMZ3w9ehshL1xp\n6H6P3cxX9ocEVritzLczV7aBkepLnCCNpqS5cqIBiQ8CAwEAAaN1MHMwDAYDVR0T\nAQH/BAIwADAOBgNVHQ8BAf8EBAMCBaAwEwYDVR0lBAwwCgYIKwYBBQUHAwIwHQYD\nVR0OBBYEFJhYZvekIWexWSegWXOIguWJmS2WMB8GA1UdIwQYMBaAFG4ipPg9GcAF\nqbQNGhRMWdOZPSujMA0GCSqGSIb3DQEBCwUAA4IBAQB8++cVKFRc7vz/dEL4qQGA\n9m4Ss06Mw+e2x7Ns4bc0HjxJSe/2XeARUmFTJknwJA9e3+tLz9a3M1turL5PZTCA\n3+NnNZUeFChsMIV07xa60KdFbd6lkV+Z8y2gw365j4twJLoibw6Rkfd9P+tGJT4w\nNDKmVotOPBbCCaiUANX7TVUxrB9FL+h044fNj3x8R5mFy06D3HxOErbSTJalnPd9\nfJDMZD6lVqm8tskKFbCSQ0clgrlOEv6gsL9cHsjwlyLAJs17BE4PT3cvZKlHZ5Ai\nX0B5sDGWLSmhKl+9eECJt0trrjuT/NOr4UsiN6StyMJwnaC7Bucy+o+iO5Z8cOl6\n-----END CERTIFICATE-----\n";

const CLIENT_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCoAEX9oHq3+MjE\nKh6GdGH2kW2E4y+hxKMgt2R6zC5i8VLB7exCwfyol/pmC0OyfiVvmWMjgRE6vb6g\nWQk/PqRnP/HNirlGrFZXc06VOyZ3k8usdD5XfU76F2VahQxwPjuMqsLJj1qpue11\nk3ExYy8q1mhAHU0g9DXbMUrZb9sYDW43pVJf9QjYWBnjlZmDWZ89LceChnTvOS5G\nnw5bXNwuebU3t1NMorSkteBqynv186NFRgeYbZMUGxkjIVUpreVZ6YXhEYkPpiw3\nPVNipSdnrkg1b5hYjGd8PXobIS9caeh+j93MV/aHBFa4rcy3M1e2gZHqS5wgjaak\nuXKiAYkPAgMBAAECggEAERQ6CU8zupk1m8+mW8fgH6doKV7JPFpXtR8/vUYdnxxm\na+Wqo5zB+Ue+Anq5rp8pYh+HVxgrbrvUccurZ30QTJjRFbK5JCin/Grx/bTOM9DY\nH1eP8OgBy+Xt/VZSTeTdu+6uL7x9nIyUyeOr2bf6FxJF9eKksSlygi6QK+u1q8uj\njY0l2HG18BQLDgvfsTa92aSPVTiJ/gnK3/SmPt60TFUjtSPJ4Yzhx++5sijuUq9L\nNe3yDXefBJjj4y8Xdx0grnXjHh6wI96pdBWd+uuQpt7GQGz3ApQwugzYBaVMEKa6\nEc2dSYqzxUXB1JgLhBc8PaqEQgwk5RQdcTsgcL2sCQKBgQDV8uD780Y/4WgaWp3W\nkoYa90ehJtjEgTN/PIPT04ictqxzEpYRj8s0LrKCsvzO5bGOk73UC9h6jyKh0rLy\nwEE7ISn4pijh62dm8EkHGN9OvzH1eUEBkwwY7s693ivOfxxNPDhc2Zf3AHhPg5mS\nsgE5SU4SiRm9qWjW2CrepLLAuQKBgQDJBXmRhGNh5nk5dK7EEiR0VN5esjbazvlp\nHhETs86rg8/K9lRhDzZ5Je/wCoGY3gOlVQUtGOZ1jgXga5QcbwzODHZBPxDpSUsm\nYmfRO9ySRJEbG8+gYDUyA24UTm3eNKE1akbJKQFOlX4sHoxREcoI394kPEXoyvwP\n70U4VYZkBwKBgCErzAgkOsMSvqI/ZHNtOk+aAUgSDs/AvGxAxKumA2tQw0IAIrZM\nVhQcHV84QwwM/s99RpRG1eSCprryQP50Imj5hllf4bzNU7XZEWmBSLYb3LITf6mv\n09NVy0YS2TXl7UxoRtDWh8IrF3w0ii39XUU1gV5MVWpbhr6wu0zTukc5AoGBAIZg\n1I2ENHNjgDH6YEHN5vSlLymadLT8mxm78ap8DnH1YVjKJknjw4Rk6epK+6tW7pT9\nKsKk3JpE4ITPJWmEisjK59ph8Eoipsv4CHKEU8SrdVzr0HXjGmxegp2seCGMiR+N\n9dfPQ4JmyLtxiFdBTw9zp6oNaKZf2vRD/L/V3ErNAoGAfIbZxO9HAKxhx1IdtzmF\nnYq5UBDjz+dMD2O0CYOpkm6qQGtObEL0u+mkHn7QU1ojatI2XHV2yqei/eJZ3yHr\n0AdZ9rdtgqH7q1gU6GMjj/97me5SVmW+kMizR0PGf3aj5+3FDSzf1DiYshHEL3hd\nq7BEO+XYA2PpWEpAroXhMbQ=\n-----END PRIVATE KEY-----\n";

// ===========================================================================
// Helper: create a temp file containing PEM content
// ===========================================================================

fn temp_pem(contents: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    std::io::Write::write_all(file.as_file_mut(), contents.as_bytes()).expect("write pem");
    file
}

// ===========================================================================
// tls_handshake_succeeds
// ===========================================================================

#[test]
fn tls_handshake_succeeds() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let ca_cert = temp_pem(CA_CERT);
        let server_cert = temp_pem(SERVER_CERT);
        let server_key = temp_pem(SERVER_KEY);

        // Server config: Token mode, no mTLS
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.tls.enabled = true;
        config.tls.cert_path = Some(server_cert.path().display().to_string());
        config.tls.key_path = Some(server_key.path().display().to_string());

        let bundle = build_tls_bundle(&config, Some(ca_cert.path())).expect("tls bundle");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let acceptor = TlsAcceptor::new((*bundle.server).clone());
        let server_task = frankenterm_core::runtime_compat::task::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut tls_stream = acceptor.accept(stream).await.expect("accept tls");
            let mut buf = [0u8; 4];
            tls_stream.read_exact(&mut buf).await.expect("read");
            buf
        });

        let connector = TlsConnector::new((*bundle.client).clone());
        let mut stream = connector
            .connect(
                "localhost",
                TcpStream::connect(addr).await.expect("connect"),
            )
            .await
            .expect("tls connect");
        stream.write_all(b"ping").await.expect("write");

        let received = server_task.await.expect("join");
        assert_eq!(&received, b"ping");
    });
}

// ===========================================================================
// mtls_handshake_succeeds
// ===========================================================================

#[test]
fn mtls_handshake_succeeds() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let ca_cert = temp_pem(CA_CERT);
        let server_cert = temp_pem(SERVER_CERT);
        let server_key = temp_pem(SERVER_KEY);
        let client_cert = temp_pem(CLIENT_CERT);
        let client_key = temp_pem(CLIENT_KEY);

        // Server side: mTLS, server cert/key, client CA for verification
        let mut srv_cfg = DistributedConfig::default();
        srv_cfg.enabled = true;
        srv_cfg.auth_mode = DistributedAuthMode::Mtls;
        srv_cfg.tls.enabled = true;
        srv_cfg.tls.cert_path = Some(server_cert.path().display().to_string());
        srv_cfg.tls.key_path = Some(server_key.path().display().to_string());
        srv_cfg.tls.client_ca_path = Some(ca_cert.path().display().to_string());
        srv_cfg.allow_agent_ids = vec!["wa-client".to_string()];

        let server_bundle =
            build_tls_bundle(&srv_cfg, Some(ca_cert.path())).expect("server bundle");

        // Client side: mTLS, client cert/key, CA for server verification
        let mut cli_cfg = DistributedConfig::default();
        cli_cfg.enabled = true;
        cli_cfg.auth_mode = DistributedAuthMode::Mtls;
        cli_cfg.tls.enabled = true;
        cli_cfg.tls.cert_path = Some(client_cert.path().display().to_string());
        cli_cfg.tls.key_path = Some(client_key.path().display().to_string());

        let client_bundle =
            build_tls_bundle(&cli_cfg, Some(ca_cert.path())).expect("client bundle");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let acceptor = TlsAcceptor::new((*server_bundle.server).clone());
        let server_task = frankenterm_core::runtime_compat::task::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut tls_stream = acceptor.accept(stream).await.expect("accept tls");
            let mut buf = [0u8; 2];
            tls_stream.read_exact(&mut buf).await.expect("read");
            buf
        });

        let connector = TlsConnector::new((*client_bundle.client).clone());
        let mut stream = connector
            .connect(
                "localhost",
                TcpStream::connect(addr).await.expect("connect"),
            )
            .await
            .expect("tls connect");
        stream.write_all(b"ok").await.expect("write");

        let received = server_task.await.expect("join");
        assert_eq!(&received, b"ok");
    });
}

// ===========================================================================
// tls_handshake_rejects_untrusted_server
// ===========================================================================

#[test]
fn tls_handshake_rejects_untrusted_server() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let ca_cert_alt = temp_pem(CA_CERT_ALT);
        let server_cert = temp_pem(SERVER_CERT);
        let server_key = temp_pem(SERVER_KEY);

        // Server: normal TLS config
        let mut srv_cfg = DistributedConfig::default();
        srv_cfg.enabled = true;
        srv_cfg.tls.enabled = true;
        srv_cfg.tls.cert_path = Some(server_cert.path().display().to_string());
        srv_cfg.tls.key_path = Some(server_key.path().display().to_string());

        let server_bundle =
            build_tls_bundle(&srv_cfg, Some(ca_cert_alt.path())).expect("server bundle");

        // Client: uses WRONG CA (alt) — should reject server
        let client_bundle =
            build_tls_bundle(&srv_cfg, Some(ca_cert_alt.path())).expect("client bundle");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let acceptor = TlsAcceptor::new((*server_bundle.server).clone());
        let server_task = frankenterm_core::runtime_compat::task::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor.accept(stream).await
        });

        let connector = TlsConnector::new((*client_bundle.client).clone());
        let client_result = connector
            .connect(
                "localhost",
                TcpStream::connect(addr).await.expect("connect"),
            )
            .await;

        let server_result =
            frankenterm_core::runtime_compat::timeout(Duration::from_secs(2), server_task)
                .await
                .expect("server timeout")
                .expect("join");
        assert!(server_result.is_err());

        if let Ok(mut stream) = client_result {
            let write_result = stream.write_all(b"no cert").await;
            let mut buf = [0u8; 1];
            let read_result = stream.read_exact(&mut buf).await;
            assert!(write_result.is_err() || read_result.is_err());
        }
    });
}

// ===========================================================================
// mtls_handshake_rejects_missing_client_cert
// ===========================================================================

#[test]
fn mtls_handshake_rejects_missing_client_cert() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let ca_cert = temp_pem(CA_CERT);
        let server_cert = temp_pem(SERVER_CERT);
        let server_key = temp_pem(SERVER_KEY);

        // Server: mTLS required
        let mut srv_cfg = DistributedConfig::default();
        srv_cfg.enabled = true;
        srv_cfg.auth_mode = DistributedAuthMode::Mtls;
        srv_cfg.tls.enabled = true;
        srv_cfg.tls.cert_path = Some(server_cert.path().display().to_string());
        srv_cfg.tls.key_path = Some(server_key.path().display().to_string());
        srv_cfg.tls.client_ca_path = Some(ca_cert.path().display().to_string());

        let server_bundle =
            build_tls_bundle(&srv_cfg, Some(ca_cert.path())).expect("server bundle");

        // Client: Token mode (no client cert) — server requires mTLS
        let mut cli_cfg = DistributedConfig::default();
        cli_cfg.enabled = true;
        cli_cfg.auth_mode = DistributedAuthMode::Token;
        cli_cfg.tls.enabled = true;
        cli_cfg.tls.cert_path = Some(server_cert.path().display().to_string());

        let client_bundle =
            build_tls_bundle(&cli_cfg, Some(ca_cert.path())).expect("client bundle");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let acceptor = TlsAcceptor::new((*server_bundle.server).clone());
        let server_task = frankenterm_core::runtime_compat::task::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor.accept(stream).await
        });

        let connector = TlsConnector::new((*client_bundle.client).clone());
        let client_result = connector
            .connect(
                "localhost",
                TcpStream::connect(addr).await.expect("connect"),
            )
            .await;

        let server_result =
            frankenterm_core::runtime_compat::timeout(Duration::from_secs(2), server_task)
                .await
                .expect("server timeout")
                .expect("join");
        assert!(server_result.is_err());

        if let Ok(mut stream) = client_result {
            let write_result = stream.write_all(b"no cert").await;
            let mut buf = [0u8; 1];
            let read_result = stream.read_exact(&mut buf).await;
            assert!(write_result.is_err() || read_result.is_err());
        }
    });
}

// ===========================================================================
// mtls_handshake_rejects_disallowed_client
// ===========================================================================

#[test]
fn mtls_handshake_rejects_disallowed_client() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let ca_cert = temp_pem(CA_CERT);
        let server_cert = temp_pem(SERVER_CERT);
        let server_key = temp_pem(SERVER_KEY);
        let client_cert = temp_pem(CLIENT_CERT);
        let client_key = temp_pem(CLIENT_KEY);

        // Server: mTLS with allowlist that excludes the client
        let mut srv_cfg = DistributedConfig::default();
        srv_cfg.enabled = true;
        srv_cfg.auth_mode = DistributedAuthMode::Mtls;
        srv_cfg.tls.enabled = true;
        srv_cfg.tls.cert_path = Some(server_cert.path().display().to_string());
        srv_cfg.tls.key_path = Some(server_key.path().display().to_string());
        srv_cfg.tls.client_ca_path = Some(ca_cert.path().display().to_string());
        srv_cfg.allow_agent_ids = vec!["not-allowed".to_string()];

        let server_bundle =
            build_tls_bundle(&srv_cfg, Some(ca_cert.path())).expect("server bundle");

        // Client: mTLS with valid cert but not in server allowlist
        let mut cli_cfg = DistributedConfig::default();
        cli_cfg.enabled = true;
        cli_cfg.auth_mode = DistributedAuthMode::Mtls;
        cli_cfg.tls.enabled = true;
        cli_cfg.tls.cert_path = Some(client_cert.path().display().to_string());
        cli_cfg.tls.key_path = Some(client_key.path().display().to_string());

        let client_bundle =
            build_tls_bundle(&cli_cfg, Some(ca_cert.path())).expect("client bundle");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let acceptor = TlsAcceptor::new((*server_bundle.server).clone());
        let server_task = frankenterm_core::runtime_compat::task::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor.accept(stream).await
        });

        let connector = TlsConnector::new((*client_bundle.client).clone());
        let client_result = connector
            .connect(
                "localhost",
                TcpStream::connect(addr).await.expect("connect"),
            )
            .await;

        let server_result =
            frankenterm_core::runtime_compat::timeout(Duration::from_secs(2), server_task)
                .await
                .expect("server timeout")
                .expect("join");
        assert!(server_result.is_err());

        if let Ok(mut stream) = client_result {
            let write_result = stream.write_all(b"nope").await;
            let mut buf = [0u8; 1];
            let read_result = stream.read_exact(&mut buf).await;
            assert!(write_result.is_err() || read_result.is_err());
        }
    });
}

// ===========================================================================
// tls_rejects_plaintext_client
// ===========================================================================

#[test]
fn tls_rejects_plaintext_client() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let server_cert = temp_pem(SERVER_CERT);
        let server_key = temp_pem(SERVER_KEY);

        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.tls.enabled = true;
        config.tls.cert_path = Some(server_cert.path().display().to_string());
        config.tls.key_path = Some(server_key.path().display().to_string());

        let bundle = build_tls_bundle(&config, None).expect("tls bundle");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let acceptor = TlsAcceptor::new((*bundle.server).clone());
        let server_task = frankenterm_core::runtime_compat::task::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor.accept(stream).await
        });

        let mut client = TcpStream::connect(addr).await.expect("connect");
        client.write_all(b"not tls").await.expect("write");
        let _ = client.shutdown(std::net::Shutdown::Both);

        let server_result =
            frankenterm_core::runtime_compat::timeout(Duration::from_secs(2), server_task)
                .await
                .expect("server timeout")
                .expect("join");
        assert!(server_result.is_err());
    });
}
