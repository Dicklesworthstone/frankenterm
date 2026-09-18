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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ProfileArm {
        Resize,
        Echo,
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

    struct IncrementalPaneObserver {
        pane: u64,
        next_row: isize,
        expected_cols: usize,
        expected_viewport_rows: usize,
        accumulated_text: String,
        current_live_line: String,
    }

    impl IncrementalPaneObserver {
        async fn new(client: &mut DirectMuxClient, cx: &Cx, pane: u64) -> Result<Self> {
            let render = client.get_pane_render_changes_with_cx(cx, pane).await?;
            ensure!(render.pane_id as u64 == pane, "pane mismatch");
            ensure!(!render.alt_screen_active, "unexpected alt_screen_active in echo workload");
            ensure!(
                render.dimensions.cols == 80 && render.dimensions.viewport_rows == 24,
                "unexpected geometry in echo workload: cols={}, rows={}",
                render.dimensions.cols,
                render.dimensions.viewport_rows
            );
            Ok(Self {
                pane,
                next_row: render.dimensions.scrollback_top,
                expected_cols: 80,
                expected_viewport_rows: 24,
                accumulated_text: String::new(),
                current_live_line: String::new(),
            })
        }

        async fn poll_new_output(&mut self, client: &mut DirectMuxClient, cx: &Cx) -> Result<usize> {
            let render = client.get_pane_render_changes_with_cx(cx, self.pane).await?;
            ensure!(render.pane_id as u64 == self.pane, "pane mismatch in render changes");
            ensure!(!render.alt_screen_active, "unexpected alt_screen_active in echo workload");
            ensure!(
                render.dimensions.cols == self.expected_cols && render.dimensions.viewport_rows == self.expected_viewport_rows,
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

            let target_end = live_cursor_y.checked_add(1).context("cursor row overflow")?;
            if self.next_row >= target_end {
                return Ok(0);
            }

            const CHUNK_CAP: isize = 128;
            let mut bytes_ingested = 0usize;
            let mut cursor = self.next_row;

            while cursor < target_end {
                let chunk_end = cursor.saturating_add(CHUNK_CAP).min(target_end);
                let resp = client
                    .get_lines_with_cx(cx, self.pane, vec![cursor..chunk_end])
                    .await?;
                ensure!(resp.pane_id as u64 == self.pane, "pane mismatch in get_lines response");
                let (lines, _) = resp.lines.extract_data();
                ensure!(
                    lines.len() == (chunk_end - cursor) as usize,
                    "row count mismatch in get_lines: expected {}, got {}",
                    chunk_end - cursor,
                    lines.len()
                );

                // Exact row index and gap check
                for (offset, (row_idx, _)) in lines.iter().enumerate() {
                    let expected_row = cursor + offset as isize;
                    ensure!(
                        *row_idx == expected_row,
                        "row index gap detected: expected {}, got {}",
                        expected_row,
                        row_idx
                    );
                }

                for (row_idx, line) in lines {
                    let s = line.as_str();
                    if row_idx < live_cursor_y {
                        bytes_ingested += s.len();
                        self.accumulated_text.push_str(s.as_ref());
                        if !line.last_cell_was_wrapped() {
                            self.accumulated_text.push('\n');
                            bytes_ingested += 1;
                        }
                    } else {
                        self.current_live_line = s.into_owned();
                    }
                }
                cursor = chunk_end;
            }

            self.next_row = live_cursor_y;
            Ok(bytes_ingested)
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
                let new_bytes = self.poll_new_output(client, cx).await?;
                total_observed += new_bytes;
                ensure!(
                    total_observed <= max_observed_bytes,
                    "observed byte cap exceeded ({total_observed} > {max_observed_bytes}) waiting for {name}"
                );

                if let Some(val) = self.search_and_prune(&mut matcher) {
                    return Ok(val);
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
            self.wait_for_marker(
                client,
                cx,
                1024 * 1024,
                timeout,
                target,
                |text| {
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
                },
            )
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
            self.wait_for_marker(
                client,
                cx,
                16 * 1024 * 1024,
                timeout,
                &target,
                |text| {
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
                },
            )
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
            self.wait_for_marker(
                client,
                cx,
                4 * 1024 * 1024,
                timeout,
                &target,
                |text| {
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
                },
            )
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
                    other => bail!("unrecognized profile arm: {other}; expected resize or echo"),
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
            }
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
                .context("echo fixture startup deadline expired waiting for FT_ECHO_READY")?;
            emit(
                json!({
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
                }),
            )?;

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
                .context("stream start marker deadline expired")?;

            let mut latencies_us = Vec::with_capacity(self.trials);
            for trial in 0..self.trials {
                let nonce = format!("echo_trial_{trial:04}");
                let send_start = Instant::now();
                client
                    .write_to_pane_with_cx(
                        &cx,
                        self.pane,
                        format!("PROBE {nonce}\n").into_bytes(),
                    )
                    .await?;
                observer
                    .wait_for_probe(&mut client, &cx, &nonce, Duration::from_secs(5))
                    .await
                    .with_context(|| {
                        format!("keystroke echo deadline expired for trial {trial} ({nonce})")
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
                .context("terminal stream marker deadline expired; output was not ingested")?;
            let stream_duration = stream_start.elapsed();
            let duration_secs = stream_duration.as_secs_f64();
            ensure!(end_bytes >= start_bytes, "stream end bytes less than start bytes");
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
                .write_to_pane_with_cx(
                    &cx,
                    self.pane,
                    format!("EXIT {exit_nonce}\n").into_bytes(),
                )
                .await?;
            observer
                .wait_for_exit(&mut client, &cx, exit_nonce, Duration::from_secs(5))
                .await
                .context("exit receipt deadline expired; FT_EXIT was not observed")?;

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
}
