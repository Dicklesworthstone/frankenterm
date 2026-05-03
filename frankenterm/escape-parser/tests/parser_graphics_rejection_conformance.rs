//! Conformance checks for malformed graphics escape parser rejection.
//!
//! Kitty graphics APC, sixel DCS, and iTerm2 File OSC use different wire
//! envelopes, but malformed inputs should have the same externally visible
//! result: no semantic graphics action is accepted from the stream.

use frankenterm_escape_parser::Action;
use frankenterm_escape_parser::osc::{ITermProprietary, OperatingSystemCommand};
use frankenterm_escape_parser::parser::Parser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserSurface {
    KittyApc,
    SixelDcs,
    ITermFileOsc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectionVerdict {
    Rejected,
    Accepted,
}

struct MalformedFixture {
    name: &'static str,
    kitty: &'static [u8],
    sixel: &'static [u8],
    iterm: &'static [u8],
}

fn parse_actions(input: &[u8]) -> Vec<Action> {
    let mut parser = Parser::new();
    parser.parse_as_vec(input)
}

fn rejection_verdict(surface: ParserSurface, actions: &[Action]) -> RejectionVerdict {
    let accepted = actions.iter().any(|action| match (surface, action) {
        (ParserSurface::KittyApc, Action::KittyImage(_)) => true,
        (ParserSurface::SixelDcs, Action::Sixel(_)) => true,
        (ParserSurface::ITermFileOsc, Action::OperatingSystemCommand(command)) => matches!(
            command.as_ref(),
            OperatingSystemCommand::ITermProprietary(ITermProprietary::File(_))
        ),
        _ => false,
    });

    if accepted {
        RejectionVerdict::Accepted
    } else {
        RejectionVerdict::Rejected
    }
}

#[test]
fn malformed_graphics_parsers_reject_without_semantic_actions() {
    let fixtures = [
        MalformedFixture {
            name: "empty_graphics_payload",
            kitty: b"\x1b_G\x1b\\",
            sixel: b"\x1bPq\x1b\\",
            iterm: b"\x1b]1337;File=\x07",
        },
        MalformedFixture {
            name: "malformed_payload_encoding",
            kitty: b"\x1b_Gt=f;not base64\x1b\\",
            sixel: b"\x1bPq!12\x1b\\",
            iterm: b"\x1b]1337;File=name=YmFk:not base64\x07",
        },
        MalformedFixture {
            name: "oversized_declared_image",
            kitty: b"\x1b_Gt=f,S=999999999999999999999;@@@\x1b\\",
            sixel: b"\x1bPq\"1;1;99999;99999@\x1b\\",
            iterm: b"\x1b]1337;File=size=999999999999999999999:@@@\x07",
        },
    ];

    for fixture in fixtures {
        let cases = [
            (
                ParserSurface::KittyApc,
                rejection_verdict(ParserSurface::KittyApc, &parse_actions(fixture.kitty)),
            ),
            (
                ParserSurface::SixelDcs,
                rejection_verdict(ParserSurface::SixelDcs, &parse_actions(fixture.sixel)),
            ),
            (
                ParserSurface::ITermFileOsc,
                rejection_verdict(ParserSurface::ITermFileOsc, &parse_actions(fixture.iterm)),
            ),
        ];

        for (surface, verdict) in cases {
            assert_eq!(
                RejectionVerdict::Rejected,
                verdict,
                "{} accepted malformed input on {:?}",
                fixture.name,
                surface
            );
        }
    }
}
