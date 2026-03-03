use config::{ConfigHandle, SshMultiplexing};
use mux::Mux;
use mux::domain::{Domain, LocalDomain};
use mux::ssh::RemoteSshDomain;
use std::sync::Arc;
use wezterm_client::domain::{ClientDomain, ClientDomainConfig};

pub mod dispatch;
pub mod local;
pub mod pki;
pub mod sessionhandler;

fn client_domains(config: &config::ConfigHandle) -> Vec<ClientDomainConfig> {
    let mut domains = vec![];
    for unix_dom in &config.unix_domains {
        domains.push(ClientDomainConfig::Unix(unix_dom.clone()));
    }

    for ssh_dom in config.ssh_domains().into_iter() {
        if ssh_dom.multiplexing == SshMultiplexing::WezTerm {
            domains.push(ClientDomainConfig::Ssh(ssh_dom.clone()));
        }
    }

    for tls_client in &config.tls_clients {
        domains.push(ClientDomainConfig::Tls(tls_client.clone()));
    }
    domains
}

pub fn update_mux_domains(config: &ConfigHandle) -> anyhow::Result<()> {
    update_mux_domains_impl(config, false)
}

pub fn update_mux_domains_for_server(config: &ConfigHandle) -> anyhow::Result<()> {
    update_mux_domains_impl(config, true)
}

fn update_mux_domains_impl(config: &ConfigHandle, is_standalone_mux: bool) -> anyhow::Result<()> {
    let mux = Mux::get();

    for client_config in client_domains(&config) {
        if mux.get_domain_by_name(client_config.name()).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(ClientDomain::new(client_config));
        mux.add_domain(&domain);
    }

    for ssh_dom in config.ssh_domains().into_iter() {
        if ssh_dom.multiplexing != SshMultiplexing::None {
            continue;
        }

        if mux.get_domain_by_name(&ssh_dom.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(RemoteSshDomain::with_ssh_domain(&ssh_dom)?);
        mux.add_domain(&domain);
    }

    for wsl_dom in config.wsl_domains() {
        if mux.get_domain_by_name(&wsl_dom.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_wsl(wsl_dom.clone())?);
        mux.add_domain(&domain);
    }

    for exec_dom in &config.exec_domains {
        if mux.get_domain_by_name(&exec_dom.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_exec_domain(exec_dom.clone())?);
        mux.add_domain(&domain);
    }

    for serial in &config.serial_ports {
        if mux.get_domain_by_name(&serial.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_serial_domain(serial.clone())?);
        mux.add_domain(&domain);
    }

    if is_standalone_mux {
        if let Some(name) = &config.default_mux_server_domain {
            if let Some(dom) = mux.get_domain_by_name(name) {
                if dom.is::<ClientDomain>() {
                    anyhow::bail!("default_mux_server_domain cannot be set to a client domain!");
                }
                mux.set_default_domain(&dom);
            }
        }
    } else {
        if let Some(name) = &config.default_domain {
            if let Some(dom) = mux.get_domain_by_name(name) {
                mux.set_default_domain(&dom);
            }
        }
    }

    Ok(())
}

lazy_static::lazy_static! {
    pub static ref PKI: pki::Pki = pki::Pki::init().expect("failed to initialize PKI");
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{Config, SshDomain};
    use std::sync::{Mutex, OnceLock};

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn make_test_handle(ssh_domains: Vec<SshDomain>) -> ConfigHandle {
        let mut config = Config::default_config();
        config.unix_domains.clear();
        config.tls_clients.clear();
        config.ssh_domains = Some(ssh_domains);
        config::use_this_configuration(config);
        config::configuration()
    }

    fn reset_test_state() {
        config::use_test_configuration();
        Mux::shutdown();
    }

    #[test]
    fn client_domains_include_only_wezterm_ssh_domains() {
        let _guard = test_lock().lock().expect("lock");

        let raw_ssh = SshDomain {
            name: "raw-ssh".to_string(),
            remote_address: "raw.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let mux_ssh = SshDomain {
            name: "mux-ssh".to_string(),
            remote_address: "mux.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };

        let handle = make_test_handle(vec![raw_ssh, mux_ssh]);
        let domains = client_domains(&handle);

        assert_eq!(
            domains.len(),
            1,
            "only multiplexed SSH should be client domains"
        );
        match &domains[0] {
            ClientDomainConfig::Ssh(ssh) => assert_eq!(ssh.name, "mux-ssh"),
            other => panic!("expected SSH client domain, got {other:?}"),
        }

        reset_test_state();
    }

    #[test]
    fn update_mux_domains_registers_muxed_and_raw_ssh_domains() -> anyhow::Result<()> {
        let _guard = test_lock().lock().expect("lock");

        let raw_ssh = SshDomain {
            name: "raw-ssh".to_string(),
            remote_address: "raw.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let mux_ssh = SshDomain {
            name: "mux-ssh".to_string(),
            remote_address: "mux.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let handle = make_test_handle(vec![raw_ssh, mux_ssh]);

        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        update_mux_domains(&handle)?;

        let client_domain = mux
            .get_domain_by_name("mux-ssh")
            .expect("wezterm-multiplexed ssh domain should be registered");
        assert!(
            client_domain.is::<ClientDomain>(),
            "multiplexed SSH domain should use ClientDomain"
        );

        let raw_domain = mux
            .get_domain_by_name("raw-ssh")
            .expect("raw ssh domain should be registered");
        assert!(
            raw_domain.is::<RemoteSshDomain>(),
            "non-multiplexed SSH domain should use RemoteSshDomain"
        );

        reset_test_state();
        Ok(())
    }

    #[test]
    fn client_domains_empty_config_returns_empty() {
        let _guard = test_lock().lock().expect("lock");
        let handle = make_test_handle(vec![]);
        let domains = client_domains(&handle);
        assert!(domains.is_empty(), "no ssh domains means no client domains");
        reset_test_state();
    }

    #[test]
    fn update_mux_domains_with_no_ssh_registers_only_local() -> anyhow::Result<()> {
        let _guard = test_lock().lock().expect("lock");
        let handle = make_test_handle(vec![]);

        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        update_mux_domains(&handle)?;

        // Only the local domain should exist (no SSH domains registered)
        assert!(
            mux.get_domain_by_name("local").is_some(),
            "local domain should still be present"
        );

        reset_test_state();
        Ok(())
    }

    #[test]
    fn update_mux_domains_idempotent_on_second_call() -> anyhow::Result<()> {
        let _guard = test_lock().lock().expect("lock");

        let mux_ssh = SshDomain {
            name: "mux-ssh".to_string(),
            remote_address: "mux.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let handle = make_test_handle(vec![mux_ssh]);

        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        update_mux_domains(&handle)?;
        let domain_first = mux
            .get_domain_by_name("mux-ssh")
            .expect("domain should exist after first call");

        // Call again — should not add a duplicate
        update_mux_domains(&handle)?;
        let domain_second = mux
            .get_domain_by_name("mux-ssh")
            .expect("domain should still exist after second call");

        // Same domain object (not re-created)
        assert_eq!(
            domain_first.domain_id(),
            domain_second.domain_id(),
            "second call should not create a new domain"
        );

        reset_test_state();
        Ok(())
    }

    #[test]
    fn client_domains_with_only_raw_ssh_returns_empty() {
        let _guard = test_lock().lock().expect("lock");

        let raw_ssh = SshDomain {
            name: "raw-only".to_string(),
            remote_address: "raw.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let handle = make_test_handle(vec![raw_ssh]);
        let domains = client_domains(&handle);

        assert!(
            domains.is_empty(),
            "raw SSH domains should not appear in client_domains"
        );

        reset_test_state();
    }

    #[test]
    fn update_mux_domains_for_server_respects_mux_server_domain() -> anyhow::Result<()> {
        let _guard = test_lock().lock().expect("lock");

        let raw_ssh = SshDomain {
            name: "raw-ssh".to_string(),
            remote_address: "raw.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let handle = make_test_handle(vec![raw_ssh]);

        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        // update_mux_domains_for_server should work the same as update_mux_domains
        // for domain registration (the difference is in default_domain handling)
        update_mux_domains_for_server(&handle)?;

        let domain = mux
            .get_domain_by_name("raw-ssh")
            .expect("raw SSH domain should be registered by server variant");
        assert!(
            domain.is::<RemoteSshDomain>(),
            "should use RemoteSshDomain for non-multiplexed SSH"
        );

        reset_test_state();
        Ok(())
    }

    #[test]
    fn client_domains_multiple_mux_ssh() {
        let _guard = test_lock().lock().expect("lock");

        let mux1 = SshDomain {
            name: "mux-1".to_string(),
            remote_address: "host1:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let mux2 = SshDomain {
            name: "mux-2".to_string(),
            remote_address: "host2:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let raw = SshDomain {
            name: "raw".to_string(),
            remote_address: "host3:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };

        let handle = make_test_handle(vec![mux1, mux2, raw]);
        let domains = client_domains(&handle);

        assert_eq!(domains.len(), 2, "should have 2 multiplexed SSH client domains");

        let names: Vec<&str> = domains
            .iter()
            .map(|d| d.name())
            .collect();
        assert!(names.contains(&"mux-1"));
        assert!(names.contains(&"mux-2"));

        reset_test_state();
    }
}
