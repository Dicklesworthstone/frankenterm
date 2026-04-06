#![cfg(all(unix, not(target_os = "macos")))]

//! <https://github.com/flatpak/xdg-desktop-portal/blob/main/data/org.freedesktop.portal.Settings.xml>

use crate::{Appearance, Connection, ConnectionOps};
use anyhow::Context;
use futures_lite::future::FutureExt;
use futures_util::stream::StreamExt;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zbus::proxy;
use zvariant::OwnedValue;

#[proxy(
    interface = "org.freedesktop.portal.Settings",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait PortalSettings {
    fn ReadAll(
        &self,
        namespaces: &[&str],
    ) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;

    fn Read(&self, namespace: &str, key: &str) -> zbus::Result<OwnedValue>;

    #[zbus(signal)]
    fn SettingChanged(&self, namespace: &str, key: &str, value: OwnedValue) -> zbus::Result<()>;
}

#[derive(PartialEq)]
enum CachedAppearance {
    /// Never tried to determine appearance
    Unknown,
    /// Tried and failed
    None,
    /// We got it
    Some(Appearance),
}

impl CachedAppearance {
    fn to_result(&self) -> anyhow::Result<Option<Appearance>> {
        match self {
            Self::Unknown => anyhow::bail!("Appearance is Unknown"),
            Self::None => Ok(None),
            Self::Some(a) => Ok(Some(*a)),
        }
    }
}

struct State {
    appearance: CachedAppearance,
    subscribe_running: bool,
    refresh_running: bool,
    last_update: Instant,
}

lazy_static::lazy_static! {
  static ref STATE: Mutex<State> = Mutex::new(
          State {
              appearance: CachedAppearance::Unknown,
              subscribe_running: false,
              refresh_running: false,
              last_update: Instant::now(),
          }
   );
}

fn cached_appearance_for_state(state: &State) -> anyhow::Result<Option<Appearance>> {
    match &state.appearance {
        CachedAppearance::Some(_)
            if state.subscribe_running || state.last_update.elapsed() < Duration::from_secs(1) =>
        {
            state.appearance.to_result()
        }
        CachedAppearance::None => Ok(None),
        CachedAppearance::Some(_) | CachedAppearance::Unknown => {
            anyhow::bail!("Appearance cache is cold")
        }
    }
}

pub fn get_appearance_if_cached() -> anyhow::Result<Option<Appearance>> {
    let state = STATE.lock().unwrap();
    cached_appearance_for_state(&state)
}

pub fn refresh_appearance_in_background() {
    let should_spawn = {
        let mut state = STATE.lock().unwrap();
        if state.refresh_running || cached_appearance_for_state(&state).is_ok() {
            false
        } else {
            state.refresh_running = true;
            true
        }
    };

    if !should_spawn {
        return;
    }

    promise::spawn::spawn(async move {
        let refreshed = read_setting("org.freedesktop.appearance", "color-scheme")
            .await
            .and_then(value_to_appearance);

        let maybe_notify = {
            let mut state = STATE.lock().unwrap();
            state.refresh_running = false;
            state.last_update = Instant::now();

            match &refreshed {
                Ok(appearance) => {
                    let changed = state.appearance != CachedAppearance::Some(*appearance);
                    state.appearance = CachedAppearance::Some(*appearance);
                    changed.then_some(*appearance)
                }
                Err(_) => {
                    state.appearance = CachedAppearance::None;
                    None
                }
            }
        };

        match refreshed {
            Ok(_) => {
                if let Some(appearance) = maybe_notify {
                    if let Some(conn) = Connection::get() {
                        conn.advise_of_appearance_change(appearance);
                    }
                }
            }
            Err(err) => {
                log::warn!("Unable to resolve appearance using xdg-desktop-portal: {err:#}");
            }
        }
    })
    .detach();
}

pub async fn read_setting(namespace: &str, key: &str) -> anyhow::Result<OwnedValue> {
    let connection = zbus::ConnectionBuilder::session()?.build().await?;
    let proxy = PortalSettingsProxy::new(&connection)
        .await
        .context("make proxy")?;

    proxy
        .Read(namespace, key)
        .or(async {
            promise::spawn::sleep(std::time::Duration::from_secs(1)).await;
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Timed out reading from xdg-portal; this indicates a problem \
                 with your graphical environment. Consider running \
                 'systemctl restart --user xdg-desktop-portal.service'",
            )
            .into())
        })
        .await
        .with_context(|| format!("Reading xdg-portal {namespace} {key}"))
}

fn value_to_appearance(value: OwnedValue) -> anyhow::Result<Appearance> {
    Ok(match value.downcast_ref::<u32>() {
        Ok(1) => Appearance::Dark,
        Ok(_) => Appearance::Light,
        Err(err) => {
            anyhow::bail!(
                "Unable to resolve appearance \
                 using xdg-desktop-portal: {err:#?}"
            );
        }
    })
}

pub async fn get_appearance() -> anyhow::Result<Option<Appearance>> {
    if let Ok(cached) = get_appearance_if_cached() {
        return Ok(cached);
    }

    match read_setting("org.freedesktop.appearance", "color-scheme").await {
        Ok(value) => {
            let appearance = value_to_appearance(value).context("value_to_appearance")?;
            let mut state = STATE.lock().unwrap();
            state.appearance = CachedAppearance::Some(appearance);
            state.last_update = Instant::now();
            Ok(Some(appearance))
        }
        Err(err) => {
            let mut state = STATE.lock().unwrap();
            // Cache that we didn't get any value, so we can avoid
            // repeating this query again later
            state.appearance = CachedAppearance::None;
            state.last_update = Instant::now();
            // but bubble up the underlying message so that we can
            // log a warning elsewhere
            Err(err).context("get_appearance.read_setting")
        }
    }
}

pub async fn run_signal_loop(stream: &mut SettingChangedStream<'_>) -> Result<(), anyhow::Error> {
    // query appearance again as it might have changed without us knowing
    if let Ok(value) =
        value_to_appearance(read_setting("org.freedesktop.appearance", "color-scheme").await?)
    {
        let mut state = STATE.lock().unwrap();
        if state.appearance != CachedAppearance::Some(value) {
            state.appearance = CachedAppearance::Some(value);
            state.last_update = Instant::now();
            drop(state);
            let conn = Connection::get().ok_or_else(|| anyhow::anyhow!("connection is dead"))?;
            conn.advise_of_appearance_change(value);
        }
    }

    while let Some(signal) = stream.next().await {
        let args = signal.args()?;
        if args.namespace == "org.freedesktop.appearance" && args.key == "color-scheme" {
            if let Ok(appearance) = value_to_appearance(args.value) {
                let mut state = STATE.lock().unwrap();
                state.appearance = CachedAppearance::Some(appearance);
                state.last_update = Instant::now();
                drop(state);
                let conn =
                    Connection::get().ok_or_else(|| anyhow::anyhow!("connection is dead"))?;
                conn.advise_of_appearance_change(appearance);
            }
        }
    }
    Result::<(), anyhow::Error>::Ok(())
}

pub fn subscribe() {
    promise::spawn::spawn(async move {
        let connection = zbus::ConnectionBuilder::session()?.build().await?;
        let proxy = PortalSettingsProxy::new(&connection)
            .await
            .context("make proxy")?;
        let mut stream = proxy.receive_SettingChanged().await?;

        STATE.lock().unwrap().subscribe_running = true;
        let res = run_signal_loop(&mut stream).await;
        STATE.lock().unwrap().subscribe_running = false;

        res
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::{cached_appearance_for_state, CachedAppearance, State};
    use crate::Appearance;
    use std::time::{Duration, Instant};

    fn make_state(appearance: CachedAppearance, subscribe_running: bool, age: Duration) -> State {
        State {
            appearance,
            subscribe_running,
            refresh_running: false,
            last_update: Instant::now() - age,
        }
    }

    #[test]
    fn cached_appearance_is_returned_while_subscription_is_running() {
        let state = make_state(
            CachedAppearance::Some(Appearance::Dark),
            true,
            Duration::from_secs(5),
        );
        assert_eq!(
            cached_appearance_for_state(&state).unwrap(),
            Some(Appearance::Dark)
        );
    }

    #[test]
    fn stale_cached_appearance_is_treated_as_cold_without_subscription() {
        let state = make_state(
            CachedAppearance::Some(Appearance::Light),
            false,
            Duration::from_secs(2),
        );
        assert!(cached_appearance_for_state(&state).is_err());
    }

    #[test]
    fn cached_none_is_returned_without_refresh() {
        let state = make_state(CachedAppearance::None, false, Duration::from_secs(30));
        assert_eq!(cached_appearance_for_state(&state).unwrap(), None);
    }
}
