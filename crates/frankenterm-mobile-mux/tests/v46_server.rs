//! The phone's session against a v0.13 mux server: whatever the remote pane
//! shows, the phone's terminal shows, and its scrollback keeps the history.

mod support;

use codec::TopologyEventKind;
use frankenterm_mobile_mux::Event;
use frankenterm_mobile_mux::mirror::history_rows;
use support::{
    Harness, all_text, assert_kept, assert_mirrored, server_frame, text, wrapped_text, wraps,
};

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

    // As much history as the phone keeps: 256 rows at this size.
    let history = history_rows(rows, 20);
    assert_eq!(history, 256);
    let asked = harness.server.line_ranges.len();
    harness.quiet_output(&numbered(0, history + 200));
    harness.push();
    assert_mirrored(&mut harness);

    // The phone holds the last `history` rows above the new screen; the
    // rows between them and the screen it showed were never fetched.
    let remote = all_text(&mut harness.server.term);
    let replayed = remote.len() - rows - history;
    assert_eq!(all_text(&mut harness.phone), remote[replayed..]);
    let skipped = isize::try_from(rows).unwrap()..isize::try_from(replayed).unwrap();
    assert!(skipped.len() > 100);
    for range in &harness.server.line_ranges[asked..] {
        assert!(
            range.end <= skipped.start || range.start >= skipped.end,
            "{range:?} reaches into {skipped:?}"
        );
    }
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

/// The phone's rows wrap where the remote's do, on screen and in its
/// scrollback, however they are drawn: so a copied line or a tapped address
/// runs on across them, and a row that only fills its width does not.
#[test]
fn rows_wrap_on_the_phone_where_they_wrap_on_the_remote() {
    let mut harness = Harness::new(5, 10);
    let mirrored = |harness: &mut Harness| {
        assert_mirrored(harness);
        let remote = wraps(&mut harness.server.term);
        assert_eq!(wraps(&mut harness.phone), remote);
        assert_eq!(
            all_text(&mut harness.phone),
            all_text(&mut harness.server.term)
        );
        // Nothing stands between a wrapping row and the row it runs on into
        // that is not there on the remote (a link or selection reads across).
        assert_eq!(
            wrapped_text(&mut harness.phone),
            wrapped_text(&mut harness.server.term)
        );
        remote
    };
    // Into history: two and a half rows; exactly two; a wide character
    // that does not fit the last column, which wraps a column early.
    harness.quiet_output(b"abcdefghijklmnopqrstuvwxy\r\n0123456789ABCDEFGHIJ\r\n");
    harness.quiet_output("123456789\u{754c}x\r\n".as_bytes());
    harness.quiet_output(&numbered(0, 4));
    harness.quiet_output(b"$ ");
    harness.attach();
    let first = mirrored(&mut harness);
    assert_eq!(
        first.iter().filter(|wrapped| **wrapped).count(),
        4,
        "{first:?}"
    );

    // Shown: output wraps as it scrolls in, and a prompt line that fills
    // the bottom row wraps only when the next key arrives.
    harness.output(b"echo The quick brown fox\r\nThe quick brown fox\r\n$ ");
    mirrored(&mut harness);
    // A wide character that does not fit wraps a column early as it scrolls
    // in, too.
    harness.output("\r\n123456789\u{754c}x\r\n$ ".as_bytes());
    mirrored(&mut harness);
    harness.output(b"12345678");
    mirrored(&mut harness);
    harness.output(b"9");
    let typed = mirrored(&mut harness);
    assert_eq!(
        typed[typed.len() - 2..],
        [true, false],
        "the full prompt row wraps"
    );

    // A line editor redraws the start of a wrapped line in place, and the
    // row it wraps onto is left alone; then the line is cut short.
    harness.output(b"\x1b[A\r$ X");
    mirrored(&mut harness);
    harness.output(b"\r\x1b[2K$ short\x1b[B\r\x1b[2K");
    let cut = mirrored(&mut harness);
    assert!(!cut[cut.len() - 2], "a line cut short no longer wraps");

    // A full-screen program's own long line.
    harness.output(b"\x1b[?1049h\x1b[H\x1b[2Jfull screen text that wraps");
    mirrored(&mut harness);
    harness.output(b"\x1b[?1049l");
    mirrored(&mut harness);
}

/// The phone's model ends the session on a piece of output that would make
/// it do more than its work budget allows (the harness fails there). A wide
/// screen is drawn within it: the top row, which history wraps into, is
/// redrawn without erasing it cell by cell, which costs the square of the
/// width (90,000 at 300 columns).
#[test]
fn a_wide_screen_is_drawn_within_the_phones_work_budget() {
    let mut harness = Harness::new(100, 300);
    harness.quiet_output(&numbered(0, 20));
    let mut long = "w".repeat(300 * 120 + 7).into_bytes();
    long.extend_from_slice(b"\r\n$ ");
    harness.quiet_output(&long);
    harness.attach();
    assert_mirrored(&mut harness);
    assert_kept(&mut harness);
    // The top row changes in place.
    harness.output(b"\x1b7\x1b[1;1HZ\x1b8");
    assert_mirrored(&mut harness);
    assert_kept(&mut harness);
}

/// A burst of short lines on a wide screen scrolls in, at once, as much
/// history as the phone keeps (256 rows here) and the screen below it.
/// They enter blank and are not erased.
#[test]
fn a_burst_of_short_lines_on_a_wide_screen_stays_within_the_budget() {
    let mut harness = Harness::new(30, 200);
    harness.quiet_output(b"$ ");
    harness.attach();
    harness.quiet_output(&numbered(0, 1100));
    harness.push();
    assert_mirrored(&mut harness);
    assert_kept(&mut harness);
}

/// At the most cells the phone shows (32,768), the first paint's wipe
/// costs the whole budget (both screens), on its own. A screenful of
/// output in one go then draws each row that leaves where it was, erasing
/// it: half. The row above the entering ones wraps into them, and is
/// carried on as it leaves; the rows entering scroll in blank.
#[test]
fn a_screenful_at_the_largest_size_stays_within_the_budget() {
    let mut harness = Harness::new(128, 256);
    harness.quiet_output(&numbered(0, 127));
    harness.attach();
    // From the bottom row: a line that wraps once, then enough lines to
    // move the screen by exactly its height.
    let mut batch = "x".repeat(300).into_bytes();
    batch.extend_from_slice(b"\r\n");
    batch.extend(numbered(127, 253));
    harness.output(&batch);
    assert_mirrored(&mut harness);
    assert_kept(&mut harness);
}

/// Shown first in a full-screen program at that size: the wipe takes the
/// whole budget, so the program's screen is entered (a screen's worth) in
/// a piece of output of its own.
#[test]
fn a_pane_shown_in_a_full_screen_program_at_the_largest_size_stays_within_the_budget() {
    let mut harness = Harness::new(128, 256);
    harness.quiet_output(&numbered(0, 40));
    harness.quiet_output(b"\x1b[?1049h\x1b[H\x1b[2J\x1b[44m~ editing\x1b[0m");
    harness.attach();
    assert!(harness.phone.is_alt_screen_active());
    assert_mirrored(&mut harness);
}

/// At that size, a full-screen program's screen is entered and drawn for
/// a screen's worth. Leaving it as the bottom row wraps into one more
/// redraws every primary row in place: with the switch back, the whole
/// budget, so the row wrapping into the new one carries on in place
/// instead of being drawn twice.
#[test]
fn a_full_screen_program_at_the_largest_size_stays_within_the_budget() {
    let mut harness = Harness::new(128, 256);
    harness.quiet_output(&numbered(0, 126));
    harness.quiet_output(&"y".repeat(300).into_bytes());
    harness.attach();
    harness.output(b"\x1b[?1049h\x1b[H\x1b[2J\x1b[44m~ editing\x1b[0m");
    assert_mirrored(&mut harness);
    let mut back = b"\x1b[?1049l".to_vec();
    back.extend("z".repeat(222).into_bytes());
    harness.output(&back);
    assert_mirrored(&mut harness);
    assert_kept(&mut harness);
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

    harness.phone = support::phone_terminal(5, 20);
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
    // Between v0.13 and the current window, and past it with an oldest
    // accepted version past this app's newest.
    for version in [48, 72] {
        let mut harness = Harness::new(4, 20);
        harness.server.codec_vers = version;
        harness.server.min_supported = version;
        harness.pump();
        assert!(!harness.session.is_ready());
        let reason = harness.events.iter().find_map(|event| match event {
            Event::Failed { reason } => Some(reason.clone()),
            _ => None,
        });
        let reason = reason.expect("a failure");
        assert!(
            reason.contains(&version.to_string()) && reason.contains("46") && reason.contains("71"),
            "{reason}"
        );
        assert_eq!(harness.server.requests, ["GetCodecVersion"]);
    }
}

#[test]
fn a_newer_server_that_still_accepts_this_apps_protocol_is_spoken_to() {
    let mut harness = Harness::new(5, 24);
    harness.server.codec_vers = codec::CODEC_VERSION + 1;
    harness.server.min_supported = codec::CODEC_VERSION_MIN_SUPPORTED;
    harness.quiet_output(&numbered(0, 12));
    harness.attach();
    assert!(harness.session.is_ready());
    assert_eq!(
        &harness.server.requests[..3],
        ["GetCodecVersion", "SetClientId", "ListPanesCoherent"]
    );
    assert_mirrored(&mut harness);
    let pane = harness.server.pane_id;
    harness.session.paste(pane, "three");
    harness.pump();
    assert_eq!(harness.server.pastes, ["three"]);
}

#[test]
fn the_current_window_keeps_earlier_servers_and_accepts_its_new_upper_bound() {
    for version in [61, 70, 71] {
        let mut harness = Harness::new(4, 20);
        harness.server.codec_vers = version;
        harness.attach();
        assert!(harness.session.is_ready(), "codec version {version}");
        assert_mirrored(&mut harness);
    }
}

#[test]
fn a_current_mux_like_a_macs_gui_is_spoken_to_in_its_own_layout() {
    use termwiz::input::{KeyCode, Modifiers};
    let mut harness = Harness::new(5, 24);
    harness.server.codec_vers = 71;
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

fn lists(harness: &Harness) -> usize {
    harness
        .server
        .requests
        .iter()
        .filter(|r| **r == "ListPanes")
        .count()
}

#[test]
fn a_current_mux_names_its_incarnation_before_its_first_pane_list() {
    let mut harness = Harness::new(4, 20);
    harness.server.codec_vers = 71;
    harness.attach();
    let named = harness
        .events
        .iter()
        .position(|event| *event == Event::Incarnation { id: [0x1c; 16] });
    let listed = harness
        .events
        .iter()
        .position(|event| matches!(event, Event::Panes(_)));
    assert!(
        named.is_some() && named < listed,
        "events: {:?}",
        harness.events
    );
    assert_eq!(
        harness.server.requests[..3],
        ["GetCodecVersion", "SetClientId", "ListPanesCoherent"]
    );
    assert_eq!(lists(&harness), 0, "the coherent list is the first list");
    assert_mirrored(&mut harness);

    // A v0.13 server has no incarnation and is never asked for one.
    let mut harness = Harness::new(4, 20);
    harness.attach();
    assert!(
        !harness
            .events
            .iter()
            .any(|event| matches!(event, Event::Incarnation { .. }))
    );
    assert!(!harness.server.requests.contains(&"ListPanesCoherent"));
}

#[test]
fn a_current_muxs_topology_changes_list_the_panes_again() {
    let mut harness = Harness::new(4, 20);
    harness.server.codec_vers = 71;
    harness.attach();
    // A new title needs no list.
    harness.server.topology(TopologyEventKind::TabTitleChanged {
        tab_id: 3,
        title: "renamed".to_owned(),
    });
    harness.pump();
    assert_eq!(lists(&harness), 0);
    harness.server.notify_tab_added();
    harness.pump();
    assert_eq!(lists(&harness), 1);
    // A change a newer server knows and this client does not: the list
    // says what it did.
    let mut unknown = vec![0x5a; 16];
    unknown.extend_from_slice(&[9, 99]);
    harness
        .server
        .to_phone
        .extend_from_slice(&server_frame(83, 0, &unknown, false));
    harness.pump();
    assert_eq!(lists(&harness), 2);
    assert!(harness.session.is_ready());
    harness.server.remove_pane();
    harness.pump();
    assert!(harness.events.contains(&Event::PaneGone { pane_id: 7 }));
    assert_eq!(lists(&harness), 3);
}

#[test]
fn a_current_mux_without_a_coherent_list_is_listed_plainly() {
    let mut harness = Harness::new(4, 20);
    harness.server.codec_vers = 71;
    harness.server.unfenced = true;
    harness.attach();
    assert!(
        !harness
            .events
            .iter()
            .any(|event| matches!(event, Event::Incarnation { .. }))
    );
    assert_eq!(
        harness.server.requests[..4],
        [
            "GetCodecVersion",
            "SetClientId",
            "ListPanesCoherent",
            "ListPanes"
        ]
    );
    assert_mirrored(&mut harness);
    harness.server.remove_pane();
    harness.pump();
    assert!(harness.events.contains(&Event::PaneGone { pane_id: 7 }));
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
fn a_wheel_turns_over_the_cell_it_is_over_in_bounded_notches() {
    use frankenterm_term::input::{MouseButton, MouseEventKind};
    let mut harness = Harness::new(5, 30);
    harness.attach();
    let pane = harness.server.pane_id;
    harness.session.wheel(pane, -2, 7, 3);
    harness.session.wheel(pane, 1, 0, 0);
    harness.session.wheel(pane, 0, 0, 0);
    harness.pump();
    let seen: Vec<_> = harness
        .server
        .mouse
        .iter()
        .map(|event| (event.kind, event.button, event.x, event.y))
        .collect();
    assert_eq!(
        seen,
        [
            (MouseEventKind::Press, MouseButton::WheelUp(1), 7, 3),
            (MouseEventKind::Press, MouseButton::WheelUp(1), 7, 3),
            (MouseEventKind::Press, MouseButton::WheelDown(1), 0, 0),
        ]
    );
    harness.session.wheel(pane, -1000, 0, 0);
    harness.pump();
    assert_eq!(
        harness.server.mouse.len(),
        3 + frankenterm_mobile_mux::session::MAX_WHEEL_STEPS as usize
    );
    assert_mirrored(&mut harness);
}

#[test]
fn a_programs_hold_on_the_mouse_is_reported_and_a_click_reaches_it() {
    use frankenterm_term::input::{MouseButton, MouseEventKind};
    let grabs = |harness: &Harness| -> Vec<bool> {
        harness
            .events
            .iter()
            .filter_map(|event| match event {
                Event::MouseGrab { grabbed, .. } => Some(*grabbed),
                _ => None,
            })
            .collect()
    };
    let mut harness = Harness::new(5, 30);
    harness.attach();
    assert_eq!(grabs(&harness), [false], "the first update says");
    harness.server.mouse_grabbed = true;
    harness.output(b"htop");
    harness.output(b" running");
    assert_eq!(grabs(&harness), [false, true], "only changes are reported");

    let pane = harness.server.pane_id;
    harness.session.click(pane, 12, 3);
    harness.pump();
    let seen: Vec<_> = harness
        .server
        .mouse
        .iter()
        .map(|event| (event.kind, event.button, event.x, event.y))
        .collect();
    assert_eq!(
        seen,
        [
            (MouseEventKind::Press, MouseButton::Left, 12, 3),
            (MouseEventKind::Release, MouseButton::Left, 12, 3),
        ]
    );
    harness.server.mouse_grabbed = false;
    harness.output(b"\r\n");
    assert_eq!(grabs(&harness), [false, true, false]);
}

#[test]
fn an_unfinished_paste_waits_on_its_pane_while_input_goes_to_another() {
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
    assert!(
        harness.server.pastes.is_empty(),
        "typing elsewhere does not end it"
    );
    assert_eq!(harness.server.written, b"x");
    harness.session.write(pane, b", then the rest\x1b[201~");
    harness.pump();
    assert_eq!(harness.server.pastes, ["half a paste, then the rest"]);
    assert_eq!(harness.server.written, b"x");
}

#[test]
fn the_rest_of_a_paste_into_a_pane_that_closed_ends_the_connection_unsent() {
    let mut harness = Harness::new(4, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"\x1b[200~make ");
    harness.pump();
    harness.server.remove_pane();
    harness.pump();
    assert!(harness.events.contains(&Event::PaneGone { pane_id: pane }));
    assert!(harness.session.is_ready());
    harness.session.write(pane, b"install\r\x1b[201~");
    harness.pump();
    assert!(harness.server.pastes.is_empty());
    assert!(harness.server.written.is_empty());
    assert!(!harness.session.is_ready());
    assert!(
        harness
            .events
            .iter()
            .any(|event| matches!(event, Event::Failed { .. }))
    );
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

fn restored_sizes(harness: &Harness) -> usize {
    harness
        .events
        .iter()
        .filter(|event| matches!(event, Event::PhoneSizeRestored { .. }))
        .count()
}

/// Removes the queued restore so a test can control its answer independently
/// of the server. Other requests must already have drained through the harness.
fn queued_restore_serial(harness: &mut Harness) -> u64 {
    let mut reader = frankenterm_mobile_mux::frame::FrameReader::default();
    reader.push(&harness.session.take_outgoing());
    let request = reader.next_frame().unwrap().expect("one queued restore");
    assert_eq!(request.ident, 14);
    assert!(reader.next_frame().unwrap().is_none());
    request.serial
}

#[test]
fn a_restore_is_restoring_until_the_server_answers_it() {
    let mut harness = Harness::new(6, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    assert!(!harness.session.restoring());
    harness.session.resize(pane, 10, 32);
    harness.pump();
    assert!(
        !harness.session.restoring(),
        "the phone's own size is not a restore"
    );

    // Still on its way to the server, then answered but the answer on its way.
    harness.hold_requests = true;
    harness.session.restore_size(pane);
    harness.pump();
    assert!(harness.session.restoring());
    assert_eq!(restored_sizes(&harness), 0, "queueing is not a receipt");
    harness.hold_requests = false;
    harness.hold = true;
    harness.pump();
    assert_eq!(
        harness
            .server
            .resizes
            .last()
            .map(|size| (size.rows, size.cols)),
        Some((6, 20))
    );
    assert!(
        harness.session.restoring(),
        "applied, but the phone cannot know yet"
    );
    assert_eq!(restored_sizes(&harness), 0, "the answer is still withheld");
    harness.hold = false;
    harness.pump();
    assert!(!harness.session.restoring());
    assert_eq!(restored_sizes(&harness), 1);

    // A connection that ended waits for nothing.
    harness.session.resize(pane, 10, 32);
    harness.pump();
    harness.hold_requests = true;
    harness.session.restore_size(pane);
    harness.pump();
    assert!(harness.session.restoring());
    harness.session.closed();
    harness.pump();
    assert!(!harness.session.restoring());
    assert_eq!(
        restored_sizes(&harness),
        1,
        "a dropped restore has no receipt"
    );
}

#[test]
fn a_rejected_restore_and_a_later_list_preserve_the_original_desktop_size() {
    let mut harness = Harness::new(6, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 10, 32);
    harness.pump();
    harness.session.restore_size(pane);
    let rejected = queued_restore_serial(&mut harness);

    // The restore has not reached the server. A list asked for afterward
    // still reports the phone's size and must not redefine the desktop size.
    harness.session.refresh_panes();
    harness.pump();
    assert!(harness.session.restoring());
    assert_eq!(
        (
            harness.session.panes()[0].size.rows,
            harness.session.panes()[0].size.cols
        ),
        (10, 32)
    );
    let refusal = varbincode::serialize(&"Error: resize refused".to_owned()).unwrap();
    harness
        .session
        .receive(&server_frame(0, rejected, &refusal, false));
    harness.pump();
    assert!(harness.session.is_ready());
    assert!(!harness.session.restoring());
    assert_eq!(
        restored_sizes(&harness),
        0,
        "an error is not successful restoration"
    );

    harness.session.restore_size(pane);
    harness.pump();
    let restored = harness.server.resizes.last().unwrap();
    assert_eq!((restored.rows, restored.cols), (6, 20));
    assert_eq!(restored_sizes(&harness), 1);
}

#[test]
fn a_list_taken_before_restore_ack_cannot_rebase_desktop_after_it() {
    let mut harness = Harness::new(6, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 10, 32);
    harness.pump();
    harness.session.restore_size(pane);
    let restore = queued_restore_serial(&mut harness);

    // Capture a real server list while the restore is still pending and
    // the pane is phone-sized. Hold its response across the restore ACK.
    harness.hold = true;
    harness.session.refresh_panes();
    harness.pump();
    let before_ack = std::mem::take(&mut harness.server.to_phone);
    assert!(!before_ack.is_empty(), "a held phone-sized pane list");

    // The server completes restoration before the delayed snapshot reaches
    // the client. Its request serial alone cannot prove snapshot freshness.
    harness.server.term.resize(support::size(6, 20));
    harness
        .session
        .receive(&server_frame(10, restore, &[], false));
    harness.hold = false;
    harness.pump();
    assert_eq!(restored_sizes(&harness), 1);
    harness.session.receive(&before_ack);
    harness.pump();

    harness.session.resize(pane, 11, 33);
    harness.pump();
    harness.session.restore_size(pane);
    harness.pump();
    let restored = harness.server.resizes.last().unwrap();
    assert_eq!((restored.rows, restored.cols), (6, 20));
    assert_eq!(restored_sizes(&harness), 2);

    // A list requested after the ACK is authoritative for a later desktop
    // resize, so the freshness fence does not permanently freeze geometry.
    harness.server.term.resize(support::size(8, 26));
    harness.session.refresh_panes();
    harness.pump();
    harness.session.resize(pane, 12, 34);
    harness.pump();
    harness.session.restore_size(pane);
    harness.pump();
    let restored = harness.server.resizes.last().unwrap();
    assert_eq!((restored.rows, restored.cols), (8, 26));
    assert_eq!(restored_sizes(&harness), 3);
}

#[test]
fn a_superseded_restore_reply_cannot_retire_a_newer_phone_size() {
    for reply in [0, 10] {
        let mut harness = Harness::new(6, 20);
        harness.attach();
        let pane = harness.server.pane_id;
        harness.session.resize(pane, 10, 32);
        harness.pump();
        harness.session.restore_size(pane);
        let old = queued_restore_serial(&mut harness);

        harness.session.resize(pane, 11, 33);
        harness.pump();
        harness.session.restore_size(pane);
        harness.hold_requests = true;
        harness.pump();
        assert!(harness.session.restoring());
        let body = if reply == 0 {
            varbincode::serialize(&"Error: old resize refused".to_owned()).unwrap()
        } else {
            Vec::new()
        };
        harness
            .session
            .receive(&server_frame(reply, old, &body, false));
        harness.pump();
        assert!(
            harness.session.restoring(),
            "the newer restore is still pending"
        );
        assert_eq!(
            restored_sizes(&harness),
            0,
            "a superseded answer cannot retire recovery"
        );

        harness.hold_requests = false;
        harness.pump();
        assert_eq!(restored_sizes(&harness), 1);
        let restored = harness.server.resizes.last().unwrap();
        assert_eq!((restored.rows, restored.cols), (6, 20));
        // Replaying the earlier serial cannot manufacture another receipt.
        harness.session.receive(&server_frame(10, old, &[], false));
        harness.pump();
        assert_eq!(restored_sizes(&harness), 1);
    }
}

#[test]
fn a_malformed_restore_acknowledgement_never_retires_recovery() {
    let mut harness = Harness::new(6, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 10, 32);
    harness.pump();
    harness.session.restore_size(pane);
    let restore = queued_restore_serial(&mut harness);
    harness
        .session
        .receive(&server_frame(10, restore, &[1], false));
    harness.pump();
    assert!(!harness.session.is_ready());
    assert_eq!(restored_sizes(&harness), 0);
}

/// The phone can tell whether what it sent reached the pane: input is
/// unanswered until the server has written it, and stays so when the
/// connection ends first.
#[test]
fn input_is_unanswered_until_the_server_has_written_it() {
    let mut harness = Harness::new(6, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    assert!(!harness.session.unanswered_input(pane));

    // On its way to the server, then written but the answer on its way.
    harness.hold_requests = true;
    harness.session.write(pane, b"ls\r");
    harness.session.resize(pane, 10, 32);
    harness.pump();
    assert!(harness.session.unanswered_input(pane));
    assert!(
        !harness.session.unanswered_input(pane + 1),
        "only that pane's"
    );
    harness.hold_requests = false;
    harness.hold = true;
    harness.pump();
    assert_eq!(harness.server.written, b"ls\r");
    assert!(
        harness.session.unanswered_input(pane),
        "written, but the phone cannot know yet"
    );
    harness.hold = false;
    harness.pump();
    assert!(
        !harness.session.unanswered_input(pane),
        "answered; the resize was never input"
    );

    // A paste and a click count too, and the pane's output answers neither.
    harness.hold_requests = true;
    harness.session.paste(pane, "echo hi");
    harness.session.click(pane, 1, 1);
    harness.output(b"x");
    assert!(harness.session.unanswered_input(pane));
    harness.hold_requests = false;
    harness.pump();
    assert_eq!(harness.server.pastes, ["echo hi"]);
    assert_eq!(harness.server.mouse.len(), 2);
    assert!(!harness.session.unanswered_input(pane));

    // A connection that ended first leaves it unanswered: it may never
    // have arrived.
    harness.hold_requests = true;
    harness.session.write(pane, b"exit\r");
    harness.pump();
    harness.session.closed();
    assert!(harness.session.unanswered_input(pane));
}

/// One PhoneSize event: pane, the desktop's rows and columns (none when
/// cleared), and the phone's rows and columns.
type PhoneSizeSeen = (usize, Option<(usize, usize)>, usize, usize);

fn phone_sizes(harness: &Harness) -> Vec<PhoneSizeSeen> {
    harness
        .events
        .iter()
        .filter_map(|event| match event {
            Event::PhoneSize {
                pane_id,
                desktop,
                rows,
                cols,
            } => Some((
                *pane_id,
                desktop.map(|size| (size.rows, size.cols)),
                *rows,
                *cols,
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn a_size_left_by_a_dropped_connection_is_given_back_by_the_next() {
    let mut harness = Harness::new(6, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 10, 32);
    harness.pump();
    let left = phone_sizes(&harness);
    assert_eq!(left, [(pane, Some((6, 20)), 10, 32)]);
    let desktop = match harness.events.iter().rev().find_map(|event| match event {
        Event::PhoneSize {
            desktop: Some(size),
            ..
        } => Some(*size),
        _ => None,
    }) {
        Some(size) => size,
        None => panic!("a desktop size"),
    };

    // The connection drops without a restore; a new one attaches.
    harness.session = frankenterm_mobile_mux::Session::new();
    harness.events.clear();
    harness.pump();
    assert!(harness.session.is_ready());
    // A size this pane no longer has is not taken over.
    assert!(!harness.session.adopt_phone_size(pane, desktop, 11, 32));
    assert!(!harness.session.adopt_phone_size(pane + 1, desktop, 10, 32));
    assert!(harness.session.adopt_phone_size(pane, desktop, 10, 32));
    harness.session.restore_size(pane);
    harness.pump();
    let restored = harness.server.resizes.last().copied().expect("a restore");
    assert_eq!((restored.rows, restored.cols), (6, 20));
    assert!(
        phone_sizes(&harness).is_empty(),
        "queueing emitted no retirement"
    );
    assert_eq!(
        restored_sizes(&harness),
        1,
        "the successful answer retires the record"
    );

    // Once restored, a later list shows the desktop's size: nothing to adopt.
    harness.session.refresh_panes();
    harness.pump();
    assert!(!harness.session.adopt_phone_size(pane, desktop, 10, 32));
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

fn activity(harness: &Harness) -> usize {
    harness
        .events
        .iter()
        .filter(|event| matches!(event, Event::Activity { .. }))
        .count()
}

#[test]
fn a_hidden_pane_that_changes_is_reported_and_one_on_screen_is_not() {
    let mut harness = Harness::new(5, 20);
    harness.attach();
    let pane = harness.server.pane_id;
    harness.output(b"on screen\r\n");
    harness.session.refresh_panes();
    harness.pump();
    assert_eq!(activity(&harness), 0, "output on screen is not news");

    harness.session.hide(pane);
    harness.server.title = "renamed".to_owned();
    harness.push();
    assert_eq!(activity(&harness), 0, "a new title alone is not output");

    harness.output(b"while away");
    assert!(harness.events.contains(&Event::Activity { pane_id: pane }));

    // Two updates that arrive together are one report.
    let before = activity(&harness);
    harness.hold = true;
    harness.output(b" more");
    harness.output(b"\r\nand more");
    harness.hold = false;
    harness.pump();
    assert_eq!(activity(&harness), before + 1);

    harness.phone = support::phone_terminal(5, 20);
    harness.session.show(pane);
    harness.pump();
    let shown = activity(&harness);
    harness.output(b"\r\nback on screen");
    assert_eq!(
        activity(&harness),
        shown,
        "a shown pane's output is not news"
    );
    assert_mirrored(&mut harness);
}

#[test]
fn a_pane_never_shown_that_scrolls_or_moves_its_cursor_between_lists_is_reported() {
    let mut harness = Harness::new(5, 20);
    harness.pump();
    assert!(harness.session.is_ready(), "events: {:?}", harness.events);
    let pane = harness.server.pane_id;
    harness.session.refresh_panes();
    harness.pump();
    assert_eq!(activity(&harness), 0, "lists alike say nothing happened");

    harness.quiet_output(b"working\r\n");
    harness.session.refresh_panes();
    harness.pump();
    assert_eq!(activity(&harness), 1);
    assert!(harness.events.contains(&Event::Activity { pane_id: pane }));

    // A new size makes programs redraw, which is not news; the next list
    // compares from the new size.
    harness.server.term.resize(support::size(6, 20));
    harness.quiet_output(b"redrawn\r\n");
    harness.session.refresh_panes();
    harness.pump();
    assert_eq!(activity(&harness), 1);
    harness.quiet_output(b"done\r\n");
    harness.session.refresh_panes();
    harness.pump();
    assert_eq!(activity(&harness), 2);
}
