pub mod client;
pub mod discovery;
pub mod domain;
pub mod pane;

#[cfg(test)]
pub(crate) static MUX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
