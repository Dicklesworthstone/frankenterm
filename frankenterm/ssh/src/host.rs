use crate::runtime::channel::{bounded, Sender};
use crate::session::SessionEvent;
#[cfg(feature = "ssh2")]
use anyhow::anyhow;
use anyhow::Context;
#[cfg(feature = "ssh2")]
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error(
    "host key mismatch for ssh server {remote_address}. Got fingerprint {key} instead of the expected value from your known hosts file {file:?}."
)]
pub struct HostVerificationFailed {
    pub remote_address: String,
    pub key: String,
    pub file: Option<std::path::PathBuf>,
}

#[derive(Debug)]
pub struct HostVerificationEvent {
    pub message: String,
    pub(crate) reply: Sender<bool>,
}

impl HostVerificationEvent {
    pub async fn answer(self, trust_host: bool) -> anyhow::Result<()> {
        Ok(self.reply.send(trust_host).await?)
    }
    pub fn try_answer(self, trust_host: bool) -> anyhow::Result<()> {
        Ok(self.reply.try_send(trust_host)?)
    }
}

#[cfg(feature = "ssh2")]
fn known_hosts_files_from_config(config: &crate::config::ConfigMap) -> Vec<PathBuf> {
    config
        .get("userknownhostsfile")
        .map(|value| {
            value
                .split_whitespace()
                .filter(|path| !path.eq_ignore_ascii_case("none"))
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(feature = "ssh2")]
fn known_hosts_write_target(files: &[PathBuf]) -> anyhow::Result<&Path> {
    files
        .first()
        .map(PathBuf::as_path)
        .ok_or_else(|| anyhow!("no UserKnownHostsFile configured; refusing to trust ssh host key"))
}

#[cfg(feature = "ssh2")]
fn known_hosts_file_needs_leading_newline(file: &Path) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};

    let mut existing = std::fs::File::open(file)?;
    if existing.metadata()?.len() == 0 {
        return Ok(false);
    }

    existing.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    existing.read_exact(&mut last)?;
    Ok(last != *b"\n")
}

#[cfg(feature = "ssh2")]
fn append_known_host_entry(
    known_hosts: &ssh2::KnownHosts,
    host_and_port: &str,
    file: &Path,
) -> anyhow::Result<()> {
    use std::io::Write;

    let host = known_hosts
        .iter()
        .context("listing known_hosts entries after add")?
        .into_iter()
        .rev()
        .find(|host| host.name() == Some(host_and_port))
        .ok_or_else(|| {
            anyhow!("failed to find newly-added known_hosts entry for {host_and_port}")
        })?;

    let mut line = known_hosts
        .write_string(&host, ssh2::KnownHostFileKind::OpenSSH)
        .context("formatting known_hosts entry")?;
    if !line.ends_with('\n') {
        line.push('\n');
    }

    if let Some(parent) = file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating known_hosts directory {}", parent.display()))?;
    }

    let needs_leading_newline = match file.metadata() {
        Ok(metadata) if metadata.len() > 0 => known_hosts_file_needs_leading_newline(file)
            .with_context(|| format!("checking known_hosts newline state {}", file.display()))?,
        Ok(_) => false,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            return Err(err)
                .with_context(|| format!("checking known_hosts file {}", file.display()));
        }
    };

    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .with_context(|| format!("opening known_hosts file {}", file.display()))?;
    if needs_leading_newline {
        out.write_all(b"\n")
            .with_context(|| format!("separating known_hosts entry in {}", file.display()))?;
    }
    out.write_all(line.as_bytes())
        .with_context(|| format!("appending known_hosts entry to {}", file.display()))?;

    Ok(())
}

impl crate::sessioninner::SessionInner {
    #[cfg(feature = "libssh-rs")]
    pub fn host_verification_libssh(
        &mut self,
        sess: &libssh_rs::Session,
        hostname: &str,
        port: u16,
    ) -> anyhow::Result<()> {
        let key = sess
            .get_server_public_key()?
            .get_public_key_hash_hexa(libssh_rs::PublicKeyHashType::Sha256)?;

        match sess.is_known_server()? {
            libssh_rs::KnownHosts::Ok => Ok(()),
            libssh_rs::KnownHosts::NotFound | libssh_rs::KnownHosts::Unknown => {
                let (reply, confirm) = bounded(1);
                self.tx_event
                    .try_send(SessionEvent::HostVerify(HostVerificationEvent {
                        message: format!(
                            "SSH host {}:{} is not yet trusted.\n\
                                    Fingerprint: {}.\n\
                                    Trust and continue connecting?",
                            hostname, port, key
                        ),
                        reply,
                    }))
                    .context("sending HostVerify request to user")?;

                let trusted = crate::runtime::block_on(confirm.recv())
                    .context("waiting for host verification confirmation from user")?;

                if !trusted {
                    anyhow::bail!("user declined to trust host");
                }

                Ok(sess.update_known_hosts_file()?)
            }
            libssh_rs::KnownHosts::Changed => {
                let mut file = None;
                if let Some(kh) = self.config.get("userknownhostsfile") {
                    if let Some(candidate) = kh.split_whitespace().next() {
                        file.replace(candidate.into());
                    }
                }

                let failed = HostVerificationFailed {
                    remote_address: format!("{hostname}:{port}"),
                    key,
                    file,
                };
                self.tx_event
                    .try_send(SessionEvent::HostVerificationFailed(failed))
                    .context("sending HostVerificationFailed event to user")?;
                anyhow::bail!("Host key verification failed");
            }
            libssh_rs::KnownHosts::Other => {
                anyhow::bail!(
                    "The host key for this server was not found, but another\n\
            type of key exists. An attacker might change the default\n\
            server key to confuse your client into thinking the key\n\
            does not exist"
                );
            }
        }
    }

    #[cfg(feature = "ssh2")]
    pub fn host_verification(
        &mut self,
        sess: &ssh2::Session,
        remote_host_name: &str,
        port: u16,
        remote_address: &str,
    ) -> anyhow::Result<()> {
        use std::io::Write;

        let mut known_hosts = sess.known_hosts().context("preparing known hosts")?;
        let known_hosts_files = known_hosts_files_from_config(&self.config);
        let write_target = known_hosts_write_target(&known_hosts_files)?;
        let mut mismatch_file = None;

        for file in &known_hosts_files {
            if !file.exists() {
                continue;
            }

            known_hosts
                .read_file(file, ssh2::KnownHostFileKind::OpenSSH)
                .with_context(|| format!("reading known_hosts file {}", file.display()))?;
            mismatch_file.get_or_insert_with(|| file.clone());
        }

        let (key, key_type) = sess
            .host_key()
            .ok_or_else(|| anyhow!("failed to get ssh host key"))?;

        let fingerprint = sess
            .host_key_hash(ssh2::HashType::Sha256)
            .map(|fingerprint| {
                use base64::Engine;
                let engine = base64::engine::general_purpose::GeneralPurpose::new(
                    &base64::alphabet::STANDARD,
                    base64::engine::general_purpose::NO_PAD,
                );
                format!("SHA256:{}", engine.encode(fingerprint))
            })
            .or_else(|| {
                // Querying for the Sha256 can fail if for example we were linked
                // against libssh < 1.9, so let's fall back to Sha1 in that case.
                sess.host_key_hash(ssh2::HashType::Sha1).map(|fingerprint| {
                    let mut res = vec![];
                    write!(&mut res, "SHA1").ok();
                    for b in fingerprint {
                        write!(&mut res, ":{:02x}", *b).ok();
                    }
                    String::from_utf8(res).unwrap()
                })
            })
            .ok_or_else(|| anyhow!("failed to get host fingerprint"))?;

        match known_hosts.check_port(&remote_host_name, port, key) {
            ssh2::CheckResult::Match => {}
            ssh2::CheckResult::NotFound => {
                let (reply, confirm) = bounded(1);
                self.tx_event
                    .try_send(SessionEvent::HostVerify(HostVerificationEvent {
                        message: format!(
                            "SSH host {} is not yet trusted.\n\
                            {:?} Fingerprint: {}.\n\
                            Trust and continue connecting?",
                            remote_address, key_type, fingerprint
                        ),
                        reply,
                    }))
                    .context("sending HostVerify request to user")?;

                let trusted = crate::runtime::block_on(confirm.recv())
                    .context("waiting for host verification confirmation from user")?;

                if !trusted {
                    anyhow::bail!("user declined to trust host");
                }

                let host_and_port = if port != 22 {
                    format!("[{}]:{}", remote_host_name, port)
                } else {
                    remote_host_name.to_string()
                };

                known_hosts
                    .add(&host_and_port, key, &remote_address, key_type.into())
                    .context("adding known_hosts entry in memory")?;
                append_known_host_entry(&known_hosts, &host_and_port, write_target)?;
            }
            ssh2::CheckResult::Mismatch => {
                let failed = HostVerificationFailed {
                    remote_address: remote_address.to_string(),
                    key: fingerprint,
                    file: mismatch_file.or_else(|| Some(write_target.to_path_buf())),
                };
                self.tx_event
                    .try_send(SessionEvent::HostVerificationFailed(failed))
                    .context("sending HostVerificationFailed event to user")?;
                anyhow::bail!("Host key verification failed");
            }
            ssh2::CheckResult::Failure => {
                anyhow::bail!("failed to check the known hosts");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "ssh2")]
    #[test]
    fn known_hosts_files_from_config_preserves_configured_order() {
        let mut config = crate::config::ConfigMap::new();
        config.insert(
            "userknownhostsfile".to_string(),
            "/tmp/ft-known-hosts-a none   /tmp/ft-known-hosts-b".to_string(),
        );

        let files = known_hosts_files_from_config(&config);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], PathBuf::from("/tmp/ft-known-hosts-a"));
        assert_eq!(files[1], PathBuf::from("/tmp/ft-known-hosts-b"));
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn known_hosts_write_target_uses_first_configured_path_even_when_missing() {
        let files = vec![
            PathBuf::from("/tmp/ft-missing-known-hosts-a"),
            PathBuf::from("/tmp/ft-missing-known-hosts-b"),
        ];

        let target = known_hosts_write_target(&files).unwrap();
        assert_eq!(target, files[0].as_path());
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn known_hosts_write_target_rejects_absent_config() {
        let files = Vec::new();
        let err = known_hosts_write_target(&files).unwrap_err();
        assert!(err.to_string().contains("no UserKnownHostsFile"));
    }

    #[test]
    fn host_verification_failed_display_with_file() {
        let err = HostVerificationFailed {
            remote_address: "example.com:22".to_string(),
            key: "SHA256:abc123".to_string(),
            file: Some(std::path::PathBuf::from("/home/user/.ssh/known_hosts")),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("example.com:22"));
        assert!(msg.contains("SHA256:abc123"));
        assert!(msg.contains("known_hosts"));
    }

    #[test]
    fn host_verification_failed_display_without_file() {
        let err = HostVerificationFailed {
            remote_address: "10.0.0.1:2222".to_string(),
            key: "SHA256:xyz789".to_string(),
            file: None,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("10.0.0.1:2222"));
        assert!(msg.contains("SHA256:xyz789"));
        assert!(msg.contains("None"));
    }

    #[test]
    fn host_verification_failed_debug() {
        let err = HostVerificationFailed {
            remote_address: "host:22".to_string(),
            key: "fingerprint".to_string(),
            file: None,
        };
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("HostVerificationFailed"));
        assert!(dbg.contains("host:22"));
    }

    #[test]
    fn host_verification_failed_is_error() {
        let err = HostVerificationFailed {
            remote_address: "host:22".to_string(),
            key: "fp".to_string(),
            file: None,
        };
        // Verify it implements std::error::Error via thiserror
        let error: &dyn std::error::Error = &err;
        assert!(error.to_string().contains("host:22"));
    }

    #[test]
    fn host_verification_event_debug() {
        let (tx, _rx) = bounded(1);
        let event = HostVerificationEvent {
            message: "Trust this host?".to_string(),
            reply: tx,
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("HostVerificationEvent"));
        assert!(dbg.contains("Trust this host?"));
    }

    #[test]
    fn host_verification_event_try_answer_trust() {
        let (tx, rx) = bounded(1);
        let event = HostVerificationEvent {
            message: "Trust?".to_string(),
            reply: tx,
        };
        event.try_answer(true).unwrap();
        let result = crate::runtime::block_on(rx.recv()).unwrap();
        assert!(result);
    }

    #[test]
    fn host_verification_event_try_answer_reject() {
        let (tx, rx) = bounded(1);
        let event = HostVerificationEvent {
            message: "Trust?".to_string(),
            reply: tx,
        };
        event.try_answer(false).unwrap();
        let result = crate::runtime::block_on(rx.recv()).unwrap();
        assert!(!result);
    }

    #[test]
    fn host_verification_event_async_answer() {
        let (tx, rx) = bounded(1);
        let event = HostVerificationEvent {
            message: "Trust?".to_string(),
            reply: tx,
        };
        crate::runtime::block_on(async {
            event.answer(true).await.unwrap();
            let result = rx.recv().await.unwrap();
            assert!(result);
        });
    }

    #[test]
    fn host_verification_failed_source_is_none() {
        let err = HostVerificationFailed {
            remote_address: "host:22".to_string(),
            key: "fp".to_string(),
            file: None,
        };
        let error: &dyn std::error::Error = &err;
        assert!(error.source().is_none());
    }

    #[test]
    fn host_verification_failed_with_path() {
        let err = HostVerificationFailed {
            remote_address: "server.example.com:22".to_string(),
            key: "SHA256:abcdef1234567890".to_string(),
            file: Some(std::path::PathBuf::from(
                "/very/long/path/to/.ssh/known_hosts",
            )),
        };
        let msg = err.to_string();
        assert!(msg.contains("server.example.com:22"));
        assert!(msg.contains("SHA256:abcdef1234567890"));
        assert!(msg.contains("/very/long/path/to/.ssh/known_hosts"));
    }

    #[test]
    fn host_verification_event_message_accessible() {
        let (tx, _rx) = bounded(1);
        let event = HostVerificationEvent {
            message: "Do you trust this host?".to_string(),
            reply: tx,
        };
        assert_eq!(event.message, "Do you trust this host?");
    }

    #[test]
    fn host_verification_event_async_reject() {
        let (tx, rx) = bounded(1);
        let event = HostVerificationEvent {
            message: "Trust?".to_string(),
            reply: tx,
        };
        crate::runtime::block_on(async {
            event.answer(false).await.unwrap();
            let result = rx.recv().await.unwrap();
            assert!(!result);
        });
    }
}
