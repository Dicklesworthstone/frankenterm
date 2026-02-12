#![cfg(feature = "asupersync-runtime")]

use std::future::ready;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use asupersync::channel::mpsc;
use asupersync::combinator::{Either, Select};
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::unix::{UnixListener, UnixStream};
use asupersync::runtime::RuntimeBuilder;
use asupersync::sync::{Mutex, Semaphore};
use asupersync::{Budget, CancelKind, Cx, LabConfig, LabRuntime, Time};
use tempfile::TempDir;

async fn write_pdu(stream: &mut UnixStream, payload: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(payload.len()).expect("payload length should fit in u32");
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await
}

async fn read_pdu(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut header = [0_u8; 4];
    stream.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header) as usize;
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

fn socket_path(tempdir: &TempDir, file: &str) -> PathBuf {
    tempdir.path().join(file)
}

#[test]
fn spike_unixstream_pdu_framing_round_trip() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");
    let handle = runtime.handle();

    runtime.block_on(async {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket = socket_path(&dir, "spike-pdu.sock");

        let listener = UnixListener::bind(Path::new(&socket))
            .await
            .expect("bind listener");

        let server = handle.spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let payload = read_pdu(&mut stream).await.expect("read pdu");
            assert_eq!(payload, b"frankenterm-spike");
            write_pdu(&mut stream, b"ack").await.expect("write ack");
        });

        let mut client = UnixStream::connect(Path::new(&socket))
            .await
            .expect("connect client");
        write_pdu(&mut client, b"frankenterm-spike")
            .await
            .expect("write payload");
        let ack = read_pdu(&mut client).await.expect("read ack");
        assert_eq!(ack, b"ack");

        server.await;
    });
}

#[test]
fn spike_two_phase_channel_send_in_scope() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");

    runtime.block_on(async {
        let cx = Cx::for_testing();
        let scope = cx.scope();
        assert_eq!(scope.region_id(), cx.region_id());
        assert_eq!(scope.budget(), cx.budget());

        let (tx, rx) = mpsc::channel::<String>(1);
        let permit = tx.reserve(&cx).await.expect("reserve permit");
        permit.send("two-phase-send".to_string());
        let received = rx.recv(&cx).await.expect("receive value");
        assert_eq!(received, "two-phase-send");
    });
}

#[test]
fn spike_pool_pattern_semaphore_mutex_budget_timeout() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");

    runtime.block_on(async {
        let cx = Cx::for_testing();
        let pool = Mutex::new(vec![1_u32]);
        let gate = Semaphore::new(1);

        let permit = gate.acquire(&cx, 1).await.expect("acquire permit");
        {
            let mut entries = pool.lock(&cx).await.expect("lock pool");
            entries.push(2);
            entries.push(3);
            assert_eq!(entries.len(), 3);
        }
        assert_eq!(gate.available_permits(), 0);

        let timeout_budget = Budget::new().with_poll_quota(0);
        assert!(timeout_budget.is_exhausted());
        let timeout_cx = Cx::for_testing_with_budget(timeout_budget);
        timeout_cx.cancel_with(CancelKind::Timeout, Some("budget-timeout probe"));
        assert!(
            gate.acquire(&timeout_cx, 1).await.is_err(),
            "budget-exhausted context should fail fast"
        );

        drop(permit);
        assert_eq!(gate.available_permits(), 1);
    });
}

#[test]
fn spike_labruntime_virtual_time_and_oracle_report() {
    let mut runtime = LabRuntime::new(LabConfig::new(7).worker_count(2).max_steps(10_000));
    let report = runtime.run_until_quiescent_with_report();

    assert!(report.oracle_report.all_passed());
    assert!(report.invariant_violations.is_empty());
    assert_eq!(runtime.now(), Time::ZERO);
}

#[test]
fn spike_select_and_race_semantics() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");

    runtime.block_on(async {
        let selected = Select::new(ready("left"), ready("right")).await;
        assert!(matches!(selected, Either::Left("left")));

        let cx = Cx::for_testing();
        let futures: Vec<Pin<Box<dyn std::future::Future<Output = u8> + Send>>> =
            vec![Box::pin(async { 11_u8 }), Box::pin(async { 22_u8 })];
        let raced = cx.race(futures).await.expect("race should complete");
        assert_eq!(raced, 11);
    });
}
