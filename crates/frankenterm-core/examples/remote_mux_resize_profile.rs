//! Persistent real-socket resize diagnostic for one explicitly owned mux pane.
//!
//! Build via strict RCH with `--features vendored --profile release-perf`.
//! Run under an external process watchdog, with stdout retained as JSONL:
//! `remote_mux_resize_profile SOCKET SERVER_PID PANE_ID TAB_ID CORPUS TRIALS`.
//! The companion `scripts/remote_mux_resize_fixture.py` must own the sole PTY.
//! UnitResponse measures admission; render dimensions plus a nonce-bearing
//! TIOCGWINSZ reply measure client-observed convergence including observer cost.
//! This does not measure native rendering, network transport, or display latency.

#[cfg(not(all(unix, feature = "vendored")))]
fn main() {
    eprintln!("remote_mux_resize_profile requires Unix and --features vendored");
    std::process::exit(2);
}

#[cfg(all(unix, feature = "vendored"))]
fn main() -> anyhow::Result<()> {
    measured::run()
}

#[cfg(all(unix, feature = "vendored"))]
mod measured {
    use anyhow::{Context, Result, bail, ensure};
    use frankenterm_core::cx::Cx;
    use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder, sleep_with_cx};
    use frankenterm_core::vendored::{DirectMuxClient, DirectMuxClientConfig, MuxTextReadResult};
    use frankenterm_term::TerminalSize;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const TEXT_CAP: usize = 16 * 1024 * 1024;
    const SETTLE: Duration = Duration::from_secs(5);

    struct Workload {
        socket: PathBuf,
        server_pid: u32,
        pane: u64,
        tab: u64,
        expected: String,
        ready: String,
        corpus_sha256: String,
        trials: usize,
    }

    fn emit(value: Value) -> Result<()> {
        let mut out = std::io::stdout().lock();
        serde_json::to_writer(&mut out, &value)?;
        writeln!(out)?;
        out.flush()?;
        Ok(())
    }

    fn physical_join(text: &str) -> String {
        text.chars().filter(|c| *c != '\r' && *c != '\n').collect()
    }

    fn complete_text(text: MuxTextReadResult) -> Result<String> {
        match text {
            MuxTextReadResult::Text(text)
            | MuxTextReadResult::Bounded {
                text,
                truncated: false,
                ..
            } => Ok(text),
            _ => bail!("text oracle received an incomplete or over-cap snapshot"),
        }
    }

    impl Workload {
        fn parse() -> Result<Self> {
            let args: Vec<_> = std::env::args_os().skip(1).collect();
            ensure!(
                args.len() == 6,
                "expected SOCKET SERVER_PID PANE_ID TAB_ID CORPUS TRIALS"
            );
            let number = |index: usize| -> Result<u64> {
                args[index]
                    .to_str()
                    .context("non-UTF8 integer")?
                    .parse()
                    .map_err(Into::into)
            };
            let socket = PathBuf::from(&args[0]);
            ensure!(socket.is_absolute(), "private socket must be absolute");
            let server_pid = u32::try_from(number(1)?)?;
            ensure!(server_pid > 1, "invalid owned server PID");
            let mut bytes = Vec::new();
            fs::File::open(&args[4])?
                .take(TEXT_CAP as u64 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= TEXT_CAP, "corpus exceeds cap");
            let text = String::from_utf8(bytes)?;
            ensure!(
                text.ends_with('\n') && !text.contains('\r'),
                "invalid corpus line endings"
            );
            let lines: Vec<_> = text.lines().collect();
            ensure!(
                lines.len() == 10_000,
                "baseline requires exactly 10,000 original records"
            );
            for (index, line) in lines.iter().enumerate() {
                ensure!(
                    line.starts_with(&format!("FT_RECORD_{index:05d} "))
                        && line.ends_with(&format!("FT_END_{index:05d}")),
                    "invalid record {index}"
                );
            }
            let corpus_sha256 = format!("{:x}", Sha256::digest(text.as_bytes()));
            let trials = usize::try_from(number(5)?)?;
            ensure!((1..=100).contains(&trials), "trials must be 1..100");
            Ok(Self {
                socket,
                server_pid,
                pane: number(2)?,
                tab: number(3)?,
                expected: physical_join(&text),
                ready: format!("FT_CORPUS_READY {corpus_sha256}"),
                corpus_sha256,
                trials,
            })
        }

        fn verify_lease(&self) -> Result<()> {
            let lock = PathBuf::from(format!("{}.lock", self.socket.display()));
            let lease = fs::read_to_string(lock)?;
            let metadata = fs::metadata(&self.socket)?;
            ensure!(
                metadata.file_type().is_socket(),
                "private endpoint is not a socket"
            );
            let fields: Vec<_> = lease.split_whitespace().collect();
            ensure!(
                fields.first() == Some(&"FT_SOCKET_LEASE_V1")
                    && fields.get(1) == Some(&format!("pid={}", self.server_pid).as_str())
                    && fields.get(2) == Some(&format!("dev={}", metadata.dev()).as_str())
                    && fields.get(3) == Some(&format!("ino={}", metadata.ino()).as_str()),
                "private socket lease does not identify the owned server and socket inode"
            );
            Ok(())
        }

        async fn oracle(&self, client: &mut DirectMuxClient, cx: &Cx) -> Result<()> {
            let text = complete_text(client.get_text_with_cx(cx, self.pane, TEXT_CAP).await?)?;
            let joined = physical_join(&text);
            let (records, suffix) = joined
                .split_once(&self.ready)
                .context("missing corpus ready marker")?;
            ensure!(
                records == self.expected,
                "exact ordered Unicode/whitespace corpus differs"
            );
            // Only known fixture responses may follow the corpus, never another record.
            let mut suffix = suffix.trim_end_matches(' ');
            while !suffix.is_empty() {
                suffix = suffix
                    .strip_prefix("FT_PROBE ")
                    .context("unexpected corpus suffix")?;
                let (nonce, rest) = suffix.split_once(' ').context("missing probe nonce")?;
                ensure!(
                    !nonce.is_empty()
                        && nonce
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "invalid probe nonce"
                );
                let (rows, rest) = rest.split_once(' ').context("missing probe rows")?;
                ensure!(rows.parse::<usize>().is_ok(), "invalid probe rows");
                let end = rest
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(rest.len());
                ensure!(end > 0, "missing probe columns");
                suffix = &rest[end..];
            }
            Ok(())
        }

        async fn resize(
            &self,
            client: &mut DirectMuxClient,
            cx: &Cx,
            cols: usize,
            nonce: &str,
        ) -> Result<Value> {
            self.verify_lease()?;
            let before = client
                .get_pane_render_changes_with_cx(cx, self.pane)
                .await?;
            let started = Instant::now();
            client
                .resize_with_cx(
                    cx,
                    self.tab,
                    self.pane,
                    TerminalSize {
                        rows: 24,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                        dpi: 0,
                    },
                )
                .await?;
            let admission_us = started.elapsed().as_micros();
            let mut polls = 0usize;
            let terminal_us;
            let mut previous_seqno = before.seqno;
            loop {
                ensure!(
                    started.elapsed() < SETTLE,
                    "terminal geometry convergence deadline expired"
                );
                let render = client
                    .get_pane_render_changes_with_cx(cx, self.pane)
                    .await?;
                polls += 1;
                ensure!(render.seqno >= previous_seqno, "render sequence regressed");
                previous_seqno = render.seqno;
                if render.dimensions.cols == cols && render.dimensions.viewport_rows == 24 {
                    terminal_us = started.elapsed().as_micros();
                    break;
                }
                sleep_with_cx(cx, Duration::from_millis(1))
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
            client
                .write_to_pane_with_cx(cx, self.pane, format!("PROBE {nonce}\n").into_bytes())
                .await?;
            let expected_probe = format!("FT_PROBE {nonce} 24 {cols}");
            loop {
                ensure!(
                    started.elapsed() < SETTLE,
                    "PTY echo convergence deadline expired"
                );
                let text = complete_text(
                    client
                        .get_text_tail_with_cx(cx, self.pane, 65536, Some(20))
                        .await?,
                )?;
                polls += 1;
                let joined = physical_join(&text);
                if joined
                    .split_once(&expected_probe)
                    .is_some_and(|(_, rest)| !rest.starts_with(|c: char| c.is_ascii_digit()))
                {
                    break;
                }
                sleep_with_cx(cx, Duration::from_millis(1))
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
            let convergence_us = started.elapsed().as_micros();
            // Full history correctness is deliberately outside the timed interval.
            self.oracle(client, cx).await?;
            Ok(json!({"status":"passed", "columns":cols, "rows":24,
                "from_columns":before.dimensions.cols, "admission_us":admission_us,
                "terminal_observed_us":terminal_us, "pty_echo_convergence_us":convergence_us,
                "observer_polls":polls, "sequence_before":before.seqno,
                "sequence_at_terminal_geometry":previous_seqno,
                "exact_corpus_preserved":true}))
        }

        async fn measure(&self) -> Result<()> {
            self.verify_lease()?;
            let cx = Cx::for_request();
            let mut client = DirectMuxClient::connect_with_cx(
                &cx,
                DirectMuxClientConfig {
                    socket_path: Some(self.socket.clone()),
                    connect_timeout: Duration::from_secs(5),
                    read_timeout: Duration::from_secs(5),
                    write_timeout: Duration::from_secs(5),
                    ..DirectMuxClientConfig::default()
                },
            )
            .await?;
            let topology = client.list_panes_with_cx(&cx).await?;
            ensure!(
                topology.tabs.len() == 1 && topology.floating_panes.is_empty(),
                "workload requires exactly one owned tab and no floating panes"
            );
            match &topology.tabs[0] {
                mux::tab::PaneNode::Leaf(pane) => ensure!(
                    pane.pane_id as u64 == self.pane && pane.tab_id as u64 == self.tab,
                    "owned pane/tab identity mismatch"
                ),
                _ => bail!("workload requires one unsplit owned pane"),
            }
            let ingestion = Instant::now();
            loop {
                ensure!(
                    ingestion.elapsed() < Duration::from_secs(120),
                    "initial corpus parse deadline expired"
                );
                let tail = complete_text(
                    client
                        .get_text_tail_with_cx(&cx, self.pane, 65536, Some(20))
                        .await?,
                )?;
                if physical_join(&tail).contains(&self.ready) {
                    break;
                }
                sleep_with_cx(&cx, Duration::from_millis(20))
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
            self.oracle(&mut client, &cx)
                .await
                .context("initial corpus oracle")?;
            emit(
                json!({"event":"contract", "version":1, "trials":self.trials,
                "scope":"remote-host-private-Unix-socket; excludes native and network",
                "boundary":"request start to observed terminal dimensions and real PTY geometry echo",
                "observer_cost_included":true,"corpus_sha256":self.corpus_sha256,
                "socket":self.socket,"server_pid":self.server_pid,"pane_id":self.pane,"tab_id":self.tab,
                "columns":[120,60,100,80],"rows":24,"settle_deadline_ms":5000}),
            )?;
            emit(
                json!({"event":"warmup", "measurement":self.resize(&mut client, &cx, 80, "warmup").await?}),
            )?;
            for trial in 0..self.trials {
                for cols in [120, 60, 100, 80] {
                    let nonce = format!("trial_{trial:03}_cols_{cols}");
                    match self.resize(&mut client, &cx, cols, &nonce).await {
                        Ok(mut row) => {
                            row["event"] = json!("trial");
                            row["trial"] = json!(trial);
                            emit(row)?;
                        }
                        Err(error) => {
                            emit(json!({"event":"trial", "trial":trial,"columns":cols,
                                "status":"failed","error":format!("{error:#}")}))?;
                            return Err(error);
                        }
                    }
                }
            }
            emit(
                json!({"event":"complete","status":"passed","trials_per_geometry":self.trials,
                "samples":self.trials * 4}),
            )?;
            Ok(())
        }
    }

    pub fn run() -> Result<()> {
        ensure!(
            std::env::var("FT_REMOTE_MUX_PROFILE_WATCHDOG_SECONDS").as_deref() == Ok("600"),
            "run through the owned fixture measure wrapper with its 600-second process watchdog"
        );
        let workload = Workload::parse()?;
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .map_err(anyhow::Error::msg)?;
        runtime.block_on(workload.measure())
    }
}
