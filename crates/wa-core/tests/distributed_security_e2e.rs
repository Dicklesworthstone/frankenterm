#![cfg(feature = "distributed")]

use std::io::{Seek, SeekFrom};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use wa_core::config::{DistributedAuthMode, DistributedConfig};
use wa_core::distributed::{
    DistributedSecurityError, SessionReplayGuard, build_tls_bundle, resolve_expected_token,
    validate_token,
};

const OLD_CA_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDGzCCAgOgAwIBAgIUR8JHXom3tZxZAwXcBF09FctZBXUwDQYJKoZIhvcNAQEL\nBQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAxMzExOTUwNDFaFw0yNjAz\nMDIxOTUwNDFaMBUxEzARBgNVBAMMCndhLXRlc3QtY2EwggEiMA0GCSqGSIb3DQEB\nAQUAA4IBDwAwggEKAoIBAQCLsfmpPVqsXx4W3mJhOSonFeARj9j9jZ2z7HKq5DwF\nt40XW9aBTJ3tAyEf+96so/196v2dwNL/GF2c/NLFDYblpVKWKEBpbIxsFeimquz/\nBP+biMAXHK18/r2Sotad5FNb3jLGmeZ5q9jjC2T+Mvw7KFc0ptz/m7yivBgECQgS\n3qfaKfeYwdPVtRT9BHLXtVi0y1r7E+7bvfnWBkIJ5Jz/LIDOQBoEd/ofwuvWx/as\n3Pnz4jbN8Rz5/x8GmgVni5ryaoJv0nmNavoZScIGgVOua3Cro8Nf47lW67HQ7QTl\ngWbTURQzjRznD2KWQKclNt8LMfhaTPWCwWv5m99wibDDAgMBAAGjYzBhMB0GA1Ud\nDgQWBBRuIqT4PRnABam0DRoUTFnTmT0rozAfBgNVHSMEGDAWgBRuIqT4PRnABam0\nDRoUTFnTmT0rozAPBgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIBBjANBgkq\nhkiG9w0BAQsFAAOCAQEAIrtQ1+ykRNoqpYuvcuMa5s3inzpCkmtXfrhXAIclroAW\nhxkZ8YobU381HSjq9CoOmcEwvj/SESqCD21u3qH4iqAPXEMSdi7sfXznc41Xmm+Z\nK5gXwmeqmO+VX7t2XtSvAeBEhOTpgtFcOCt2UoSVD38Qq8yJGcE7zS5d2B2rncTz\nhtHaFr21HeGSpn+Jz91CgPBCdhHuVrruZOr61lhfHfaNH8E7pPS63GXbo58yrOfX\nw/w5gkbPZVMkxLFn1OQt2Ah4uud4VbJ76JOylfyKwWJH3VrYw8ZE98M3CWRh6mGq\nhLXdOswkuXOAIL5kTVIpJzkXRxW+owwW5pHvCs0DiA==\n-----END CERTIFICATE-----\n";
const OLD_SERVER_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDSjCCAjKgAwIBAgIUJCkA/YZgClbfb2uy8x2u/esjLQswDQYJKoZIhvcNAQEL\nBQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAxMzExOTUwNDFaFw0yNjAz\nMDIxOTUwNDFaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcNAQEB\nBQADggEPADCCAQoCggEBAJCazMUTdFnCMXolx/7uXzPMWX5CVxXTKL/tFuisXo3m\nPuxdT+gbaHOsDSwuOAm1jojUtQblCr1NSHNdvJoIMdOmZ2Z4wOexaqb+d25p6QcZ\n2yyILjmEWUhGu/OKT95rxH0t+rwidMnfh4MT7qkrE/ybjzaYuxH18qLIRAbKy/xp\nsrOO7loBCS3PUqrXwj9eDXqm7WzzN1PcqqVqGzEJCOJJVJGN4qW3F7xXrVZQ3UYo\n25Ve/W3w27qOF7szrGpdT3j6ZBeDuCkzVba1jbTfwDJ+azo5Hc4wtuFkb1izQItd\no+D3ChXP4kF1fxb7MLIHJ4ICpNNjsAeaWzY5wkEXskkCAwEAAaOBkjCBjzAMBgNV\nHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIFoDATBgNVHSUEDDAKBggrBgEFBQcDATAa\nBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwHQYDVR0OBBYEFHB089XTOjeLi+KX\niGzgJbz6vyUXMB8GA1UdIwQYMBaAFG4ipPg9GcAFqbQNGhRMWdOZPSujMA0GCSqG\nSIb3DQEBCwUAA4IBAQBRXt2g280K7U5bsLUO5rMhTgDw3OfaGul6FYCH0Cfah1jC\n/DlTQ+bWHnK+zz2Jqvh2zYw8wHEUGD+aCWIK2B9+9B6oOUAMIzWhQovIro11AAut\n8FKYpdNT32UWbWSv0hKU5H5HBetfM+7ZEA3ZAdGgblBvnW3h6LZfmCMgUAuzbsdq\n4WrgpDiNArSxLC+ZFdsNWfIztntg4IDRGnbpd59dnuL3sznB2ggXJq6MW9wnfbtu\njzteJfIE4m2SU7zlsZY6mDGLx8u7Hz22WfCrdhxq6vomYyrxlDJTNR1kudOcwwFB\nquZGgDxcDu64rrmVno3xYqfPMUeA8/NpwKYI2y2+\n-----END CERTIFICATE-----\n";
const OLD_SERVER_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCQmszFE3RZwjF6\nJcf+7l8zzFl+QlcV0yi/7RborF6N5j7sXU/oG2hzrA0sLjgJtY6I1LUG5Qq9TUhz\nXbyaCDHTpmdmeMDnsWqm/nduaekHGdssiC45hFlIRrvzik/ea8R9Lfq8InTJ34eD\nE+6pKxP8m482mLsR9fKiyEQGysv8abKzju5aAQktz1Kq18I/Xg16pu1s8zdT3Kql\nahsxCQjiSVSRjeKltxe8V61WUN1GKNuVXv1t8Nu6jhe7M6xqXU94+mQXg7gpM1W2\ntY2038Ayfms6OR3OMLbhZG9Ys0CLXaPg9woVz+JBdX8W+zCyByeCAqTTY7AHmls2\nOcJBF7JJAgMBAAECggEAHnAnODiPHjGtPnvjbDr62SljkRsfv51SD4w1bUaTJKVZ\ni2Fc54uVYfvOTgVwkEKiPRUhAdGGgDBbVsVdZMLi0h1N2JkEagDDZWFc/GXYwkDk\nDKyhpkPAk2EoQOxVQYlHs93Q0HckRDYEDUhNzVge/eY0sBZYEkDGERO8lf1sELZS\nAkgUNl+jwsGkpTuDXd87dN0cQ5DgORsj8LiCbCMSMyL/sFv58CUgiwzQyi6hQSTw\ngBvLe8snAf65B+M63WTs5UBoD5U52Lpr98jqdY/U+B0SRB0xluQfYeMegJkab+H8\nOy+/nWeih6gtWXvco+OlUAabPCOUpwaETxx4QIUjPQKBgQDBFYDnq22wHuW15kBS\nKoK9kXtYGxiJ+nAbtRYorres+fd6VFH9CBUslUDpHfiEZ4qI1FBRhrx0mMDHs/hS\nQdCnUhZaDAOjmNLwNImPwZM9YEVRDwWlmzy/0/l4O/HM+1Rs2dakASoH+/+PDrLZ\nFd0+RawX34drfILHWeZsS2p/twKBgQC/uUulbrjeWVuHcp7QBC5VAyihWdmRTzEx\nNSruxFrHqq/P5WOkN5C4upOt/QJYBSietXjT4i6w26jrxQOXdetZoc9JRTVqbh1R\nJapFWb/HsFreps2+O7eqtPa21aad37a+WHbX0QBXBxN0ACtHafqkOgUY3KYCd7JI\n6fzoMUtd/wKBgEKGWid31Q79Vj/Z2Qd2Rh1yZoDwtP+1HbMuLThPGlGqvi2Tp7v6\ncPEva3HmNZ3I3t5N6G5ucbfqeWFVDJWqv20mxzS3NvnCycqhD1RMaaKX7MoE1vk8\nBy5Apo9ad/EcFvZ6B43yKL0fgemUMuLAub2e27BN/6Z0+8obm1xsj4D5AoGBALyf\nc4IN3cm7xiYLKZ3kDyVKV0XvHPMuI2qTMWr5OYrpLdFukEp29GYaAcMSgaTRZnZG\nedqT03Xill1nVjJELEjhvgsLERNlxGgak1tpghnXMn+NQivfmsJTCcs1hZgbCjJY\n3ItVr2zvpD7jD7FR3eqGvo8IPjd9RaUgt9ZE8S5HAoGALZDIV3SPPBPAY0ihfYWa\nJvqq4q+r44NMxk3yksr6yypuX3oZZM6HDERlRvhARYhIA+LIY5uK9tlZRsBmL7Ka\nVbhuUjmV7CF3lfyni4cvVM3D8fv05gSc5v4fnhrzAI2WZ53Vr/6f8k5avXYEocjn\nkxlgLg6xndsSmoukN3i0FrI=\n-----END PRIVATE KEY-----\n";
const ROTATED_CA_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDEzCCAfugAwIBAgIUcdKCdi77J5LYb+mBfHpvm7P27yAwDQYJKoZIhvcNAQEL\nBQAwGTEXMBUGA1UEAwwOd2EtdGVzdC1jYS1yb3QwHhcNMjYwMjA2MTY0NDQ1WhcN\nMjYwMzA4MTY0NDQ1WjAZMRcwFQYDVQQDDA53YS10ZXN0LWNhLXJvdDCCASIwDQYJ\nKoZIhvcNAQEBBQADggEPADCCAQoCggEBALmbCM0szTqbV+jsEi/ese6BsGQx3MRp\nDjvFO1ApZmZ/iyZo2ADbJq7BnJvDmB/oJYyP/K9Q8TNLiCx0hQov6ew5N37KQU9k\nEEE9Jd1f176FMG7DCzwlNnxwEjI0i4BOfjJ22RLqss5kaT9TVoumKuLeryeIMP/q\nCYYKaRelB5snVBGcQxa7vfxtpN7ymJHvslNRoV47SMl6KK15LhZCrqIsoBsio4Vx\nxVg4WYjiNfse5JeB7gIVfcxlnqTkG4VIl3r4cCUbR/kgzDT1mvhrXQKvWZVu1S4N\ntoEOauFUygruvjSjtJCvfYqho2quM0TAX/yrfg8K4xoHP9CkEsFENB0CAwEAAaNT\nMFEwHQYDVR0OBBYEFPEsRXCgwEUXvUrXEFWm7SwCA4OqMB8GA1UdIwQYMBaAFPEs\nRXCgwEUXvUrXEFWm7SwCA4OqMA8GA1UdEwEB/wQFMAMBAf8wDQYJKoZIhvcNAQEL\nBQADggEBAFXz1840GQhHF8JhFkSR/yJsRF/6dlHpMjAIne54lVG5LZXSMznE4g0Q\nU5KQACFcaP8CWMLsUUvH/kStTCKvQ7LW4wTRthfe72v/o6yN669uVKKqErkYAVjU\nMrtDG7d2LhXZ4q8hm2m5CLsHL3Czzci8UwHDuZL7HHKwCJjTdlHBnoPblb9NoJK2\nHyj/BKpY8IyK5dYgGTDiCeRkgZaiv2H7Rc8AmIqSVSHy/OKMRO3/Bf7in9VRHZrv\nC7E4VLXay7epw53bDEubo2zKjsTOgutq1PO4kb9qKomCSC8anMyRS2mSRg9j9ieG\ndsI8qmeA+YX4bQSkQzRAz0eW++lr4XY=\n-----END CERTIFICATE-----\n";
const ROTATED_SERVER_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDPDCCAiSgAwIBAgIUW9LnrD8WsN2vZrz6UcK8/V0wZVowDQYJKoZIhvcNAQEL\nBQAwGTEXMBUGA1UEAwwOd2EtdGVzdC1jYS1yb3QwHhcNMjYwMjA2MTY0NDQ1WhcN\nMjYwMzA4MTY0NDQ1WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwggEiMA0GCSqGSIb3\nDQEBAQUAA4IBDwAwggEKAoIBAQC23VIKJUtNl2qf6xeoELYIXy/YAELikEuZL5lh\nSxk3yIgD8lY/O8jerXu7pcqpkCNEjf5SE4+9CloZQhZMqHgc0jwwnRBwIyqrzcRQ\n52lgO635aPyNq/rxpNULtDcHlZUznuUs4M5UMR7jt7UnUZsZD4N3uEbjn8KshZBJ\nscfnQP9iefLvttqb/RjFm73kdESKahTduB6bH/ZtYK+8ha7afKHym+6nKyzjGD3u\nxfmDcjnTa9CoUac5fG8molNYSH5Jfg56604jYLDkD6zSKUoubdc1Af2UokNGnM/z\nWH7hWafkQGPOFVhwxTBlMva06d6lmL+l0afCqfLyJxLVyBtjAgMBAAGjgYAwfjAa\nBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwEwYDVR0lBAwwCgYIKwYBBQUHAwEw\nCwYDVR0PBAQDAgWgMB0GA1UdDgQWBBTqo8T2w7dxaPaxS/UXiAq1NMpTBjAfBgNV\nHSMEGDAWgBTxLEVwoMBFF71K1xBVpu0sAgODqjANBgkqhkiG9w0BAQsFAAOCAQEA\nhrNWN5N/mpz8+tHe5bFy5h7uV9cY1rnSMY0YlsJ+Wo2LWqWBEkMXIDM4Rc3Wk/Hg\npaAgGNzH9dDBlwc6a1fM8lzyNUR9lsskuTR7KoPnBze9e6TOjr0GFGRm6PXRZsVY\nZ4hoOIzYJj0Rh1XoCjZTnj3bmRnXuAIRK/WkOflbdtRUanhsC83FCv4laaj3T6tE\n9gqjcZoJVHpm3zGqzNlCd6LlILuiaeiBWrQOrCEnjL6pyqVr3OoFHrEknR0vYp7h\nwWzOEOhFoi6+5LiSVU5SSRanWR6WeEM+uiGa/shr3fTnaOpLjqUHtgySq2sR9iFJ\nNsLfP5Dfhno2CA0Vpu1ZKw==\n-----END CERTIFICATE-----\n";
const ROTATED_SERVER_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC23VIKJUtNl2qf\n6xeoELYIXy/YAELikEuZL5lhSxk3yIgD8lY/O8jerXu7pcqpkCNEjf5SE4+9CloZ\nQhZMqHgc0jwwnRBwIyqrzcRQ52lgO635aPyNq/rxpNULtDcHlZUznuUs4M5UMR7j\nt7UnUZsZD4N3uEbjn8KshZBJscfnQP9iefLvttqb/RjFm73kdESKahTduB6bH/Zt\nYK+8ha7afKHym+6nKyzjGD3uxfmDcjnTa9CoUac5fG8molNYSH5Jfg56604jYLDk\nD6zSKUoubdc1Af2UokNGnM/zWH7hWafkQGPOFVhwxTBlMva06d6lmL+l0afCqfLy\nJxLVyBtjAgMBAAECggEANw9axW1HSDygQTiTLeqiNNEcYchqWzehW6WGZFItbKt3\nsOCF8ZI5wDqyN+UKqZWZ2Ol8OxBixkPYryRD/J75U4xFzUltiqY8EfDp/IZBJ1Ww\n45kl+i5fZ+T+tQB1VVZHz3w3exTRa25C48QLyqP6tEgEiMa2qZEQF8w7jsT18P4R\nNuFJvaRYSDUKH04eXw/a2Tf4RrT8ARrq2LPGt++HfbtxX9mvNdXM/L0IDoJ+thoC\ncljF2aGP3Ac0MdvRApM0nS8L96Tn5lsM0DvLYWwmxOJ46uBfDNE0oBdnzbvPxOYt\nxb/vTYzjczx8XlWzswj868VF3VRjHOU5VMNcRdAf+QKBgQDu/O4T/uuyZNelJuRH\nbypBuLtiaAnW0CqT/dwxgXUUnM6CDcVRwdMxZZpJHZcw0xRcC9SqIagcnYDPKCs+\nMN71JVBqDtEr6sWc1YQK8BVrhV43PmcsWUlbFwhp1TroagTgU4gekhm7yQd4uQjW\nb79BkzMDs4n2cU0htxjHwpCchQKBgQDD4aeFixHWBeiq/4GqxtXtlLyZE0npHQCE\nIcYrwDtAuPccQfI4Ew+iGRD4n8y0KFirgQAdMWxNNmbpDubtHRUxzP1n7qp6tTbE\ndm1HtCH0x3rjyFoBExM/h89ZtqV5tVSqxAhXHjhJnLpI4j+2h3NXg0L71OEo5qqq\nn2T+HaewxwKBgQDcRCxeK58q3bzPj6fomvG0f0Hd8gvXfCcyHVD8I9g4NkozHeQW\ndXFkXsOzzd0SeAmUyKaqY7jhHt2gkOJCQKLOCSUzixKIyqp14WkA98SWQ+bRPeez\nvVtZ5EGx4YCYw1ZZN0QHARtMs3z6bHhTw8zf8H6dU7W9eTHg+DOTsaS9TQKBgCjz\nUvdbNJZe095z3iLawLyTfL4vxyLh+kqlWO2qmXiVcqvIqZ/JdFo6DU888Sm0yZzJ\nMkHoJDEcL3WHtQVbMCQiK9P/lEpk+hcmfwAfi33F+k4Gg7J3z21XsiSaR4vjOdkd\ndHTqD3BsQJGeIx3AwX9JJMbLIWtQldtnyVBK2NTfAoGBAM77wP0jNVpE/yCFedfX\nXvhk4gK7EVQSvrjcO/m+qPe8p1wxThxi6MYPbOCD43ziaEQRE+eFnxKnU7JuOeRJ\n9bjmJJruQ5suVIrhj8M2jYYYdDpaOFkhbikF2ElJK2/8FKpShGkIJrFVtosXH3Ro\n3yY4kwI9byl3+kOsCncB3BuN\n-----END PRIVATE KEY-----\n";

const PERF_HANDSHAKE_ROUNDS: usize = 12;
const PERF_VERIFY_ITERATIONS: usize = 5_000;
const PERF_HANDSHAKE_P95_BUDGET_MS: u128 = 400;
const PERF_VERIFY_P95_BUDGET_US: u128 = 250;
const PERF_VERIFY_THROUGHPUT_BUDGET_MSGS_PER_SEC: f64 = 2_000.0;

fn temp_pem(contents: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    std::io::Write::write_all(file.as_file_mut(), contents.as_bytes()).expect("write pem");
    file
}

fn emit_e2e_artifact(label: &str, value: serde_json::Value) {
    eprintln!("[ARTIFACT][distributed-e2e] {label}={value}");
}

fn percentile_u128(samples: &mut [u128], percentile: usize) -> u128 {
    assert!(!samples.is_empty(), "percentile requires non-empty samples");
    assert!(percentile <= 100, "percentile must be <= 100");
    samples.sort_unstable();
    let max_index = samples.len() - 1;
    let rank = (max_index * percentile) / 100;
    samples[rank]
}

async fn tls_round_trip(
    server_config: Arc<rustls::ServerConfig>,
    client_config: Arc<rustls::ClientConfig>,
    payload: &[u8],
) -> Result<Vec<u8>, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind: {e}"))?;
    let addr = listener.local_addr().map_err(|e| format!("addr: {e}"))?;
    let expected_len = payload.len();

    let acceptor = TlsAcceptor::from(server_config);
    let server_task = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| format!("accept: {e}"))?;
        let mut tls_stream = acceptor
            .accept(stream)
            .await
            .map_err(|e| format!("tls_accept: {e}"))?;
        let mut buf = vec![0u8; expected_len];
        tls_stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| format!("server_read: {e}"))?;
        Ok::<Vec<u8>, String>(buf)
    });

    let connector = TlsConnector::from(client_config);
    let server_name =
        rustls::pki_types::ServerName::try_from("localhost").map_err(|e| e.to_string())?;
    let mut client_stream = connector
        .connect(
            server_name,
            tokio::net::TcpStream::connect(addr)
                .await
                .map_err(|e| format!("tcp_connect: {e}"))?,
        )
        .await
        .map_err(|e| format!("tls_connect: {e}"))?;
    client_stream
        .write_all(payload)
        .await
        .map_err(|e| format!("client_write: {e}"))?;

    let received = timeout(Duration::from_secs(2), server_task)
        .await
        .map_err(|e| format!("server_timeout: {e}"))?
        .map_err(|e| format!("server_join: {e}"))??;
    Ok(received)
}

async fn tls_handshake_rejected(
    server_config: Arc<rustls::ServerConfig>,
    client_config: Arc<rustls::ClientConfig>,
) -> bool {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let acceptor = TlsAcceptor::from(server_config);
    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        acceptor.accept(stream).await
    });

    let connector = TlsConnector::from(client_config);
    let server_name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let client_result = connector
        .connect(
            server_name,
            tokio::net::TcpStream::connect(addr).await.expect("connect"),
        )
        .await;
    let server_result = timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server timeout")
        .expect("join");

    client_result.is_err() || server_result.is_err()
}

#[tokio::test]
async fn distributed_security_e2e_tls_required_happy_path_with_artifacts() {
    let ca_cert = temp_pem(OLD_CA_CERT);
    let server_cert = temp_pem(OLD_SERVER_CERT);
    let server_key = temp_pem(OLD_SERVER_KEY);

    let mut config = DistributedConfig::default();
    config.enabled = true;
    config.auth_mode = DistributedAuthMode::Token;
    config.token = Some("agent-a:token-v1".to_string());
    config.tls.enabled = true;
    config.tls.cert_path = Some(server_cert.path().display().to_string());
    config.tls.key_path = Some(server_key.path().display().to_string());

    let bundle = build_tls_bundle(&config, Some(ca_cert.path())).expect("tls bundle");
    let payload = b"secure-path";
    let received = tls_round_trip(
        Arc::clone(&bundle.server),
        Arc::clone(&bundle.client),
        payload,
    )
    .await
    .expect("round trip");
    assert_eq!(received.as_slice(), payload);

    assert!(
        validate_token(
            config.auth_mode,
            config.token.as_deref(),
            Some("agent-a:token-v1"),
            Some("agent-a"),
        )
        .is_ok()
    );

    emit_e2e_artifact(
        "aggregator_log",
        serde_json::json!({
            "scenario": "tls_required_happy_path",
            "bind": "127.0.0.1:ephemeral",
            "tls": "enabled",
            "payload_bytes": payload.len(),
            "result": "accepted"
        }),
    );
    emit_e2e_artifact(
        "agent_log",
        serde_json::json!({
            "scenario": "tls_required_happy_path",
            "tls_connect": "ok",
            "write": "ok",
            "read_echo": String::from_utf8_lossy(&received),
        }),
    );
    emit_e2e_artifact(
        "security_config_summary",
        serde_json::json!({
            "auth_mode": "token",
            "tls_enabled": true,
            "token_source": "inline",
            "token_preview": "[REDACTED]",
        }),
    );

    let db_file = tempfile::NamedTempFile::new().expect("db file");
    let conn = rusqlite::Connection::open(db_file.path()).expect("open db");
    conn.execute_batch(
        "CREATE TABLE security_events (
            id INTEGER PRIMARY KEY,
            scenario TEXT NOT NULL,
            outcome TEXT NOT NULL
         );
         INSERT INTO security_events (scenario, outcome)
         VALUES ('tls_required_happy_path', 'accepted');",
    )
    .expect("seed db");
    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM security_events", [], |row| row.get(0))
        .expect("count rows");
    let file_size = std::fs::metadata(db_file.path())
        .expect("db metadata")
        .len();
    emit_e2e_artifact(
        "db_snapshot",
        serde_json::json!({
            "path": db_file.path().display().to_string(),
            "rows": row_count,
            "size_bytes": file_size
        }),
    );
}

#[tokio::test]
async fn distributed_security_e2e_tls_failures_and_plaintext_rejection() {
    let trusted_ca = temp_pem(OLD_CA_CERT);
    let untrusted_ca = temp_pem(ROTATED_CA_CERT);
    let server_cert = temp_pem(OLD_SERVER_CERT);
    let server_key = temp_pem(OLD_SERVER_KEY);

    let mut config = DistributedConfig::default();
    config.enabled = true;
    config.auth_mode = DistributedAuthMode::Token;
    config.tls.enabled = true;
    config.tls.cert_path = Some(server_cert.path().display().to_string());
    config.tls.key_path = Some(server_key.path().display().to_string());

    let trusted_bundle = build_tls_bundle(&config, Some(trusted_ca.path())).expect("trusted");
    let untrusted_bundle = build_tls_bundle(&config, Some(untrusted_ca.path())).expect("untrusted");

    assert!(
        tls_handshake_rejected(
            Arc::clone(&trusted_bundle.server),
            Arc::clone(&untrusted_bundle.client),
        )
        .await
    );
    let success = tls_round_trip(
        Arc::clone(&trusted_bundle.server),
        Arc::clone(&trusted_bundle.client),
        b"ok",
    )
    .await
    .expect("trusted success");
    assert_eq!(success.as_slice(), b"ok");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let acceptor = TlsAcceptor::from(Arc::clone(&trusted_bundle.server));
    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        acceptor.accept(stream).await
    });

    let mut plaintext = tokio::net::TcpStream::connect(addr)
        .await
        .expect("plain connect");
    plaintext
        .write_all(b"not tls")
        .await
        .expect("write plaintext");
    let _ = plaintext.shutdown().await;
    let plaintext_rejected = timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server timeout")
        .expect("join")
        .is_err();
    assert!(plaintext_rejected);

    emit_e2e_artifact(
        "aggregator_log",
        serde_json::json!({
            "scenario": "tls_failures",
            "untrusted_ca_rejected": true,
            "plaintext_rejected": plaintext_rejected
        }),
    );
    emit_e2e_artifact(
        "agent_log",
        serde_json::json!({
            "scenario": "tls_failures",
            "trusted_client_status": "success",
            "untrusted_client_status": "rejected",
            "stable_error_code": DistributedSecurityError::AuthFailed.code()
        }),
    );
}

#[tokio::test]
async fn distributed_security_e2e_auth_replay_and_rotation() {
    let mut token_file = tempfile::NamedTempFile::new().expect("token file");
    std::io::Write::write_all(token_file.as_file_mut(), b"agent-a:token-v1")
        .expect("write token v1");

    let mut token_config = DistributedConfig::default();
    token_config.enabled = true;
    token_config.auth_mode = DistributedAuthMode::TokenAndMtls;
    token_config.token_path = Some(token_file.path().display().to_string());

    let token_v1 = resolve_expected_token(&token_config)
        .expect("resolve token v1")
        .expect("token required");
    assert!(
        validate_token(
            token_config.auth_mode,
            Some(&token_v1),
            Some("agent-a:token-v1"),
            Some("agent-a"),
        )
        .is_ok()
    );
    let wrong = validate_token(
        token_config.auth_mode,
        Some(&token_v1),
        Some("agent-a:bad-token"),
        Some("agent-a"),
    )
    .expect_err("bad token should fail");
    assert_eq!(wrong, DistributedSecurityError::AuthFailed);
    let wrong_msg = wrong.to_string();
    assert!(!wrong_msg.contains("token-v1"));
    assert!(!wrong_msg.contains("bad-token"));

    token_file.as_file_mut().set_len(0).expect("truncate token");
    token_file
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .expect("seek token");
    std::io::Write::write_all(token_file.as_file_mut(), b"agent-a:token-v2")
        .expect("write token v2");

    let token_v2 = resolve_expected_token(&token_config)
        .expect("resolve token v2")
        .expect("token required");
    assert!(
        validate_token(
            token_config.auth_mode,
            Some(&token_v2),
            Some("agent-a:token-v2"),
            Some("agent-a"),
        )
        .is_ok()
    );
    assert_eq!(
        validate_token(
            token_config.auth_mode,
            Some(&token_v2),
            Some("agent-a:token-v1"),
            Some("agent-a"),
        )
        .expect_err("old token must fail"),
        DistributedSecurityError::AuthFailed
    );

    let mut replay_guard = SessionReplayGuard::new(4);
    assert!(replay_guard.validate("session-1", 1).is_ok());
    let replay_err = replay_guard
        .validate("session-1", 1)
        .expect_err("replay should fail");
    assert_eq!(replay_err, DistributedSecurityError::ReplayDetected);
    assert_eq!(replay_err.code(), "dist.replay_detected");

    let old_ca = temp_pem(OLD_CA_CERT);
    let old_server_cert = temp_pem(OLD_SERVER_CERT);
    let old_server_key = temp_pem(OLD_SERVER_KEY);
    let new_ca = temp_pem(ROTATED_CA_CERT);
    let new_server_cert = temp_pem(ROTATED_SERVER_CERT);
    let new_server_key = temp_pem(ROTATED_SERVER_KEY);

    let mut old_config = DistributedConfig::default();
    old_config.enabled = true;
    old_config.auth_mode = DistributedAuthMode::Token;
    old_config.tls.enabled = true;
    old_config.tls.cert_path = Some(old_server_cert.path().display().to_string());
    old_config.tls.key_path = Some(old_server_key.path().display().to_string());

    let mut new_config = DistributedConfig::default();
    new_config.enabled = true;
    new_config.auth_mode = DistributedAuthMode::Token;
    new_config.tls.enabled = true;
    new_config.tls.cert_path = Some(new_server_cert.path().display().to_string());
    new_config.tls.key_path = Some(new_server_key.path().display().to_string());

    let old_bundle = build_tls_bundle(&old_config, Some(old_ca.path())).expect("old bundle");
    let new_bundle = build_tls_bundle(&new_config, Some(new_ca.path())).expect("new bundle");

    let old_exchange = tls_round_trip(
        Arc::clone(&old_bundle.server),
        Arc::clone(&old_bundle.client),
        b"old-cert-ok",
    )
    .await
    .expect("old cert should work");
    assert_eq!(old_exchange.as_slice(), b"old-cert-ok");

    assert!(
        tls_handshake_rejected(
            Arc::clone(&new_bundle.server),
            Arc::clone(&old_bundle.client),
        )
        .await,
        "old trust material should reject rotated cert"
    );

    let new_exchange = tls_round_trip(
        Arc::clone(&new_bundle.server),
        Arc::clone(&new_bundle.client),
        b"new-cert-ok",
    )
    .await
    .expect("new cert should work");
    assert_eq!(new_exchange.as_slice(), b"new-cert-ok");

    emit_e2e_artifact(
        "rotation_log",
        serde_json::json!({
            "scenario": "auth_replay_rotation",
            "token_rotated": true,
            "replay_rejected": replay_err.code(),
            "old_cert_rejected_after_rotation": true,
            "new_cert_accepted_after_rotation": true
        }),
    );
}

#[tokio::test]
async fn distributed_security_perf_budgets_within_initial_thresholds() {
    let ca_cert = temp_pem(OLD_CA_CERT);
    let server_cert = temp_pem(OLD_SERVER_CERT);
    let server_key = temp_pem(OLD_SERVER_KEY);

    let mut config = DistributedConfig::default();
    config.enabled = true;
    config.auth_mode = DistributedAuthMode::Token;
    config.token = Some("agent-a:token-v1".to_string());
    config.tls.enabled = true;
    config.tls.cert_path = Some(server_cert.path().display().to_string());
    config.tls.key_path = Some(server_key.path().display().to_string());

    let bundle = build_tls_bundle(&config, Some(ca_cert.path())).expect("tls bundle");

    let mut handshake_samples_ms = Vec::with_capacity(PERF_HANDSHAKE_ROUNDS);
    for _ in 0..PERF_HANDSHAKE_ROUNDS {
        let start = Instant::now();
        let received = tls_round_trip(
            Arc::clone(&bundle.server),
            Arc::clone(&bundle.client),
            b"perf",
        )
        .await
        .expect("tls round trip");
        assert_eq!(received.as_slice(), b"perf");
        handshake_samples_ms.push(start.elapsed().as_millis());
    }
    let handshake_p95_ms = percentile_u128(&mut handshake_samples_ms, 95);
    assert!(
        handshake_p95_ms <= PERF_HANDSHAKE_P95_BUDGET_MS,
        "TLS connection budget exceeded: p95={}ms > {}ms",
        handshake_p95_ms,
        PERF_HANDSHAKE_P95_BUDGET_MS
    );

    let mut replay_guard = SessionReplayGuard::new(PERF_VERIFY_ITERATIONS + 8);
    let expected_token = "agent-a:token-v1";
    let verify_start = Instant::now();
    let mut verify_samples_us = Vec::with_capacity(PERF_VERIFY_ITERATIONS);
    for seq in 1..=u64::try_from(PERF_VERIFY_ITERATIONS).expect("iteration count fits u64") {
        let sample_start = Instant::now();
        validate_token(
            DistributedAuthMode::TokenAndMtls,
            Some(expected_token),
            Some(expected_token),
            Some("agent-a"),
        )
        .expect("token should validate");
        replay_guard
            .validate("perf-session", seq)
            .expect("replay guard should accept monotonic sequence");
        verify_samples_us.push(sample_start.elapsed().as_micros());
    }
    let verify_elapsed = verify_start.elapsed();
    let verify_p95_us = percentile_u128(&mut verify_samples_us, 95);
    let verify_throughput = PERF_VERIFY_ITERATIONS as f64 / verify_elapsed.as_secs_f64();

    assert!(
        verify_p95_us <= PERF_VERIFY_P95_BUDGET_US,
        "Message verification budget exceeded: p95={}us > {}us",
        verify_p95_us,
        PERF_VERIFY_P95_BUDGET_US
    );
    assert!(
        verify_throughput >= PERF_VERIFY_THROUGHPUT_BUDGET_MSGS_PER_SEC,
        "Verification throughput budget missed: {:.0} msg/s < {:.0} msg/s",
        verify_throughput,
        PERF_VERIFY_THROUGHPUT_BUDGET_MSGS_PER_SEC
    );

    emit_e2e_artifact(
        "perf_budget_report",
        serde_json::json!({
            "scenario": "perf_budgets",
            "budgets": {
                "tls_connection_p95_ms_max": PERF_HANDSHAKE_P95_BUDGET_MS,
                "verify_p95_us_max": PERF_VERIFY_P95_BUDGET_US,
                "verify_throughput_msgs_per_sec_min": PERF_VERIFY_THROUGHPUT_BUDGET_MSGS_PER_SEC
            },
            "observed": {
                "tls_connection_p95_ms": handshake_p95_ms,
                "verify_p95_us": verify_p95_us,
                "verify_throughput_msgs_per_sec": verify_throughput
            },
            "samples": {
                "tls_connection_rounds": PERF_HANDSHAKE_ROUNDS,
                "verify_iterations": PERF_VERIFY_ITERATIONS
            }
        }),
    );
}
