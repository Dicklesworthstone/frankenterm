//! Persistent real-socket resize diagnostic for one explicitly owned mux pane.
//!
//! Build via strict RCH with `--features vendored --profile release-perf`.
//! Run under an external process watchdog, with stdout retained as JSONL:
//! `remote_mux_resize_profile SOCKET SERVER_PID PANE_ID TAB_ID CORPUS TRIALS`.
//! The companion `scripts/remote_mux_resize_fixture.py` must own the sole PTY.
//! UnitResponse measures admission; render dimensions plus a nonce-bearing
//! TIOCGWINSZ reply measure client-observed convergence including observer cost.
//! This does not measure native rendering, network transport, or display latency.
//! Set FT_REMOTE_MUX_PROFILE_PHASES=1 only in the instrumentation arm. Phase
//! intervals use CLOCK_MONOTONIC; the sampler must explicitly use the same clock.
//! Instrumented timings must not be mixed into the ordinary latency baseline.

// The nested Cx-aware timeout and transport futures require the same trait
// solver depth as frankenterm-core when Clippy verifies their Send bounds.
#![recursion_limit = "256"]

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
    const SWARM_VIEWPORT_ROWS: usize = 24;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ProfileArm {
        Resize,
        Echo,
        SwarmEcho,
    }

    struct Workload {
        arm: ProfileArm,
        socket: PathBuf,
        server_pid: u32,
        pane: u64,
        tab: u64,
        expected: String,
        ready: String,
        corpus_sha256: String,
        trials: usize,
        profile_phases: bool,
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

    fn nearest_rank_percentile(sorted: &[u128], q: f64) -> u128 {
        let n = sorted.len();
        assert!(n > 0, "cannot compute percentile of empty slice");
        let rank = (q * n as f64).ceil() as usize;
        let idx = rank.clamp(1, n) - 1;
        sorted[idx]
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

    fn probe_text(text: MuxTextReadResult) -> Result<String> {
        // A requested tail deliberately excludes the older history and reports
        // truncated=true. That is valid for nonce/ready-marker observation;
        // the separate full-corpus oracle must still require complete_text.
        match text {
            MuxTextReadResult::Text(text) | MuxTextReadResult::Bounded { text, .. } => Ok(text),
            MuxTextReadResult::OutputTooLarge { .. } => bail!("probe tail exceeds byte cap"),
        }
    }

    fn noise_record_bytes() -> u64 {
        // Includes CRLF on the producer wire, not the rendered row separator.
        b"FT_NOISE 0000000000000000 0123456789abcdef FT_END\r\n".len() as u64
    }

    fn swarm_ready(text: &str, role: &str) -> bool {
        let marker = format!("FT_SWARM_READY {role}");
        text.lines()
            .any(|line| line.trim_end_matches(' ') == marker)
    }

    fn latest_noise_sequence(text: &str) -> Result<u64> {
        let mut latest = None;
        for line in text.lines() {
            let line = line.trim_end_matches(' ');
            let Some(rest) = line.strip_prefix("FT_NOISE ") else {
                continue;
            };
            let Some(digits) = rest.strip_suffix(" 0123456789abcdef FT_END") else {
                continue;
            };
            ensure!(
                digits.len() == 16 && digits.bytes().all(|b| b.is_ascii_digit()),
                "malformed noise counter"
            );
            let sequence = digits.parse::<u64>()?;
            if let Some(previous) = latest {
                ensure!(sequence == previous + 1, "discontinuous noise tail");
            }
            latest = Some(sequence);
        }
        latest.context("no complete parsed noise record in bounded current tail")
    }

    fn noise_ingested_lower_bound(start: u64, end: u64) -> Result<u64> {
        // A visible record's printable suffix does not prove its CRLF was
        // parsed. At baseline, the following record can also be partial.
        // Exclude both uncertain records; at the end credit only records
        // strictly preceding the newest complete printable record.
        end.checked_sub(start)
            .and_then(|delta| delta.checked_sub(2))
            .and_then(|records| records.checked_mul(noise_record_bytes()))
            .context("noise interval lacks fresh complete records or overflows")
    }

    struct IncrementalPaneObserver {
        pane: u64,
        next_row: isize,
        expected_cols: usize,
        expected_viewport_rows: usize,
        accumulated_text: String,
        current_live_line: String,
    }

    impl IncrementalPaneObserver {
        fn new_with_state(
            pane: u64,
            start_row: isize,
            expected_cols: usize,
            expected_viewport_rows: usize,
        ) -> Self {
            Self {
                pane,
                next_row: start_row,
                expected_cols,
                expected_viewport_rows,
                accumulated_text: String::new(),
                current_live_line: String::new(),
            }
        }

        async fn new(client: &mut DirectMuxClient, cx: &Cx, pane: u64) -> Result<Self> {
            let render = client.get_pane_render_changes_with_cx(cx, pane).await?;
            ensure!(render.pane_id as u64 == pane, "pane mismatch");
            ensure!(
                !render.alt_screen_active,
                "unexpected alt_screen_active in echo workload"
            );
            ensure!(
                render.dimensions.cols == 80 && render.dimensions.viewport_rows == 24,
                "unexpected geometry in echo workload: cols={}, rows={}",
                render.dimensions.cols,
                render.dimensions.viewport_rows
            );
            Ok(Self::new_with_state(
                pane,
                render.dimensions.scrollback_top,
                80,
                24,
            ))
        }

        fn ingest_raw_chunk(
            &mut self,
            lines: &[(isize, &str, bool)],
            chunk_end: isize,
            live_cursor_y: isize,
            total_observed: &mut usize,
            max_observed_bytes: usize,
            name: &str,
        ) -> Result<()> {
            ensure!(
                lines.len() == (chunk_end - self.next_row) as usize,
                "row count mismatch: expected {}, got {}",
                chunk_end - self.next_row,
                lines.len()
            );

            for (offset, (row_idx, _, _)) in lines.iter().enumerate() {
                let expected_row = self.next_row + offset as isize;
                ensure!(
                    *row_idx == expected_row,
                    "row index gap detected: expected {}, got {}",
                    expected_row,
                    row_idx
                );
            }

            // Enforce byte cap BEFORE append / allocation
            let mut chunk_bytes = 0usize;
            for (row_idx, text, wrapped) in lines {
                chunk_bytes = chunk_bytes
                    .checked_add(text.len())
                    .context("chunk byte count overflow")?;
                if *row_idx < live_cursor_y && !wrapped {
                    chunk_bytes = chunk_bytes
                        .checked_add(1)
                        .context("chunk byte count overflow")?;
                }
            }

            ensure!(
                total_observed.saturating_add(chunk_bytes) <= max_observed_bytes,
                "observed byte cap exceeded ({} + {} > {}) waiting for {}",
                total_observed,
                chunk_bytes,
                max_observed_bytes,
                name
            );

            *total_observed += chunk_bytes;

            // Clear stale live line if any committed rows are being ingested
            if lines.iter().any(|(row_idx, _, _)| *row_idx < live_cursor_y) {
                self.current_live_line.clear();
            }

            for (row_idx, text, wrapped) in lines {
                if *row_idx < live_cursor_y {
                    self.accumulated_text.push_str(text);
                    if !wrapped {
                        self.accumulated_text.push('\n');
                    }
                } else {
                    self.current_live_line = (*text).to_string();
                }
            }

            self.next_row = chunk_end.min(live_cursor_y);
            Ok(())
        }

        fn search_and_prune<F, T>(&mut self, mut matcher: F) -> Option<T>
        where
            F: FnMut(&str) -> Option<(usize, usize, T)>,
        {
            let mut combined = self.accumulated_text.clone();
            combined.push_str(&self.current_live_line);

            if let Some((_start, end, val)) = matcher(&combined) {
                if end <= self.accumulated_text.len() {
                    let mut safe_end = end;
                    while !self.accumulated_text.is_char_boundary(safe_end) {
                        safe_end += 1;
                    }
                    self.accumulated_text.drain(..safe_end);
                } else {
                    let live_drain = end - self.accumulated_text.len();
                    self.accumulated_text.clear();
                    if live_drain <= self.current_live_line.len() {
                        let mut safe_drain = live_drain;
                        while !self.current_live_line.is_char_boundary(safe_drain) {
                            safe_drain += 1;
                        }
                        self.current_live_line.drain(..safe_drain);
                    } else {
                        self.current_live_line.clear();
                    }
                }
                return Some(val);
            }

            // Suffix retention: search failed, prune prefix if accumulated_text exceeds 64 KiB
            const ACCUMULATOR_MAX: usize = 64 * 1024;
            const RETAINED_SUFFIX: usize = 4096;
            if self.accumulated_text.len() > ACCUMULATOR_MAX {
                let mut drop_len = self.accumulated_text.len() - RETAINED_SUFFIX;
                while drop_len > 0 && !self.accumulated_text.is_char_boundary(drop_len) {
                    drop_len -= 1;
                }
                self.accumulated_text.drain(..drop_len);
            }

            None
        }

        async fn wait_for_marker<F, T>(
            &mut self,
            client: &mut DirectMuxClient,
            cx: &Cx,
            max_observed_bytes: usize,
            timeout: Duration,
            name: &str,
            mut matcher: F,
        ) -> Result<T>
        where
            F: FnMut(&str) -> Option<(usize, usize, T)>,
        {
            if let Some(val) = self.search_and_prune(&mut matcher) {
                return Ok(val);
            }

            let deadline = Instant::now() + timeout;
            let mut total_observed = 0usize;

            while Instant::now() < deadline {
                ensure!(
                    Instant::now() < deadline,
                    "timeout waiting for {name} after {total_observed} bytes observed"
                );
                cx.checkpoint()
                    .map_err(|e| anyhow::anyhow!("context cancelled waiting for {name}: {e}"))?;

                let render = client
                    .get_pane_render_changes_with_cx(cx, self.pane)
                    .await
                    .context("poll render changes during echo observation")?;
                ensure!(
                    Instant::now() < deadline,
                    "timeout waiting for {name} after {total_observed} bytes observed"
                );
                ensure!(
                    render.pane_id as u64 == self.pane,
                    "pane mismatch in render changes"
                );
                ensure!(
                    !render.alt_screen_active,
                    "unexpected alt_screen_active in echo workload"
                );
                ensure!(
                    render.dimensions.cols == self.expected_cols
                        && render.dimensions.viewport_rows == self.expected_viewport_rows,
                    "unexpected resize in echo workload: cols={}, rows={}",
                    render.dimensions.cols,
                    render.dimensions.viewport_rows
                );

                let scrollback_top = render.dimensions.scrollback_top;
                let live_cursor_y = render.cursor_position.y;

                // Strict retention overrun check: fail closed if next_row was dropped
                ensure!(
                    self.next_row >= scrollback_top,
                    "retention overrun: observer next_row {} is behind scrollback_top {}",
                    self.next_row,
                    scrollback_top
                );

                ensure!(
                    live_cursor_y >= self.next_row,
                    "cursor moved backwards: live_cursor_y {} < next_row {}",
                    live_cursor_y,
                    self.next_row
                );

                let pass_end = live_cursor_y
                    .checked_add(1)
                    .context("cursor row overflow")?;
                if self.next_row >= pass_end {
                    sleep_with_cx(cx, Duration::from_millis(1))
                        .await
                        .map_err(anyhow::Error::msg)?;
                    continue;
                }

                const CHUNK_CAP: isize = 128;
                while self.next_row < pass_end {
                    ensure!(
                        Instant::now() < deadline,
                        "timeout waiting for {name} after {total_observed} bytes observed"
                    );
                    cx.checkpoint().map_err(|e| {
                        anyhow::anyhow!("context cancelled waiting for {name}: {e}")
                    })?;

                    let chunk_end = self.next_row.saturating_add(CHUNK_CAP).min(pass_end);
                    let reached_pass_end = chunk_end == pass_end;

                    let resp = client
                        .get_lines_with_cx(
                            cx,
                            self.pane,
                            std::iter::once(self.next_row..chunk_end).collect(),
                        )
                        .await
                        .context("read text rows during echo observation")?;
                    ensure!(
                        Instant::now() < deadline,
                        "timeout waiting for {name} after {total_observed} bytes observed"
                    );
                    ensure!(
                        resp.pane_id as u64 == self.pane,
                        "pane mismatch in get_lines response"
                    );
                    let (lines, _) = resp.lines.extract_data();

                    let cows: Vec<(isize, std::borrow::Cow<'_, str>, bool)> = lines
                        .iter()
                        .map(|(r, l)| (*r, l.as_str(), l.last_cell_was_wrapped()))
                        .collect();
                    let chunk_tuples: Vec<(isize, &str, bool)> =
                        cows.iter().map(|(r, c, w)| (*r, c.as_ref(), *w)).collect();

                    self.ingest_raw_chunk(
                        &chunk_tuples,
                        chunk_end,
                        live_cursor_y,
                        &mut total_observed,
                        max_observed_bytes,
                        name,
                    )?;

                    // Search after EACH bounded chunk
                    if let Some(val) = self.search_and_prune(&mut matcher) {
                        ensure!(
                            Instant::now() < deadline,
                            "timeout waiting for {name} after {total_observed} bytes observed"
                        );
                        return Ok(val);
                    }

                    if reached_pass_end {
                        break;
                    }
                }

                sleep_with_cx(cx, Duration::from_millis(1))
                    .await
                    .map_err(anyhow::Error::msg)?;
            }

            bail!("timeout waiting for {name} after {total_observed} bytes observed");
        }

        async fn wait_for_ready(
            &mut self,
            client: &mut DirectMuxClient,
            cx: &Cx,
            timeout: Duration,
        ) -> Result<()> {
            let target = "FT_ECHO_READY";
            self.wait_for_marker(client, cx, 1024 * 1024, timeout, target, |text| {
                let pos = text.find(target)?;
                let match_end = pos + target.len();
                let rest = &text[match_end..];
                let full_end = if rest.starts_with("\r\n") {
                    match_end + 2
                } else if rest.starts_with('\n') || rest.starts_with('\r') {
                    match_end + 1
                } else {
                    match_end
                };
                Some((pos, full_end, ()))
            })
            .await
        }

        async fn wait_for_start_marker(
            &mut self,
            client: &mut DirectMuxClient,
            cx: &Cx,
            nonce: &str,
            timeout: Duration,
        ) -> Result<u64> {
            let prefix = format!("FT_STREAM_START {nonce} bytes=");
            let suffix = " FT_END";
            self.wait_for_marker(
                client,
                cx,
                4 * 1024 * 1024,
                timeout,
                "FT_STREAM_START",
                |text| {
                    let pos = text.find(&prefix)?;
                    let rest = &text[pos + prefix.len()..];
                    let (num_str, _) = rest.split_once(suffix)?;
                    if !num_str.is_empty() && num_str.chars().all(|c| c.is_ascii_digit()) {
                        let b = num_str.parse::<u64>().ok()?;
                        let match_end = pos + prefix.len() + num_str.len() + suffix.len();
                        let rest_after = &text[match_end..];
                        let full_end = if rest_after.starts_with("\r\n") {
                            match_end + 2
                        } else if rest_after.starts_with('\n') || rest_after.starts_with('\r') {
                            match_end + 1
                        } else {
                            match_end
                        };
                        Some((pos, full_end, b))
                    } else {
                        None
                    }
                },
            )
            .await
        }

        async fn wait_for_probe(
            &mut self,
            client: &mut DirectMuxClient,
            cx: &Cx,
            nonce: &str,
            timeout: Duration,
        ) -> Result<()> {
            let target = format!("FT_PROBE {nonce}");
            self.wait_for_marker(client, cx, 16 * 1024 * 1024, timeout, &target, |text| {
                let pos = text.find(&target)?;
                let match_end = pos + target.len();
                let rest = &text[match_end..];
                if rest.starts_with('\r') || rest.starts_with('\n') || rest.is_empty() {
                    let full_end = if rest.starts_with("\r\n") {
                        match_end + 2
                    } else if rest.starts_with('\n') || rest.starts_with('\r') {
                        match_end + 1
                    } else {
                        match_end
                    };
                    Some((pos, full_end, ()))
                } else {
                    None
                }
            })
            .await
        }

        async fn wait_for_end_marker(
            &mut self,
            client: &mut DirectMuxClient,
            cx: &Cx,
            nonce: &str,
            timeout: Duration,
        ) -> Result<u64> {
            let prefix = format!("FT_STREAM_END {nonce} bytes=");
            let suffix = " FT_END";
            self.wait_for_marker(
                client,
                cx,
                64 * 1024 * 1024,
                timeout,
                "FT_STREAM_END",
                |text| {
                    let pos = text.find(&prefix)?;
                    let rest = &text[pos + prefix.len()..];
                    let (num_str, _) = rest.split_once(suffix)?;
                    if !num_str.is_empty() && num_str.chars().all(|c| c.is_ascii_digit()) {
                        let b = num_str.parse::<u64>().ok()?;
                        let match_end = pos + prefix.len() + num_str.len() + suffix.len();
                        let rest_after = &text[match_end..];
                        let full_end = if rest_after.starts_with("\r\n") {
                            match_end + 2
                        } else if rest_after.starts_with('\n') || rest_after.starts_with('\r') {
                            match_end + 1
                        } else {
                            match_end
                        };
                        Some((pos, full_end, b))
                    } else {
                        None
                    }
                },
            )
            .await
        }

        async fn wait_for_exit(
            &mut self,
            client: &mut DirectMuxClient,
            cx: &Cx,
            nonce: &str,
            timeout: Duration,
        ) -> Result<()> {
            let target = format!("FT_EXIT {nonce}");
            self.wait_for_marker(client, cx, 4 * 1024 * 1024, timeout, &target, |text| {
                let pos = text.find(&target)?;
                let match_end = pos + target.len();
                let rest = &text[match_end..];
                let full_end = if rest.starts_with("\r\n") {
                    match_end + 2
                } else if rest.starts_with('\n') || rest.starts_with('\r') {
                    match_end + 1
                } else {
                    match_end
                };
                Some((pos, full_end, ()))
            })
            .await
        }
    }

    impl Workload {
        fn parse() -> Result<Self> {
            let args: Vec<_> = std::env::args_os().skip(1).collect();
            ensure!(
                args.len() == 6 || args.len() == 7,
                "expected SOCKET SERVER_PID PANE_ID TAB_ID CORPUS TRIALS [ARM]"
            );
            let arm = if args.len() == 7 {
                let s = args[6].to_str().context("non-UTF8 arm argument")?;
                match s {
                    "resize" => ProfileArm::Resize,
                    "echo" => ProfileArm::Echo,
                    "swarm-echo" => ProfileArm::SwarmEcho,
                    other => bail!(
                        "unrecognized profile arm: {other}; expected resize, echo or swarm-echo"
                    ),
                }
            } else {
                ProfileArm::Resize
            };
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
            let trials = usize::try_from(number(5)?)?;
            let expected;
            let ready;
            let corpus_sha256;
            if arm == ProfileArm::Resize {
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
                        line.starts_with(&format!("FT_RECORD_{index:05} "))
                            && line.ends_with(&format!("FT_END_{index:05}")),
                        "invalid record {index}"
                    );
                }
                corpus_sha256 = hex::encode(Sha256::digest(text.as_bytes()));
                expected = physical_join(&text);
                ready = format!("FT_CORPUS_READY {corpus_sha256}");
                ensure!((1..=100).contains(&trials), "trials must be 1..100");
            } else {
                corpus_sha256 = String::new();
                expected = String::new();
                ready = "FT_ECHO_READY".to_string();
                ensure!(
                    trials == 1000,
                    "echo arm requires 1000 trials, got {trials}"
                );
            }
            Ok(Self {
                arm,
                socket,
                server_pid,
                pane: number(2)?,
                tab: number(3)?,
                expected,
                ready,
                corpus_sha256,
                trials,
                profile_phases: std::env::var("FT_REMOTE_MUX_PROFILE_PHASES").as_deref() == Ok("1"),
            })
        }

        fn phase_time(&self) -> Result<Option<u128>> {
            if !self.profile_phases {
                return Ok(None);
            }
            let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
            let seconds = u128::try_from(now.tv_sec).context("negative monotonic seconds")?;
            let nanos = u128::try_from(now.tv_nsec).context("negative monotonic nanoseconds")?;
            ensure!(nanos < 1_000_000_000, "invalid monotonic nanoseconds");
            Ok(Some(seconds * 1_000_000_000 + nanos))
        }

        fn phase(
            nonce: &str,
            cols: usize,
            name: &str,
            start: Option<u128>,
            end: Option<u128>,
        ) -> Result<()> {
            if let Some((start, end)) = start.zip(end) {
                ensure!(end >= start, "profiling clock regressed");
                emit(json!({"event":"phase", "nonce":nonce, "columns":cols,
                    "phase":name, "clock":"CLOCK_MONOTONIC", "start_ns":start,
                    "end_ns":end, "instrumented":true}))?;
            }
            Ok(())
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
            if records != self.expected {
                let first_difference = records
                    .bytes()
                    .zip(self.expected.bytes())
                    .take_while(|(actual, expected)| actual == expected)
                    .count();
                let start = first_difference.saturating_sub(32);
                let actual_end = first_difference.saturating_add(96).min(records.len());
                let expected_end = first_difference.saturating_add(96).min(self.expected.len());
                bail!(
                    "exact ordered Unicode/whitespace corpus differs: first_byte={first_difference} \
                     expected_bytes={} actual_bytes={} expected_context={:?} actual_context={:?}",
                    self.expected.len(),
                    records.len(),
                    String::from_utf8_lossy(&self.expected.as_bytes()[start..expected_end]),
                    String::from_utf8_lossy(&records.as_bytes()[start..actual_end]),
                );
            }
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
            let request_start = self.phase_time()?;
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
            let admission_end = self.phase_time()?;
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
                ensure!(
                    started.elapsed() < SETTLE,
                    "terminal geometry response arrived after convergence deadline"
                );
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
            let terminal_end = self.phase_time()?;
            client
                .write_to_pane_with_cx(cx, self.pane, format!("PROBE {nonce}\n").into_bytes())
                .await?;
            let expected_probe = format!("FT_PROBE {nonce} 24 {cols}");
            loop {
                ensure!(
                    started.elapsed() < SETTLE,
                    "PTY echo convergence deadline expired"
                );
                let text = probe_text(
                    client
                        .get_text_tail_with_cx(cx, self.pane, 65536, Some(20))
                        .await?,
                )?;
                polls += 1;
                ensure!(
                    started.elapsed() < SETTLE,
                    "PTY echo response arrived after convergence deadline"
                );
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
            let probe_end = self.phase_time()?;
            // Full history correctness is deliberately outside the timed interval.
            self.oracle(client, cx).await?;
            let oracle_end = self.phase_time()?;
            // Emit after measurement so stdout serialization cannot enter a
            // recorded resize interval. Missing intervals after a failure are
            // intentionally unusable for phase-filtered CPU attribution.
            Self::phase(
                nonce,
                cols,
                "resize_admission",
                request_start,
                admission_end,
            )?;
            Self::phase(
                nonce,
                cols,
                "terminal_convergence",
                admission_end,
                terminal_end,
            )?;
            Self::phase(nonce, cols, "pty_probe", terminal_end, probe_end)?;
            Self::phase(nonce, cols, "full_history_oracle", probe_end, oracle_end)?;
            Ok(json!({"status":"passed", "columns":cols, "rows":24,
                "instrumented":self.profile_phases,
                "from_columns":before.dimensions.cols, "admission_us":admission_us,
                "terminal_observed_us":terminal_us, "pty_echo_convergence_us":convergence_us,
                "observer_polls":polls, "sequence_before":before.seqno,
                "sequence_at_terminal_geometry":previous_seqno,
                "exact_corpus_preserved":true}))
        }

        async fn measure(&self) -> Result<()> {
            match self.arm {
                ProfileArm::Resize => self.measure_resize().await,
                ProfileArm::Echo => self.measure_echo().await,
                ProfileArm::SwarmEcho => self.measure_swarm_echo().await,
            }
        }

        async fn measure_swarm_echo(&self) -> Result<()> {
            self.verify_lease()?;
            let cx = Cx::for_request();
            let mut client = DirectMuxClient::connect_with_cx(
                &cx,
                DirectMuxClientConfig {
                    socket_path: Some(self.socket.clone()),
                    connect_timeout: SETTLE,
                    read_timeout: SETTLE,
                    write_timeout: SETTLE,
                    ..DirectMuxClientConfig::default()
                },
            )
            .await?;
            let topology = client.list_panes_with_cx(&cx).await?;
            ensure!(
                topology.floating_panes.is_empty(),
                "unexpected floating panes"
            );
            if std::env::var("FT_SWARM_SETUP_ONLY").as_deref() == Ok("1") {
                ensure!(
                    topology.tabs.len() == 1,
                    "setup requires only the owned interactive pane"
                );
                match &topology.tabs[0] {
                    mux::tab::PaneNode::Leaf(pane) => ensure!(
                        pane.pane_id as u64 == self.pane && pane.tab_id as u64 == self.tab,
                        "setup pane identity mismatch"
                    ),
                    _ => bail!("unexpected split pane"),
                }
                for _ in 0..4 {
                    // The private fixture config supplies an exact noise-only
                    // default_prog. No shell or production default is inherited.
                    let spawned = client
                        .spawn_v2_with_cx(
                            &cx,
                            codec::SpawnV2 {
                                domain: config::keyassignment::SpawnTabDomain::DefaultDomain,
                                window_id: None,
                                command: None,
                                command_dir: None,
                                size: TerminalSize {
                                    rows: SWARM_VIEWPORT_ROWS,
                                    cols: 80,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                    dpi: 0,
                                },
                                workspace: "owned-swarm-profile".to_string(),
                            },
                        )
                        .await?;
                    emit(json!({"event":"swarm_spawn", "pane_id":spawned.pane_id,
                        "tab_id":spawned.tab_id}))?;
                }
                return Ok(());
            }
            ensure!(
                topology.tabs.len() == 5,
                "swarm requires five owned unsplit tabs"
            );
            let mut noise = Vec::new();
            let mut found_interactive = false;
            for tab in &topology.tabs {
                let mux::tab::PaneNode::Leaf(pane) = tab else {
                    bail!("unexpected split pane")
                };
                if pane.pane_id as u64 == self.pane {
                    ensure!(
                        pane.tab_id as u64 == self.tab && !found_interactive,
                        "interactive identity mismatch"
                    );
                    found_interactive = true;
                } else {
                    noise.push(pane.pane_id as u64);
                }
            }
            noise.sort_unstable();
            ensure!(
                found_interactive && noise.len() == 4 && noise.windows(2).all(|w| w[0] != w[1]),
                "swarm pane identity mismatch"
            );
            for pane in std::iter::once(&self.pane).chain(noise.iter()) {
                let role = if *pane == self.pane {
                    "interactive"
                } else {
                    "noise"
                };
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    let text = probe_text(
                        client
                            // Startup emits READY at row zero, while the rest
                            // of the allocated viewport is still blank. A
                            // shorter tail excludes that first-row marker.
                            .get_text_tail_with_cx(&cx, *pane, 4096, Some(SWARM_VIEWPORT_ROWS))
                            .await?,
                    )?;
                    ensure!(
                        Instant::now() < deadline,
                        "swarm readiness deadline expired"
                    );
                    if swarm_ready(&text, role) {
                        break;
                    }
                    sleep_with_cx(&cx, Duration::from_millis(10))
                        .await
                        .map_err(anyhow::Error::msg)?;
                }
            }
            emit(json!({"event":"contract", "arm":"swarm-echo", "version":1,
                "trials":1000, "noise_panes":noise, "pane_id":self.pane, "tab_id":self.tab,
                "server_pid":self.server_pid, "min_throughput_mb_s":10.0,
                "p99_budget_us":50000, "record_bytes":noise_record_bytes(),
                "scope":"private Unix socket; quiet-pane echo during aggregate noise ingestion; excludes native display"}))?;
            for pane in &noise {
                client
                    .write_to_pane_with_cx(&cx, *pane, b"START_STREAM swarm_session_001\n".to_vec())
                    .await?;
            }
            let mut observer = IncrementalPaneObserver::new(&mut client, &cx, self.pane).await?;
            let interval = Instant::now();
            let mut latencies = Vec::with_capacity(1000);
            let mut first_sequences: Option<Vec<u64>> = None;
            let mut previous_sequences: Option<Vec<u64>> = None;
            for trial in 0..1000 {
                let nonce = format!("swarm_trial_{trial:04}");
                let started = Instant::now();
                let send_start_us = interval.elapsed().as_micros();
                client
                    .write_to_pane_with_cx(&cx, self.pane, format!("PROBE {nonce}\n").into_bytes())
                    .await?;
                observer
                    .wait_for_probe(&mut client, &cx, &nonce, SETTLE)
                    .await?;
                let observed_us = interval.elapsed().as_micros();
                let latency = observed_us - send_start_us;
                ensure!(
                    started.elapsed() < SETTLE,
                    "swarm probe exceeded five-second deadline"
                );
                latencies.push(latency);
                emit(
                    json!({"event":"swarm_echo_trial", "trial":trial, "nonce":nonce,
                    "send_start_us":send_start_us, "observed_us":observed_us,
                    "latency_us":latency}),
                )?;
                // Every sample lies strictly inside the first-send/last-echo
                // interval. No post-trial drain contributes to throughput.
                if trial % 10 == 0 {
                    let mut sequences = Vec::with_capacity(4);
                    for (index, pane) in noise.iter().enumerate() {
                        let request_start_us = interval.elapsed().as_micros();
                        let text = probe_text(
                            client
                                .get_text_tail_with_cx(&cx, *pane, 4096, Some(20))
                                .await?,
                        )?;
                        let sequence = latest_noise_sequence(&text)?;
                        if let Some(previous) = &previous_sequences {
                            ensure!(
                                sequence > previous[index],
                                "noise ingestion stalled or regressed"
                            );
                        }
                        emit(
                            json!({"event":"noise_sample", "after_trial":trial, "pane_id":pane,
                            "request_start_us":request_start_us, "response_end_us":interval.elapsed().as_micros(),
                            "last_complete_record":sequence, "tail":text}),
                        )?;
                        sequences.push(sequence);
                    }
                    if first_sequences.is_none() {
                        first_sequences = Some(sequences.clone());
                    }
                    previous_sequences = Some(sequences);
                }
            }
            let duration_us = interval.elapsed().as_micros();
            let first = first_sequences.context("missing first noise observation")?;
            let last = previous_sequences.context("missing final noise observation")?;
            let mut ingested = 0u64;
            for (start, end) in first.iter().zip(&last) {
                ingested = ingested
                    .checked_add(noise_ingested_lower_bound(*start, *end)?)
                    .context("aggregate byte overflow")?;
            }
            latencies.sort_unstable();
            let p99 = nearest_rank_percentile(&latencies, 0.99);
            let throughput = ingested as f64 * 1_000_000.0 / duration_us as f64 / (1024.0 * 1024.0);
            ensure!(
                ingested >= 10 * 1024 * 1024 && throughput >= 10.0,
                "concurrent aggregate ingestion below 10 MiB/s: {throughput}"
            );
            ensure!(p99 <= 50_000, "swarm p99 {p99}us exceeds 50ms");
            for pane in noise.iter().chain(std::iter::once(&self.pane)) {
                client
                    .write_to_pane_with_cx(&cx, *pane, b"EXIT swarm_exit\n".to_vec())
                    .await?;
            }
            emit(
                json!({"event":"complete", "arm":"swarm-echo", "status":"passed",
                "trials":1000, "duration_us":duration_us, "bytes_ingested_lower_bound":ingested,
                "throughput_mb_s_lower_bound":throughput, "p99_echo_us":p99}),
            )?;
            Ok(())
        }

        async fn measure_echo(&self) -> Result<()> {
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
            let mut observer = IncrementalPaneObserver::new(&mut client, &cx, self.pane).await?;
            observer
                .wait_for_ready(&mut client, &cx, Duration::from_secs(30))
                .await
                .context("echo fixture startup failed while waiting for FT_ECHO_READY")?;
            emit(json!({
                "event": "contract",
                "arm": "echo",
                "version": 1,
                "trials": self.trials,
                "min_throughput_mb_s": 10.0,
                "p99_budget_us": 50_000,
                "scope": "remote-host-private-Unix-socket; live SendText to PTY echo under sustained output",
                "socket": self.socket,
                "server_pid": self.server_pid,
                "pane_id": self.pane,
                "tab_id": self.tab,
            }))?;

            // Signal start of sustained stream
            let stream_nonce = "stream_session_001";
            let stream_start = Instant::now();
            client
                .write_to_pane_with_cx(
                    &cx,
                    self.pane,
                    format!("START_STREAM {stream_nonce}\n").into_bytes(),
                )
                .await?;

            // Await ordered start marker
            let start_bytes = observer
                .wait_for_start_marker(&mut client, &cx, stream_nonce, Duration::from_secs(10))
                .await
                .context("stream start marker observation failed")?;

            let mut latencies_us = Vec::with_capacity(self.trials);
            for trial in 0..self.trials {
                let nonce = format!("echo_trial_{trial:04}");
                let send_start = Instant::now();
                client
                    .write_to_pane_with_cx(&cx, self.pane, format!("PROBE {nonce}\n").into_bytes())
                    .await?;
                observer
                    .wait_for_probe(&mut client, &cx, &nonce, Duration::from_secs(5))
                    .await
                    .with_context(|| {
                        format!("keystroke echo observation failed for trial {trial} ({nonce})")
                    })?;
                let latency_us = send_start.elapsed().as_micros();
                latencies_us.push(latency_us);
                emit(json!({
                    "event": "echo_trial",
                    "trial": trial,
                    "nonce": nonce,
                    "latency_us": latency_us,
                }))?;
            }

            // Finish stream and wait for terminal ingestion marker proving full byte absorption
            client
                .write_to_pane_with_cx(
                    &cx,
                    self.pane,
                    format!("STREAM_FINISH {stream_nonce}\n").into_bytes(),
                )
                .await?;
            let end_bytes = observer
                .wait_for_end_marker(&mut client, &cx, stream_nonce, Duration::from_secs(30))
                .await
                .context("terminal stream marker observation failed; ingestion is unproven")?;
            let stream_duration = stream_start.elapsed();
            let duration_secs = stream_duration.as_secs_f64();
            ensure!(
                end_bytes >= start_bytes,
                "stream end bytes less than start bytes"
            );
            let bytes_ingested = end_bytes - start_bytes;
            let throughput_bytes_per_sec = (bytes_ingested as f64) / duration_secs;
            let throughput_mb_s = throughput_bytes_per_sec / (1024.0 * 1024.0);

            ensure!(
                bytes_ingested >= 10 * 1024 * 1024,
                "total bytes ingested {} below minimum required 10 MiB",
                bytes_ingested
            );
            ensure!(
                throughput_mb_s >= 10.0,
                "observed ingestion throughput {:.2} MB/s below 10.0 MB/s requirement",
                throughput_mb_s
            );

            latencies_us.sort_unstable();
            let n = latencies_us.len();
            ensure!(n == self.trials, "trial count mismatch");
            let p50_us = nearest_rank_percentile(&latencies_us, 0.50);
            let p90_us = nearest_rank_percentile(&latencies_us, 0.90);
            let p95_us = nearest_rank_percentile(&latencies_us, 0.95);
            let p99_us = nearest_rank_percentile(&latencies_us, 0.99);
            const P99_BUDGET_US: u128 = 50_000;
            ensure!(
                p99_us <= P99_BUDGET_US,
                "p99 keystroke echo latency {}us exceeds 50ms budget",
                p99_us
            );

            let exit_nonce = "echo_exit";
            client
                .write_to_pane_with_cx(&cx, self.pane, format!("EXIT {exit_nonce}\n").into_bytes())
                .await?;
            observer
                .wait_for_exit(&mut client, &cx, exit_nonce, Duration::from_secs(5))
                .await
                .context("exit receipt observation failed; FT_EXIT was not observed")?;

            emit(json!({
                "event": "complete",
                "status": "passed",
                "arm": "echo",
                "trials": n,
                "start_bytes": start_bytes,
                "end_bytes": end_bytes,
                "bytes_ingested": bytes_ingested,
                "duration_seconds": duration_secs,
                "throughput_bytes_per_sec": throughput_bytes_per_sec,
                "throughput_mb_s": throughput_mb_s,
                "p50_echo_us": p50_us,
                "p90_echo_us": p90_us,
                "p95_echo_us": p95_us,
                "p99_echo_us": p99_us,
                "p99_budget_us": P99_BUDGET_US,
            }))?;

            Ok(())
        }

        async fn measure_resize(&self) -> Result<()> {
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
                let tail = probe_text(
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
                "instrumented":self.profile_phases,
                "phase_clock":if self.profile_phases {Some("CLOCK_MONOTONIC")} else {None},
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn swarm_readiness_includes_first_row_of_short_terminal_content() {
            #[derive(Debug)]
            struct ReadyConfig;
            impl frankenterm_term::TerminalConfiguration for ReadyConfig {
                fn color_palette(&self) -> frankenterm_term::color::ColorPalette {
                    frankenterm_term::color::ColorPalette::default()
                }
            }
            for role in ["interactive", "noise"] {
                let mut terminal = frankenterm_term::Terminal::new(
                    TerminalSize {
                        rows: SWARM_VIEWPORT_ROWS,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                        dpi: 0,
                    },
                    std::sync::Arc::new(ReadyConfig),
                    "FrankenTerm",
                    "swarm-ready-regression",
                    Box::new(Vec::new()),
                );
                terminal.advance_bytes(format!("FT_SWARM_READY {role}\r\n"));
                let rows = terminal.screen().scrollback_rows();
                assert_eq!(rows, SWARM_VIEWPORT_ROWS);
                let read_tail = |count| {
                    terminal
                        .screen()
                        .lines_in_phys_range(rows - count..rows)
                        .iter()
                        .map(|line| line.as_str().into_owned())
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let old_tail = read_tail(20);
                assert!(old_tail.trim().is_empty());
                assert!(!swarm_ready(&old_tail, role));
                let full_viewport = read_tail(SWARM_VIEWPORT_ROWS);
                assert!(full_viewport.len() <= 4096);
                assert!(swarm_ready(&full_viewport, role));
                assert!(!swarm_ready(&full_viewport, "wrong-role"));
            }
        }

        #[test]
        fn swarm_parsed_counter_excludes_partial_and_uncertain_records() {
            let text = "FT_NOISE 0000000000000040 0123456789abcdef FT_END\nFT_NOISE 0000000000000041 0123456789abcdef FT_END\nFT_NOISE 0000000000000042 01234";
            assert_eq!(latest_noise_sequence(text).unwrap(), 41);
            assert_eq!(latest_noise_sequence("FT_NOISE 0000000000000041 0123456789abcdef FT_END                               \n").unwrap(), 41);
            assert_eq!(
                noise_ingested_lower_bound(10, 41).unwrap(),
                29 * noise_record_bytes()
            );
            assert!(noise_ingested_lower_bound(41, 40).is_err());
            assert!(noise_ingested_lower_bound(40, 41).is_err());
            assert!(noise_ingested_lower_bound(0, u64::MAX).is_err());
            assert!(latest_noise_sequence("FT_NOISE 0000000000000042 01234").is_err());
            assert!(latest_noise_sequence("FT_NOISE 0000000000000040 0123456789abcdef FT_END\nFT_NOISE 0000000000000042 0123456789abcdef FT_END").is_err());
            assert!(
                latest_noise_sequence("FT_NOISE 00000000000000x0 0123456789abcdef FT_END").is_err()
            );
        }

        #[test]
        fn test_observer_pre_search_buffered() {
            let mut observer = IncrementalPaneObserver::new_with_state(1, 0, 80, 24);
            observer.accumulated_text = "FT_ECHO_READY\r\n".to_string();

            let res = observer.search_and_prune(|text| {
                let pos = text.find("FT_ECHO_READY")?;
                Some((pos, pos + "FT_ECHO_READY\r\n".len(), ()))
            });
            assert!(res.is_some());
            assert!(observer.accumulated_text.is_empty());
        }

        #[test]
        fn test_observer_chunk_by_chunk_search() {
            let mut observer = IncrementalPaneObserver::new_with_state(1, 0, 80, 24);
            let mut total_observed = 0;
            let lines = vec![
                (0, "FT_STR_00000000 payload", false),
                (1, "FT_PROBE echo_trial_0000", false),
            ];
            // Chunk 0..2 where live_cursor_y is 2
            let res = observer.ingest_raw_chunk(&lines, 2, 2, &mut total_observed, 1024, "probe");
            assert!(res.is_ok());
            assert_eq!(observer.next_row, 2);

            let matched = observer.search_and_prune(|text| {
                let pos = text.find("FT_PROBE echo_trial_0000")?;
                Some((pos, pos + "FT_PROBE echo_trial_0000\n".len(), 42))
            });
            assert_eq!(matched, Some(42));
        }

        #[test]
        fn test_observer_split_marker_across_chunks() {
            let mut observer = IncrementalPaneObserver::new_with_state(1, 0, 80, 24);
            let mut total_observed = 0;

            // Chunk 1: line ends with prefix of probe (e.g. wrapped or split record)
            let chunk1 = vec![(0, "FT_PROBE echo_tr", true)];
            observer
                .ingest_raw_chunk(&chunk1, 1, 2, &mut total_observed, 1024, "probe")
                .unwrap();
            assert_eq!(observer.next_row, 1);

            // Search after chunk 1 fails to find full probe
            let target = "FT_PROBE echo_trial_0042";
            let matched = observer.search_and_prune(|text| {
                let pos = text.find(target)?;
                Some((pos, pos + target.len(), ()))
            });
            assert!(matched.is_none());

            // Chunk 2: line starts with suffix of probe
            let chunk2 = vec![(1, "ial_0042\r\n", false)];
            observer
                .ingest_raw_chunk(&chunk2, 2, 2, &mut total_observed, 1024, "probe")
                .unwrap();
            assert_eq!(observer.next_row, 2);

            // Search after chunk 2 succeeds across the chunk seam!
            let matched2 = observer.search_and_prune(|text| {
                let pos = text.find(target)?;
                Some((pos, pos + target.len(), ()))
            });
            assert!(matched2.is_some());
        }

        #[test]
        fn test_observer_byte_cap_enforced_before_append() {
            let mut observer = IncrementalPaneObserver::new_with_state(1, 0, 80, 24);
            let mut total_observed = 80;
            let max_cap = 100;

            // Chunk has 30 bytes + 1 newline = 31 bytes, which exceeds 100 - 80 = 20 bytes headroom
            let chunk = vec![(0, "123456789012345678901234567890", false)];
            let res =
                observer.ingest_raw_chunk(&chunk, 1, 1, &mut total_observed, max_cap, "test_cap");

            // Must fail closed with byte cap error
            assert!(res.is_err());
            let err_str = res.unwrap_err().to_string();
            assert!(err_str.contains("observed byte cap exceeded"));

            // Must NOT have appended any data or advanced next_row
            assert!(observer.accumulated_text.is_empty());
            assert_eq!(observer.next_row, 0);
            assert_eq!(total_observed, 80);
        }

        #[test]
        fn test_observer_suffix_retention_preserves_split_marker() {
            let mut observer = IncrementalPaneObserver::new_with_state(1, 0, 80, 24);

            // Fill accumulator to 65 KiB (exceeds ACCUMULATOR_MAX = 64 KiB)
            let padding = "A".repeat(65 * 1024);
            observer.accumulated_text = padding;
            // Append start of marker at the very tail of accumulator
            observer.accumulated_text.push_str("FT_STREAM_START s");

            // Search fails, triggering prefix prune to RETAINED_SUFFIX (4096 bytes)
            let res = observer.search_and_prune::<_, ()>(|_| None);
            assert!(res.is_none());
            assert_eq!(observer.accumulated_text.len(), 4096);
            assert!(observer.accumulated_text.ends_with("FT_STREAM_START s"));

            // Ingest next chunk with remainder of marker
            let mut total_observed = 0;
            let chunk = vec![(0, "tream_session_001 bytes=500 FT_END\r\n", false)];
            observer
                .ingest_raw_chunk(&chunk, 1, 1, &mut total_observed, 1024 * 1024, "stream")
                .unwrap();

            // Search finds the split marker across the prune boundary!
            let expected_prefix = "FT_STREAM_START stream_session_001 bytes=";
            let found = observer.search_and_prune(|text| {
                let pos = text.find(expected_prefix)?;
                Some((pos, pos + expected_prefix.len(), ()))
            });
            assert!(found.is_some());
        }

        #[test]
        fn test_observer_gap_detection_refuses_discontinuous_rows() {
            let mut observer = IncrementalPaneObserver::new_with_state(1, 0, 80, 24);
            let mut total_observed = 0;
            // Row index 0, then 2 (gap: row 1 missing)
            let lines = vec![(0, "row 0", false), (2, "row 2", false)];
            let res =
                observer.ingest_raw_chunk(&lines, 2, 2, &mut total_observed, 1024, "gap_test");
            assert!(res.is_err());
            assert!(
                res.unwrap_err()
                    .to_string()
                    .contains("row index gap detected")
            );
        }

        #[test]
        fn test_observer_row_count_mismatch_refused() {
            let mut observer = IncrementalPaneObserver::new_with_state(1, 0, 80, 24);
            let mut total_observed = 0;
            // Chunk range 0..3 expects 3 lines, but only 2 provided
            let lines = vec![(0, "row 0", false), (1, "row 1", false)];
            let res =
                observer.ingest_raw_chunk(&lines, 3, 3, &mut total_observed, 1024, "count_test");
            assert!(res.is_err());
            assert!(res.unwrap_err().to_string().contains("row count mismatch"));
        }

        #[test]
        fn test_observer_live_row_committed_during_multichunk_catchup() {
            let mut observer = IncrementalPaneObserver::new_with_state(1, 0, 80, 24);
            let mut total_observed = 0;

            // Step 1: Initial observation at cursor row 0 (live row)
            let chunk0 = vec![(0, "LIVE_PREFIX", false)];
            observer
                .ingest_raw_chunk(&chunk0, 1, 0, &mut total_observed, 1024, "probe")
                .unwrap();
            assert_eq!(observer.accumulated_text, "");
            assert_eq!(observer.current_live_line, "LIVE_PREFIX");
            assert_eq!(observer.next_row, 0);

            // Step 2: Cursor advances to row 3. Catch-up chunk 1 only fetches rows 0..2 (committed rows < 3)
            // It does NOT reach live_cursor_y (3).
            // Stale current_live_line ("LIVE_PREFIX") must be cleared so it does not duplicate when combined!
            let chunk1 = vec![
                (0, "LIVE_PREFIX_COMPLETED", false),
                (1, "ROW_1_RECORD", false),
            ];
            observer
                .ingest_raw_chunk(&chunk1, 2, 3, &mut total_observed, 1024, "probe")
                .unwrap();
            assert_eq!(
                observer.accumulated_text,
                "LIVE_PREFIX_COMPLETED\nROW_1_RECORD\n"
            );
            assert_eq!(observer.current_live_line, "");
            assert_eq!(observer.next_row, 2);

            // Verify search after chunk 1 does not duplicate LIVE_PREFIX
            let count_live_prefix = {
                let mut combined = observer.accumulated_text.clone();
                combined.push_str(&observer.current_live_line);
                combined.matches("LIVE_PREFIX").count()
            };
            assert_eq!(count_live_prefix, 1);

            // Step 3: Catch-up chunk 2 fetches rows 2..4 (row 2 committed, row 3 live)
            let chunk2 = vec![(2, "ROW_2_RECORD", false), (3, "NEW_LIVE_ROW_3", false)];
            observer
                .ingest_raw_chunk(&chunk2, 4, 3, &mut total_observed, 1024, "probe")
                .unwrap();
            assert_eq!(
                observer.accumulated_text,
                "LIVE_PREFIX_COMPLETED\nROW_1_RECORD\nROW_2_RECORD\n"
            );
            assert_eq!(observer.current_live_line, "NEW_LIVE_ROW_3");
            assert_eq!(observer.next_row, 3);

            // Search finds marker across the combined stream with zero duplicates
            let target = "ROW_2_RECORD\nNEW_LIVE_ROW_3";
            let matched = observer.search_and_prune(|text| {
                let pos = text.find(target)?;
                Some((pos, pos + target.len(), true))
            });
            assert_eq!(matched, Some(true));
        }

        #[test]
        fn test_observer_byte_cap_includes_live_row_before_allocation() {
            let mut observer = IncrementalPaneObserver::new_with_state(1, 0, 80, 24);
            let mut total_observed = 90;
            let max_cap = 100;

            // Live row (row_idx == live_cursor_y == 0) with 20 bytes. 90 + 20 = 110 > 100.
            let chunk = vec![(0, "12345678901234567890", false)];
            let res = observer.ingest_raw_chunk(
                &chunk,
                1,
                0,
                &mut total_observed,
                max_cap,
                "test_live_cap",
            );

            // Must fail closed with byte cap error before allocating current_live_line
            assert!(res.is_err());
            let err_str = res.unwrap_err().to_string();
            assert!(err_str.contains("observed byte cap exceeded"));
            assert!(observer.current_live_line.is_empty());
            assert_eq!(total_observed, 90);
        }

        #[test]
        fn test_nearest_rank_percentile_calculations() {
            // 100 items: 1..=100
            let v100: Vec<u128> = (1..=100).collect();
            // p50: rank = ceil(0.50 * 100) = 50 -> index 49 -> value 50
            assert_eq!(nearest_rank_percentile(&v100, 0.50), 50);
            // p90: rank = ceil(0.90 * 100) = 90 -> index 89 -> value 90
            assert_eq!(nearest_rank_percentile(&v100, 0.90), 90);
            // p95: rank = ceil(0.95 * 100) = 95 -> index 94 -> value 95
            assert_eq!(nearest_rank_percentile(&v100, 0.95), 95);
            // p99: rank = ceil(0.99 * 100) = 99 -> index 98 -> value 99
            assert_eq!(nearest_rank_percentile(&v100, 0.99), 99);

            // 1000 items: 1..=1000
            let v1000: Vec<u128> = (1..=1000).collect();
            assert_eq!(nearest_rank_percentile(&v1000, 0.50), 500);
            assert_eq!(nearest_rank_percentile(&v1000, 0.90), 900);
            assert_eq!(nearest_rank_percentile(&v1000, 0.95), 950);
            assert_eq!(nearest_rank_percentile(&v1000, 0.99), 990);
        }
    }
}
