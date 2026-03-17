use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use async_ossl::AsyncSslStream;
use codec::{DecodedPdu, Pdu};
use futures::FutureExt;
use mux::{Mux, MuxNotification};
use smol::Async;
use smol::prelude::*;
use std::sync::Arc;
use wezterm_uds::UnixStream;

#[cfg(unix)]
pub trait AsRawDesc: std::os::unix::io::AsRawFd + std::os::fd::AsFd {}
#[cfg(windows)]
pub trait AsRawDesc: std::os::windows::io::AsRawSocket + std::os::windows::io::AsSocket {}

impl AsRawDesc for UnixStream {}
impl AsRawDesc for AsyncSslStream {}

#[derive(Debug)]
enum Item {
    Notif(MuxNotification),
    WritePdu(DecodedPdu),
    Readable,
}

struct MuxSubscriptionGuard {
    mux: Arc<Mux>,
    sub_id: usize,
}

impl MuxSubscriptionGuard {
    fn new(mux: Arc<Mux>, sub_id: usize) -> Self {
        Self { mux, sub_id }
    }
}

impl Drop for MuxSubscriptionGuard {
    fn drop(&mut self) {
        let _ = self.mux.unsubscribe(self.sub_id);
    }
}

pub async fn process<T>(stream: T) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: AsRawDesc,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    let stream = smol::Async::new(stream)?;
    process_async(stream).await
}

pub async fn process_async<T>(mut stream: Async<T>) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    log::trace!("process_async called");

    let (item_tx, item_rx) = smol::channel::unbounded::<Item>();

    let pdu_sender = PduSender::new({
        let item_tx = item_tx.clone();
        move |pdu| {
            item_tx
                .try_send(Item::WritePdu(pdu))
                .map_err(|e| anyhow::anyhow!("{:?}", e))
        }
    });
    let mut handler = SessionHandler::new(pdu_sender);

    {
        let mux = Mux::get();
        let tx = item_tx.clone();
        let sub_id = mux.subscribe(move |n| tx.try_send(Item::Notif(n)).is_ok());
        let _subscription_guard = MuxSubscriptionGuard::new(mux, sub_id);

        loop {
            let rx_msg = item_rx.recv();
            let wait_for_read = stream.readable().map(|_| Ok(Item::Readable));

            match smol::future::or(rx_msg, wait_for_read).await {
                Ok(Item::Readable) => {
                    let decoded = match Pdu::decode_async(&mut stream, None).await {
                        Ok(data) => data,
                        Err(err) => {
                            if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                                if err.kind() == std::io::ErrorKind::UnexpectedEof {
                                    // Client disconnected: no need to make a noise
                                    return Ok(());
                                }
                            }
                            return Err(err).context("reading Pdu from client");
                        }
                    };
                    handler.process_one(decoded);
                }
                Ok(Item::WritePdu(decoded)) => {
                    match decoded.pdu.encode_async(&mut stream, decoded.serial).await {
                        Ok(()) => {}
                        Err(err) => {
                            if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                                if err.kind() == std::io::ErrorKind::BrokenPipe {
                                    // Client disconnected: no need to make a noise
                                    return Ok(());
                                }
                            }
                            return Err(err).context("encoding PDU to client");
                        }
                    };
                    match stream.flush().await {
                        Ok(()) => {}
                        Err(err) => {
                            if err.kind() == std::io::ErrorKind::BrokenPipe {
                                // Client disconnected: no need to make a noise
                                return Ok(());
                            }
                            return Err(err).context("flushing PDU to client");
                        }
                    }
                }
                Ok(Item::Notif(MuxNotification::PaneOutput(pane_id))) => {
                    handler.schedule_pane_push(pane_id);
                }
                Ok(Item::Notif(MuxNotification::PaneAdded(_pane_id))) => {}
                Ok(Item::Notif(MuxNotification::PaneRemoved(pane_id))) => {
                    Pdu::PaneRemoved(codec::PaneRemoved { pane_id })
                        .encode_async(&mut stream, 0)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(Item::Notif(MuxNotification::Alert { pane_id, alert })) => {
                    {
                        let per_pane = handler.per_pane(pane_id);
                        let mut per_pane = per_pane.lock().unwrap();
                        per_pane.notifications.push(alert);
                    }
                    handler.schedule_pane_push(pane_id);
                }
                Ok(Item::Notif(MuxNotification::SaveToDownloads { .. })) => {}
                Ok(Item::Notif(MuxNotification::AssignClipboard {
                    pane_id,
                    selection,
                    clipboard,
                })) => {
                    Pdu::SetClipboard(codec::SetClipboard {
                        pane_id,
                        clipboard,
                        selection,
                    })
                    .encode_async(&mut stream, 0)
                    .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(Item::Notif(MuxNotification::TabAddedToWindow { tab_id, window_id })) => {
                    Pdu::TabAddedToWindow(codec::TabAddedToWindow { tab_id, window_id })
                        .encode_async(&mut stream, 0)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(Item::Notif(MuxNotification::WindowRemoved(_window_id))) => {}
                Ok(Item::Notif(MuxNotification::WindowCreated(_window_id))) => {}
                Ok(Item::Notif(MuxNotification::WindowInvalidated(_window_id))) => {}
                Ok(Item::Notif(MuxNotification::WindowWorkspaceChanged(window_id))) => {
                    let workspace = {
                        let mux = Mux::get();
                        mux.get_window(window_id)
                            .map(|w| w.get_workspace().to_string())
                    };
                    if let Some(workspace) = workspace {
                        Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                            window_id,
                            workspace,
                        })
                        .encode_async(&mut stream, 0)
                        .await?;
                        stream.flush().await.context("flushing PDU to client")?;
                    }
                }
                Ok(Item::Notif(MuxNotification::PaneFocused(pane_id))) => {
                    Pdu::PaneFocused(codec::PaneFocused { pane_id })
                        .encode_async(&mut stream, 0)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(Item::Notif(MuxNotification::TabResized(tab_id))) => {
                    Pdu::TabResized(codec::TabResized { tab_id })
                        .encode_async(&mut stream, 0)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(Item::Notif(MuxNotification::TabTitleChanged { tab_id, title })) => {
                    Pdu::TabTitleChanged(codec::TabTitleChanged { tab_id, title })
                        .encode_async(&mut stream, 0)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(Item::Notif(MuxNotification::WindowTitleChanged { window_id, title })) => {
                    Pdu::WindowTitleChanged(codec::WindowTitleChanged { window_id, title })
                        .encode_async(&mut stream, 0)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(Item::Notif(MuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                })) => {
                    Pdu::RenameWorkspace(codec::RenameWorkspace {
                        old_workspace,
                        new_workspace,
                    })
                    .encode_async(&mut stream, 0)
                    .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(Item::Notif(MuxNotification::ActiveWorkspaceChanged(_))) => {}
                Ok(Item::Notif(MuxNotification::Empty)) => {}
                Err(err) => {
                    log::error!("process_async Err {}", err);
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn subscription_guard_eagerly_unsubscribes_on_drop() {
        let mux = Arc::new(Mux::new(None));
        let observed = Arc::new(AtomicUsize::new(0));
        let notifications = Arc::clone(&observed);
        let sub_id = mux.subscribe(move |_| {
            notifications.fetch_add(1, Ordering::Relaxed);
            true
        });

        {
            let _guard = MuxSubscriptionGuard::new(Arc::clone(&mux), sub_id);
            mux.notify(MuxNotification::Empty);
            assert_eq!(observed.load(Ordering::Relaxed), 1);
        }

        assert!(
            !mux.unsubscribe(sub_id),
            "subscription guard should remove the subscriber eagerly"
        );
        mux.notify(MuxNotification::Empty);
        assert_eq!(observed.load(Ordering::Relaxed), 1);
    }
}
