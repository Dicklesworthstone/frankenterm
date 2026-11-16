use anyhow::{Context as _, anyhow};
#[cfg(unix)]
use libc::{AF_UNSPEC, AI_CANONNAME, SOCK_DGRAM};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, IsCa, Issuer,
    KeyPair,
};
use std::path::PathBuf;
#[cfg(windows)]
use winapi::shared::ws2def::{AF_UNSPEC, AI_CANONNAME, SOCK_DGRAM};

/// A helper for managing keys for the TLS server component.
/// Each time the server is started, a new CA is generated
/// and used to generate a new server key, invalidating all
/// prior keys.
/// The idea is that the client connects via some other secure
/// channel (eg: ssh to reach the host, then unix domain to access
/// the server) to make a request for the key information.
/// We'll generate that request a new client cert and return
/// both the public CA certificate information and that key to the client.
/// The client will use both of those things to connect to the TLS
/// server.
pub struct Pki {
    ca_cert: Certificate,
    ca_key: KeyPair,
    ca_params: CertificateParams,
    pki_dir: PathBuf,
}

impl Pki {
    pub fn init() -> anyhow::Result<Self> {
        let pki_dir = config::pki_dir()?;
        std::fs::create_dir_all(&pki_dir)?;
        log::debug!("pki dir is {}", pki_dir.display());

        let hostname = hostname::get()?
            .into_string()
            .map_err(|_| anyhow!("hostname is not representable as unicode"))?;

        let mut alt_names = vec![hostname.clone(), "localhost".to_owned()];

        let hints = dns_lookup::AddrInfoHints {
            flags: AI_CANONNAME,
            address: AF_UNSPEC,
            socktype: SOCK_DGRAM,
            protocol: 0,
        };

        if let Ok(iter) = dns_lookup::getaddrinfo(Some(&hostname), None, Some(hints)) {
            for entry in iter.flatten() {
                if let Some(canon) = entry.canonname {
                    alt_names.push(canon);
                }
            }
        }

        alt_names.sort();
        alt_names.dedup();
        log::debug!("generating cert with alt_names={alt_names:?}");

        let unix_name = config::username_from_env()?;

        // Create the CA certificate
        let mut ca_params = CertificateParams::new(alt_names.clone())?;
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
        ca_params.serial_number = Some(0.into());
        let ca_key = KeyPair::generate()?;
        let ca_cert = ca_params.self_signed(&ca_key)?;
        let ca_pem = ca_cert.pem();
        let ca_pem_path = pki_dir.join("ca.pem");
        std::fs::write(&ca_pem_path, ca_pem.as_bytes())
            .context(format!("saving {}", ca_pem_path.display()))?;

        let mut params = CertificateParams::new(alt_names)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, unix_name);
        params.distinguished_name = dn;

        let ca_issuer = Issuer::from_params(&ca_params, &ca_key);
        let server_key = KeyPair::generate()?;
        let server_cert = params.signed_by(&server_key, &ca_issuer)?;
        let mut signed_cert = server_cert.pem();
        let key_bits = server_key.serialize_pem();
        signed_cert.push_str(&key_bits);

        let server_pem_path = pki_dir.join("server.pem");
        std::fs::write(&server_pem_path, signed_cert.as_bytes())
            .context(format!("saving {}", server_pem_path.display()))?;

        Ok(Self {
            ca_cert,
            ca_key,
            ca_params,
            pki_dir,
        })
    }

    pub fn generate_client_cert(&self) -> anyhow::Result<String> {
        let unix_name = config::username_from_env()?;

        let mut params = CertificateParams::new(vec![unix_name.clone()])?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, unix_name);
        params.distinguished_name = dn;

        let ca_issuer = Issuer::from_params(&self.ca_params, &self.ca_key);
        let client_key = KeyPair::generate()?;
        let client_cert = params.signed_by(&client_key, &ca_issuer)?;
        let mut signed_cert = client_cert.pem();
        let key_bits = client_key.serialize_pem();
        signed_cert.push_str(&key_bits);

        Ok(signed_cert)
    }

    pub fn ca_pem_string(&self) -> anyhow::Result<String> {
        Ok(self.ca_cert.pem())
    }

    pub fn ca_pem(&self) -> PathBuf {
        self.pki_dir.join("ca.pem")
    }

    pub fn server_pem(&self) -> PathBuf {
        self.pki_dir.join("server.pem")
    }
}
