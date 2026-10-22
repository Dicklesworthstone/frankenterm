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
    let mut harness = Harness::new(4, 20);
    harness.server.codec_vers = 70;
    harness.pump();
    assert!(!harness.session.is_ready());
    let reason = harness.events.iter().find_map(|event| match event {
        Event::Failed { reason } => Some(reason.clone()),
        _ => None,
    });
    let reason = reason.expect("a failure");
    assert!(reason.contains("70") && reason.contains("46"), "{reason}");
    assert_eq!(harness.server.requests, ["GetCodecVersion"]);
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
