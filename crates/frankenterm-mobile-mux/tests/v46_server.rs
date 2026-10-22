//! The phone's session against a v0.13 mux server: whatever the remote pane
//! shows, the phone's terminal shows, and its scrollback keeps the history.

mod support;

use frankenterm_mobile_mux::Event;
use frankenterm_mobile_mux::mirror::HISTORY_ROWS;
use support::{Harness, all_text, assert_mirrored, text};

fn numbered(from: usize, to: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for n in from..to {
        let color = 31 + n % 6;
        out.extend_from_slice(format!("\x1b[{color}mline\x1b[0m {n}\r\n").as_bytes());
    }
    out
}

fn remote_rows(harness: &mut Harness, from: usize, to: usize) -> Vec<String> {
    let range = isize::try_from(from).unwrap()..isize::try_from(to).unwrap();
    text(&mut harness.server.term, range)
}

#[test]
fn the_first_paint_carries_the_screen_its_styles_and_history() {
    let mut harness = Harness::new(6, 24);
    harness.quiet_output(&numbered(0, 15));
    harness.quiet_output(b"\x1b[1;4;38;2;255;128;0mbold orange\x1b[0m \x1b[7mrev\x1b[0m $ ");
    harness.chunk = 1;
    harness.attach();

    assert!(harness.events.iter().any(
        |event| matches!(event, Event::Ready { server_version } if server_version.contains("v0.13"))
    ));
    let panes = harness.events.iter().find_map(|event| match event {
        Event::Panes(panes) => Some(panes.clone()),
        _ => None,
    });
    let panes = panes.expect("a pane list");
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].pane_id, 7);
    assert_eq!(panes[0].tab_title, "tab one");
    assert_eq!(
        panes[0].working_dir.as_deref(),
        Some("file:///home/u/project")
    );
    assert_eq!((panes[0].size.rows, panes[0].size.cols), (6, 24));

    assert_mirrored(&mut harness);
    let remote = all_text(&mut harness.server.term);
    assert_eq!(all_text(&mut harness.phone), remote);
    assert!(remote.len() > 6, "the remote has history: {remote:?}");
}

#[test]
fn output_scrolls_finished_rows_into_the_phone_scrollback() {
    let mut harness = Harness::new(5, 30);
    harness.server.compress = true;
    harness.quiet_output(b"$ ");
    harness.attach();
    assert_mirrored(&mut harness);

    // Typing echoes in place; Enter and output scroll.
    harness.output(b"l");
    harness.output(b"s");
    assert_mirrored(&mut harness);
    harness.output(b"\r\n");
    harness.output(&numbered(0, 3));
    harness.output(b"$ ");
    assert_mirrored(&mut harness);

    // A burst between pushes, longer than the screen.
    harness.quiet_output(&numbered(3, 40));
    harness.quiet_output(b"$ ");
    harness.push();
    assert_mirrored(&mut harness);

    // A progress line rewritten in place, then left behind.
    for percent in [10, 55, 100] {
        harness.output(format!("\r\x1b[2Kprogress {percent}%").as_bytes());
        assert_mirrored(&mut harness);
    }
    harness.output(b"\r\ndone\r\n$ ");
    assert_mirrored(&mut harness);

    let remote = all_text(&mut harness.server.term);
    assert_eq!(all_text(&mut harness.phone), remote);
}

#[test]
fn a_row_rewritten_as_it_scrolls_away_keeps_its_final_text() {
    let mut harness = Harness::new(4, 20);
    harness.attach();
    harness.output(b"first draft");
    // The row is finished and pushed off the screen before the next push.
    harness.quiet_output(b"\r\x1b[2Kfinal text\r\n");
    harness.quiet_output(&numbered(0, 6));
    harness.push();
    assert_mirrored(&mut harness);
    let phone = all_text(&mut harness.phone);
    assert!(phone.contains(&"final text".to_owned()), "{phone:?}");
    assert!(!phone.contains(&"first draft".to_owned()), "{phone:?}");
    assert_eq!(phone, all_text(&mut harness.server.term));
}

#[test]
fn a_burst_longer_than_the_catch_up_keeps_its_last_rows() {
    let rows = 4;
    let mut harness = Harness::new(rows, 20);
    harness.output(b"start\r\n");
    harness.attach();
    assert_eq!(all_text(&mut harness.phone).len(), rows);

    let burst = HISTORY_ROWS + 200;
    harness.quiet_output(&numbered(0, burst));
    harness.push();
    assert_mirrored(&mut harness);

    // The rows the phone showed scroll away in their final form, then the
    // last HISTORY_ROWS rows above the new screen follow; the rows between
    // were never fetched.
    let remote = all_text(&mut harness.server.term);
    let mut expected = remote_rows(&mut harness, 0, rows);
    assert_eq!(expected, ["start", "line 0", "line 1", "line 2"]);
    expected.extend_from_slice(&remote[remote.len() - rows - HISTORY_ROWS..]);
    assert_eq!(all_text(&mut harness.phone), expected);
}

#[test]
fn a_full_screen_program_takes_over_and_gives_the_screen_back() {
    let mut harness = Harness::new(6, 30);
    harness.quiet_output(&numbered(0, 10));
    harness.quiet_output(b"$ vim");
    harness.attach();
    let history = all_text(&mut harness.phone);

    harness
        .output(b"\x1b[?1049h\x1b[H\x1b[2J\x1b[44m~ editing\x1b[0m\x1b[6;1H-- INSERT --\x1b[2;3H");
    assert_mirrored(&mut harness);
    harness.output(b"\x1b[3;1Hnew text");
    assert_mirrored(&mut harness);

    harness.output(b"\x1b[?1049l\r\n$ ");
    assert_mirrored(&mut harness);
    let phone = all_text(&mut harness.phone);
    assert_eq!(phone, all_text(&mut harness.server.term));
    assert_eq!(phone[..history.len() - 6], history[..history.len() - 6]);
}

#[test]
fn input_paste_and_resize_reach_the_pane() {
    let mut harness = Harness::new(6, 20);
    harness.quiet_output(&numbered(0, 12));
    harness.attach();
    let pane = harness.server.pane_id;

    harness.session.write(pane, b"ls -la\r");
    harness.session.paste(pane, "héllo\nwörld");
    harness.pump();
    assert_eq!(harness.server.written, b"ls -la\r");
    assert_eq!(harness.server.pastes, ["héllo\nwörld"]);

    harness.session.resize(pane, 10, 32);
    harness.pump();
    let resized = harness.server.resizes.last().copied().expect("a resize");
    assert_eq!((resized.rows, resized.cols, resized.dpi), (10, 32, 96));
    assert_eq!(
        (resized.pixel_width, resized.pixel_height),
        (32 * 8, 10 * 16)
    );
    assert_eq!(
        (harness.phone.get_size().rows, harness.phone.get_size().cols),
        (10, 32)
    );
    assert_mirrored(&mut harness);
    harness.output(&numbered(12, 20));
    assert_mirrored(&mut harness);

    harness.session.restore_size(pane);
    harness.pump();
    let restored = harness.server.resizes.last().copied().expect("a restore");
    assert_eq!((restored.rows, restored.cols), (6, 20));
    assert_mirrored(&mut harness);
}

#[test]
fn a_resize_is_drawn_at_the_new_size_even_without_a_redraw() {
    let mut harness = Harness::new(6, 20);
    harness.quiet_output(&numbered(0, 12));
    harness.attach();
    let pane = harness.server.pane_id;
    harness.server.quiet_resize = true;
    let outputs = harness.outputs;
    let seen = harness.events.len();

    // Output lands while the resize is in flight; the phone waits for it.
    harness.session.resize(pane, 9, 26);
    harness.server.term.advance_bytes(b"more\r\n");
    harness.pump();
    assert!(harness.outputs > outputs, "the answered resize was drawn");
    assert_eq!(
        (harness.phone.get_size().rows, harness.phone.get_size().cols),
        (9, 26)
    );
    assert_mirrored(&mut harness);
    let sizes: Vec<_> = harness.events[seen..]
        .iter()
        .filter_map(|event| match event {
            Event::Output { rows, cols, .. } => Some((*rows, *cols)),
            _ => None,
        })
        .collect();
    assert!(sizes.iter().all(|size| *size == (9, 26)), "{sizes:?}");
}

#[test]
fn showing_a_pane_again_redraws_it_from_scratch() {
    let mut harness = Harness::new(5, 20);
    harness.quiet_output(&numbered(0, 8));
    harness.attach();
    let pane = harness.server.pane_id;

    harness.session.hide(pane);
    let before = harness.outputs;
    harness.output(&numbered(8, 30));
    assert_eq!(harness.outputs, before, "a hidden pane draws nothing");

    harness.phone = support::terminal(5, 20);
    harness.session.show(pane);
    harness.pump();
    assert_mirrored(&mut harness);
    assert_eq!(
        all_text(&mut harness.phone),
        all_text(&mut harness.server.term)
    );
    assert_eq!(
        harness
            .server
            .requests
            .iter()
            .filter(|r| **r == "GetPaneRenderChanges")
            .count(),
        1,
        "the server already pushes this pane's changes"
    );
}

#[test]
fn a_refused_history_fetch_leaves_history_out_and_the_pane_carries_on() {
    let mut harness = Harness::new(4, 20);
    harness.quiet_output(&numbered(0, 10));
    harness.server.refuse_lines = 1;
    harness.attach();
    assert!(
        harness
            .events
            .iter()
            .all(|event| !matches!(event, Event::Failed { .. }))
    );
    // The screen came with the first push; only the history fetch failed.
    assert_mirrored(&mut harness);
    let history = remote_rows(&mut harness, 0, 7);
    assert!(
        all_text(&mut harness.phone)
            .iter()
            .all(|row| !history.contains(row) || row.is_empty())
    );

    harness.output(b"after\r\n");
    let phone = all_text(&mut harness.phone);
    assert!(phone.contains(&"after".to_owned()), "{phone:?}");
}

#[test]
fn a_server_on_another_protocol_is_refused() {
    // Between v0.13 and the current window, and past it.
    for version in [48, 71] {
        let mut harness = Harness::new(4, 20);
        harness.server.codec_vers = version;
        harness.pump();
        assert!(!harness.session.is_ready());
        let reason = harness.events.iter().find_map(|event| match event {
            Event::Failed { reason } => Some(reason.clone()),
            _ => None,
        });
        let reason = reason.expect("a failure");
        assert!(
            reason.contains(&version.to_string()) && reason.contains("46") && reason.contains("70"),
            "{reason}"
        );
        assert_eq!(harness.server.requests, ["GetCodecVersion"]);
    }
}

#[test]
fn a_current_mux_like_a_macs_gui_is_spoken_to_in_its_own_layout() {
    use termwiz::input::{KeyCode, Modifiers};
    let mut harness = Harness::new(5, 24);
    harness.server.codec_vers = 70;
    harness.quiet_output(&numbered(0, 12));
    harness.attach();
    assert_mirrored(&mut harness);
    let pane = harness.server.pane_id;
    harness
        .session
        .write(pane, b"ls\x1b[A\x1b[200~one\ntwo\x1b[201~");
    harness.session.paste(pane, "three");
    harness.pump();
    assert_eq!(harness.server.written, b"ls");
    assert_eq!(
        harness
            .server
            .keys
            .iter()
            .map(|key| (key.key, key.modifiers))
            .collect::<Vec<_>>(),
        [(KeyCode::UpArrow, Modifiers::NONE)]
    );
    assert_eq!(harness.server.pastes, ["one\ntwo", "three"]);
    harness.output(&numbered(12, 20));
    assert_mirrored(&mut harness);
}

#[test]
fn a_removed_pane_is_reported_and_the_list_refreshed() {
    let mut harness = Harness::new(4, 20);
    harness.attach();
    let lists = harness
        .server
        .requests
        .iter()
        .filter(|r| **r == "ListPanes")
        .count();
    harness.server.remove_pane();
    harness.pump();
    assert!(harness.events.contains(&Event::PaneGone { pane_id: 7 }));
    assert_eq!(
        harness
            .server
            .requests
            .iter()
            .filter(|r| **r == "ListPanes")
            .count(),
        lists + 1
    );
}

fn search_results(
    harness: &Harness,
    search: u64,
) -> (
    Vec<frankenterm_mobile_mux::SearchMatch>,
    usize,
    Option<String>,
) {
    harness
        .events
        .iter()
        .find_map(|event| match event {
            Event::SearchResults {
                search: id,
                matches,
                searched_rows,
                error,
                ..
            } if *id == search => Some((matches.clone(), *searched_rows, error.clone())),
            _ => None,
        })
        .expect("search results")
}

#[test]
fn a_search_reports_the_newest_matches_first_with_their_text() {
    let mut harness = Harness::new(5, 40);
    let mut output = Vec::new();
    for n in 0..40 {
        let line = if n % 10 == 3 {
            format!("build {n}: NEEDLE found\r\n")
        } else {
            format!("line {n}\r\n")
        };
        output.extend_from_slice(line.as_bytes());
    }
    harness.quiet_output(&output);
    harness.attach();
    let pane = harness.server.pane_id;
    let query = frankenterm_mobile_mux::SearchQuery {
        text: "needle".into(),
        case_sensitive: false,
        regex: false,
    };
    let search = harness.session.search(pane, &query).expect("a search");
    harness.pump();
    let (matches, searched, error) = search_results(&harness, search);
    assert_eq!(error, None);
    assert!(searched >= 40);
    let texts: Vec<_> = matches.iter().map(|hit| hit.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "build 33: NEEDLE found",
            "build 23: NEEDLE found",
            "build 13: NEEDLE found",
            "build 3: NEEDLE found"
        ]
    );
    assert_eq!((matches[0].start_column, matches[0].end_column), (10, 16));
    assert_eq!(
        &matches[0].text[matches[0].text_start..matches[0].text_end],
        "NEEDLE"
    );

    // Case matters when asked to.
    let exact = frankenterm_mobile_mux::SearchQuery {
        case_sensitive: true,
        ..query
    };
    let search = harness.session.search(pane, &exact).expect("a search");
    harness.pump();
    assert!(search_results(&harness, search).0.is_empty());
}

#[test]
fn a_search_walks_back_block_by_block_into_older_history() {
    let mut harness = Harness::new(4, 30);
    let mut output = Vec::new();
    for n in 0..6200 {
        let line = if n == 1500 {
            "early marker\r\n".to_owned()
        } else {
            format!("row {n}\r\n")
        };
        output.extend_from_slice(line.as_bytes());
    }
    harness.quiet_output(&output);
    harness.attach();
    let pane = harness.server.pane_id;
    let query = frankenterm_mobile_mux::SearchQuery {
        text: "early marker".into(),
        case_sensitive: true,
        regex: false,
    };
    let search = harness.session.search(pane, &query).expect("a search");
    harness.pump();
    let (matches, searched, _) = search_results(&harness, search);
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].text, "early marker");
    assert!(searched > 4000, "searched {searched}");
    let ranges = &harness.server.search_ranges;
    assert!(ranges.len() >= 2, "{ranges:?}");
    assert!(
        ranges.windows(2).all(|pair| pair[1].end == pair[0].start),
        "newest block first: {ranges:?}"
    );
}

#[test]
fn a_refused_search_reports_why_and_context_reads_rows() {
    let mut harness = Harness::new(5, 30);
    harness.quiet_output(&numbered(0, 20));
    harness.attach();
    let pane = harness.server.pane_id;
    let query = frankenterm_mobile_mux::SearchQuery {
        text: "(".into(),
        case_sensitive: false,
        regex: true,
    };
    let search = harness.session.search(pane, &query).expect("a search");
    harness.pump();
    let (matches, _, error) = search_results(&harness, search);
    assert!(matches.is_empty());
    assert_eq!(error.as_deref(), Some("regex parse error: unclosed group"));

    let request = harness
        .session
        .context(pane, 4..7)
        .expect("a context request");
    harness.pump();
    let lines = harness.events.iter().find_map(|event| match event {
        Event::Context {
            request: id, lines, ..
        } if *id == request => Some(lines.clone()),
        _ => None,
    });
    let lines = lines.expect("context");
    let rows: Vec<_> = lines
        .iter()
        .map(|line| (line.row, line.text.as_str()))
        .collect();
    assert_eq!(rows, [(4, "line 4"), (5, "line 5"), (6, "line 6")]);
}

#[test]
fn a_title_change_is_reported() {
    let mut harness = Harness::new(4, 20);
    harness.attach();
    harness.server.title = "cargo test".to_owned();
    harness.push();
    assert!(harness.events.contains(&Event::PaneTitle {
        pane_id: 7,
        title: "cargo test".to_owned()
    }));
    assert_eq!(harness.phone.get_title(), "cargo test");
}

fn query(text: &str) -> frankenterm_mobile_mux::SearchQuery {
    frankenterm_mobile_mux::SearchQuery {
        text: text.into(),
        case_sensitive: true,
        regex: false,
    }
}

fn requests(harness: &Harness, name: &str) -> usize {
    harness
        .server
        .requests
        .iter()
        .filter(|request| **request == name)
        .count()
}

#[test]
fn a_row_finished_and_scrolled_off_while_a_fetch_is_out_is_fetched_again() {
    let mut harness = Harness::new(4, 20);
    harness.attach();
    // A scroll needs rows fetched; the fetch is on its way while the
    // server keeps pushing.
    harness.hold_requests = true;
    harness.quiet_output(&numbered(0, 6));
    harness.push();
    // The cursor's row arrives half written, then is finished and scrolled
    // off between two pushes.
    harness.output(b"first draft");
    harness.quiet_output(b"\r\x1b[2Kfinal text\r\n");
    harness.quiet_output(&numbered(6, 12));
    harness.push();
    harness.hold_requests = false;
    harness.pump();
    assert_mirrored(&mut harness);
    let phone = all_text(&mut harness.phone);
    assert!(phone.contains(&"final text".to_owned()), "{phone:?}");
    assert!(!phone.contains(&"first draft".to_owned()), "{phone:?}");
    assert_eq!(phone, all_text(&mut harness.server.term));
}

#[test]
fn rows_fetched_before_a_hide_are_not_drawn_after_it() {
    let mut harness = Harness::new(4, 20);
    harness.quiet_output(&numbered(0, 8));
    harness.attach();
    let pane = harness.server.pane_id;
    // A redraw fetches its rows; the reply arrives after the phone looked
    // away, and the screen changes before it looks back.
    harness.session.hide(pane);
    harness.session.show(pane);
    harness.hold = true;
    harness.pump();
    harness.session.hide(pane);
    harness.hold = false;
    harness.pump();
    harness.output(b"\x1b[H\x1b[2Kchanged top\x1b[4;1H");
    harness.session.show(pane);
    harness.pump();
    assert_mirrored(&mut harness);
}

#[test]
fn the_first_paint_leaves_whatever_screen_the_phone_was_on() {
    let mut harness = Harness::new(4, 20);
    harness.quiet_output(&numbered(0, 8));
    // The phone's terminal last showed another pane's full-screen program.
    harness
        .phone
        .advance_bytes(b"\x1b[?1049h\x1b[2Jvim was here");
    harness.attach();
    assert!(!harness.phone.is_alt_screen_active());
    assert!(
        harness.phone.bracketed_paste_enabled(),
        "pastes arrive bracketed, to go to the server whole"
    );
    assert_mirrored(&mut harness);
    assert_eq!(
        all_text(&mut harness.phone),
        all_text(&mut harness.server.term)
    );
}

#[test]
fn a_v0_13_0_server_on_protocol_47_is_spoken_to() {
    let mut harness = Harness::new(4, 20);
    harness.server.codec_vers = 47;
    harness.quiet_output(b"hello");
    harness.attach();
    assert_mirrored(&mut harness);
}

#[test]
fn keys_go_as_key_presses_and_bracketed_pastes_as_pastes_in_order() {
    use termwiz::input::{KeyCode, Modifiers};
    let mut harness = Harness::new(4, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    harness
        .session
        .write(pane, b"ls\x1b[A\x1b[1;5C\x1b[200~one\rtwo\x1b[201~\r");
    harness.pump();
    let keys: Vec<_> = harness
        .server
        .keys
        .iter()
        .map(|key| (key.key, key.modifiers))
        .collect();
    assert_eq!(
        keys,
        [
            (KeyCode::UpArrow, Modifiers::NONE),
            (KeyCode::RightArrow, Modifiers::CTRL)
        ]
    );
    assert_eq!(harness.server.pastes, ["one\rtwo"]);
    assert_eq!(harness.server.written, b"ls\r");
    let order: Vec<_> = harness
        .server
        .requests
        .iter()
        .copied()
        .filter(|request| ["WriteToPane", "SendKeyDown", "SendPaste"].contains(request))
        .collect();
    assert_eq!(
        order,
        [
            "WriteToPane",
            "SendKeyDown",
            "SendKeyDown",
            "SendPaste",
            "WriteToPane"
        ]
    );
}

#[test]
fn an_unfinished_paste_is_sent_when_input_moves_to_another_pane() {
    let mut harness = Harness::new(4, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"\x1b[200~half a paste");
    harness.pump();
    assert!(
        harness.server.pastes.is_empty(),
        "a paste waits for its end"
    );
    harness.session.write(pane + 1, b"x");
    harness.pump();
    assert_eq!(harness.server.pastes, ["half a paste"]);
    assert_eq!(harness.server.written, b"x");
}

#[test]
fn the_desktop_gets_its_own_size_back_after_lists_taken_at_the_phone_size() {
    let mut harness = Harness::new(6, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    for (rows, cols) in [(10, 32), (12, 30)] {
        harness.session.resize(pane, rows, cols);
        harness.pump();
        // A list taken now shows the phone's size.
        harness.session.refresh_panes();
        harness.pump();
        harness.session.restore_size(pane);
        harness.pump();
        let restored = harness.server.resizes.last().copied().expect("a restore");
        assert_eq!((restored.rows, restored.cols), (6, 20));
    }
    // The desktop resizes the pane itself; a list after the restore says so.
    harness.server.term.resize(support::size(7, 22));
    harness.session.refresh_panes();
    harness.pump();
    harness.session.resize(pane, 10, 32);
    harness.pump();
    harness.session.restore_size(pane);
    harness.pump();
    let restored = harness.server.resizes.last().copied().expect("a restore");
    assert_eq!((restored.rows, restored.cols), (7, 22));
    let count = harness.server.resizes.len();
    harness.session.restore_size(pane);
    harness.pump();
    assert_eq!(harness.server.resizes.len(), count, "restored once");
}

#[test]
fn a_search_reaches_output_written_since_the_pane_was_listed() {
    let mut harness = Harness::new(4, 30);
    harness.pump();
    assert!(harness.session.is_ready());
    let mut output = Vec::new();
    for n in 0..3000 {
        let line = if n == 2990 {
            "late marker\r\n".to_owned()
        } else {
            format!("row {n}\r\n")
        };
        output.extend_from_slice(line.as_bytes());
    }
    harness.quiet_output(&output);
    let pane = harness.server.pane_id;
    let search = harness
        .session
        .search(pane, &query("late marker"))
        .expect("a search");
    harness.pump();
    let (matches, _, error) = search_results(&harness, search);
    assert_eq!(error, None);
    assert_eq!(matches.len(), 1, "{matches:?}");

    let search = harness
        .session
        .search(99, &query("late marker"))
        .expect("a search");
    harness.pump();
    let (matches, _, error) = search_results(&harness, search);
    assert!(matches.is_empty());
    assert_eq!(error.as_deref(), Some("The pane has closed."));
}

#[test]
fn a_new_desktop_tab_refreshes_the_pane_list() {
    let mut harness = Harness::new(4, 20);
    harness.attach();
    let lists = requests(&harness, "ListPanes");
    let panes = harness
        .events
        .iter()
        .filter(|event| matches!(event, Event::Panes(_)))
        .count();
    harness.server.notify_tab_added();
    harness.server.notify_tab_added();
    harness.pump();
    assert!(requests(&harness, "ListPanes") > lists);
    assert!(
        harness
            .events
            .iter()
            .filter(|event| matches!(event, Event::Panes(_)))
            .count()
            > panes
    );
}

#[test]
fn a_search_reply_too_large_fails_only_the_search() {
    let mut harness = Harness::new(4, 20);
    harness.attach();
    harness.server.oversize_search = true;
    let pane = harness.server.pane_id;
    let search = harness.session.search(pane, &query("e")).expect("a search");
    harness.pump();
    let (matches, _, error) = search_results(&harness, search);
    assert!(matches.is_empty());
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("Too many matches")),
        "{error:?}"
    );
    assert!(harness.session.is_ready());
    harness.output(b"still here");
    assert_mirrored(&mut harness);
}
