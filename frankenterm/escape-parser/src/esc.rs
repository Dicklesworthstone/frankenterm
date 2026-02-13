use core::fmt::{Display, Error as FmtError, Formatter, Write as FmtWrite};
use num_derive::*;
use num_traits::{FromPrimitive, ToPrimitive};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Esc {
    Unspecified {
        intermediate: Option<u8>,
        /// The final character in the Escape sequence; this typically
        /// defines how to interpret the other parameters.
        control: u8,
    },
    Code(EscCode),
}

macro_rules! esc {
    ($low:expr) => {
        ($low as isize)
    };
    ($high:expr, $low:expr) => {
        ((($high as isize) << 8) | ($low as isize))
    };
}

#[derive(Debug, Clone, PartialEq, Eq, FromPrimitive, ToPrimitive, Copy)]
pub enum EscCode {
    /// RIS - Full Reset
    FullReset = esc!('c'),
    /// IND - Index.  Note that for Vt52 and Windows 10 ANSI consoles,
    /// this is interpreted as CursorUp
    Index = esc!('D'),
    /// NEL - Next Line
    NextLine = esc!('E'),
    /// Move the cursor to the bottom left corner of the screen
    CursorPositionLowerLeft = esc!('F'),
    /// HTS - Horizontal Tab Set
    HorizontalTabSet = esc!('H'),
    /// RI - Reverse Index – Performs the reverse operation of \n, moves cursor up one line,
    /// maintains horizontal position, scrolls buffer if necessary
    ReverseIndex = esc!('M'),
    /// SS2 Single shift of G2 character set affects next character only
    SingleShiftG2 = esc!('N'),
    /// SS3 Single shift of G3 character set affects next character only
    SingleShiftG3 = esc!('O'),
    /// SPA - Start of Guarded Area
    StartOfGuardedArea = esc!('V'),
    /// EPA - End of Guarded Area
    EndOfGuardedArea = esc!('W'),
    /// SOS - Start of String
    StartOfString = esc!('X'),
    /// DECID - Return Terminal ID (obsolete form of CSI c - aka DA)
    ReturnTerminalId = esc!('Z'),
    /// ST - String Terminator
    StringTerminator = esc!('\\'),
    /// PM - Privacy Message
    PrivacyMessage = esc!('^'),
    /// APC - Application Program Command
    ApplicationProgramCommand = esc!('_'),
    /// Used by tmux for setting the window title
    TmuxTitle = esc!('k'),

    /// DECBI - Back Index
    DecBackIndex = esc!('6'),
    /// DECSC - Save cursor position
    DecSaveCursorPosition = esc!('7'),
    /// DECRC - Restore saved cursor position
    DecRestoreCursorPosition = esc!('8'),
    /// DECPAM - Application Keypad
    DecApplicationKeyPad = esc!('='),
    /// DECPNM - Normal Keypad
    DecNormalKeyPad = esc!('>'),

    /// Designate G0 Character Set – DEC Line Drawing
    DecLineDrawingG0 = esc!('(', '0'),
    /// Designate G0 Character Set - UK
    UkCharacterSetG0 = esc!('(', 'A'),
    /// Designate G0 Character Set – US ASCII
    AsciiCharacterSetG0 = esc!('(', 'B'),

    /// Designate G1 Character Set – DEC Line Drawing
    DecLineDrawingG1 = esc!(')', '0'),
    /// Designate G1 Character Set - UK
    UkCharacterSetG1 = esc!(')', 'A'),
    /// Designate G1 Character Set – US ASCII
    AsciiCharacterSetG1 = esc!(')', 'B'),

    /// https://vt100.net/docs/vt510-rm/DECALN.html
    DecScreenAlignmentDisplay = esc!('#', '8'),

    /// DECDHL - DEC double-height line, top half
    DecDoubleHeightTopHalfLine = esc!('#', '3'),
    /// DECDHL - DEC double-height line, bottom half
    DecDoubleHeightBottomHalfLine = esc!('#', '4'),
    /// DECSWL - DEC single-width line
    DecSingleWidthLine = esc!('#', '5'),
    /// DECDWL - DEC double-width line
    DecDoubleWidthLine = esc!('#', '6'),

    /// These are typically sent by the terminal when keys are pressed
    ApplicationModeArrowUpPress = esc!('O', 'A'),
    ApplicationModeArrowDownPress = esc!('O', 'B'),
    ApplicationModeArrowRightPress = esc!('O', 'C'),
    ApplicationModeArrowLeftPress = esc!('O', 'D'),
    ApplicationModeHomePress = esc!('O', 'H'),
    ApplicationModeEndPress = esc!('O', 'F'),
    F1Press = esc!('O', 'P'),
    F2Press = esc!('O', 'Q'),
    F3Press = esc!('O', 'R'),
    F4Press = esc!('O', 'S'),
}

impl Esc {
    pub fn parse(intermediate: Option<u8>, control: u8) -> Self {
        Self::internal_parse(intermediate, control).unwrap_or_else(|_| Esc::Unspecified {
            intermediate,
            control,
        })
    }

    fn internal_parse(intermediate: Option<u8>, control: u8) -> Result<Self, ()> {
        let packed = match intermediate {
            Some(high) => ((u16::from(high)) << 8) | u16::from(control),
            None => u16::from(control),
        };

        let code = FromPrimitive::from_u16(packed).ok_or(())?;

        Ok(Esc::Code(code))
    }
}

impl Display for Esc {
    // TODO: data size optimization opportunity: if we could somehow know that we
    // had a run of CSI instances being encoded in sequence, we could
    // potentially collapse them together.  This is a few bytes difference in
    // practice so it may not be worthwhile with modern networks.
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        f.write_char(0x1b as char)?;
        use self::Esc::*;
        match self {
            Code(code) => {
                let packed = code
                    .to_u16()
                    .expect("num-derive failed to implement ToPrimitive");
                if packed > u16::from(u8::max_value()) {
                    write!(
                        f,
                        "{}{}",
                        (packed >> 8) as u8 as char,
                        (packed & 0xff) as u8 as char
                    )?;
                } else {
                    f.write_char((packed & 0xff) as u8 as char)?;
                }
            }
            Unspecified {
                intermediate,
                control,
            } => {
                if let Some(i) = intermediate {
                    write!(f, "{}{}", *i as char, *control as char)?;
                } else {
                    f.write_char(*control as char)?;
                }
            }
        };
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloc::string::String;

    fn encode(osc: &Esc) -> String {
        format!("{}", osc)
    }

    fn parse(esc: &str) -> Esc {
        let result = if esc.len() == 1 {
            Esc::parse(None, esc.as_bytes()[0])
        } else {
            Esc::parse(Some(esc.as_bytes()[0]), esc.as_bytes()[1])
        };

        assert_eq!(encode(&result), format!("\x1b{}", esc));

        result
    }

    #[test]
    fn test() {
        assert_eq!(parse("(0"), Esc::Code(EscCode::DecLineDrawingG0));
        assert_eq!(parse("(B"), Esc::Code(EscCode::AsciiCharacterSetG0));
        assert_eq!(parse(")0"), Esc::Code(EscCode::DecLineDrawingG1));
        assert_eq!(parse(")B"), Esc::Code(EscCode::AsciiCharacterSetG1));
        assert_eq!(parse("#3"), Esc::Code(EscCode::DecDoubleHeightTopHalfLine));
        assert_eq!(
            parse("#4"),
            Esc::Code(EscCode::DecDoubleHeightBottomHalfLine)
        );
        assert_eq!(parse("#5"), Esc::Code(EscCode::DecSingleWidthLine));
        assert_eq!(parse("#6"), Esc::Code(EscCode::DecDoubleWidthLine));
    }

    // --- Single-byte escape codes ---

    #[test]
    fn parse_full_reset() {
        assert_eq!(parse("c"), Esc::Code(EscCode::FullReset));
    }

    #[test]
    fn parse_index() {
        assert_eq!(parse("D"), Esc::Code(EscCode::Index));
    }

    #[test]
    fn parse_next_line() {
        assert_eq!(parse("E"), Esc::Code(EscCode::NextLine));
    }

    #[test]
    fn parse_cursor_position_lower_left() {
        assert_eq!(parse("F"), Esc::Code(EscCode::CursorPositionLowerLeft));
    }

    #[test]
    fn parse_horizontal_tab_set() {
        assert_eq!(parse("H"), Esc::Code(EscCode::HorizontalTabSet));
    }

    #[test]
    fn parse_reverse_index() {
        assert_eq!(parse("M"), Esc::Code(EscCode::ReverseIndex));
    }

    #[test]
    fn parse_single_shift_g2() {
        assert_eq!(parse("N"), Esc::Code(EscCode::SingleShiftG2));
    }

    #[test]
    fn parse_single_shift_g3() {
        assert_eq!(parse("O"), Esc::Code(EscCode::SingleShiftG3));
    }

    #[test]
    fn parse_start_of_guarded_area() {
        assert_eq!(parse("V"), Esc::Code(EscCode::StartOfGuardedArea));
    }

    #[test]
    fn parse_end_of_guarded_area() {
        assert_eq!(parse("W"), Esc::Code(EscCode::EndOfGuardedArea));
    }

    #[test]
    fn parse_start_of_string() {
        assert_eq!(parse("X"), Esc::Code(EscCode::StartOfString));
    }

    #[test]
    fn parse_return_terminal_id() {
        assert_eq!(parse("Z"), Esc::Code(EscCode::ReturnTerminalId));
    }

    #[test]
    fn parse_string_terminator() {
        assert_eq!(parse("\\"), Esc::Code(EscCode::StringTerminator));
    }

    #[test]
    fn parse_privacy_message() {
        assert_eq!(parse("^"), Esc::Code(EscCode::PrivacyMessage));
    }

    #[test]
    fn parse_application_program_command() {
        assert_eq!(parse("_"), Esc::Code(EscCode::ApplicationProgramCommand));
    }

    #[test]
    fn parse_tmux_title() {
        assert_eq!(parse("k"), Esc::Code(EscCode::TmuxTitle));
    }

    #[test]
    fn parse_dec_back_index() {
        assert_eq!(parse("6"), Esc::Code(EscCode::DecBackIndex));
    }

    #[test]
    fn parse_dec_save_cursor() {
        assert_eq!(parse("7"), Esc::Code(EscCode::DecSaveCursorPosition));
    }

    #[test]
    fn parse_dec_restore_cursor() {
        assert_eq!(parse("8"), Esc::Code(EscCode::DecRestoreCursorPosition));
    }

    #[test]
    fn parse_dec_application_keypad() {
        assert_eq!(parse("="), Esc::Code(EscCode::DecApplicationKeyPad));
    }

    #[test]
    fn parse_dec_normal_keypad() {
        assert_eq!(parse(">"), Esc::Code(EscCode::DecNormalKeyPad));
    }

    // --- Two-byte escape codes ---

    #[test]
    fn parse_uk_charset_g0() {
        assert_eq!(parse("(A"), Esc::Code(EscCode::UkCharacterSetG0));
    }

    #[test]
    fn parse_uk_charset_g1() {
        assert_eq!(parse(")A"), Esc::Code(EscCode::UkCharacterSetG1));
    }

    #[test]
    fn parse_ascii_charset_g1() {
        assert_eq!(parse(")B"), Esc::Code(EscCode::AsciiCharacterSetG1));
    }

    #[test]
    fn parse_dec_screen_alignment() {
        assert_eq!(parse("#8"), Esc::Code(EscCode::DecScreenAlignmentDisplay));
    }

    // --- Application mode keys ---

    #[test]
    fn parse_app_mode_arrow_up() {
        assert_eq!(parse("OA"), Esc::Code(EscCode::ApplicationModeArrowUpPress));
    }

    #[test]
    fn parse_app_mode_arrow_down() {
        assert_eq!(
            parse("OB"),
            Esc::Code(EscCode::ApplicationModeArrowDownPress)
        );
    }

    #[test]
    fn parse_app_mode_arrow_right() {
        assert_eq!(
            parse("OC"),
            Esc::Code(EscCode::ApplicationModeArrowRightPress)
        );
    }

    #[test]
    fn parse_app_mode_arrow_left() {
        assert_eq!(
            parse("OD"),
            Esc::Code(EscCode::ApplicationModeArrowLeftPress)
        );
    }

    #[test]
    fn parse_app_mode_home() {
        assert_eq!(parse("OH"), Esc::Code(EscCode::ApplicationModeHomePress));
    }

    #[test]
    fn parse_app_mode_end() {
        assert_eq!(parse("OF"), Esc::Code(EscCode::ApplicationModeEndPress));
    }

    #[test]
    fn parse_f1() {
        assert_eq!(parse("OP"), Esc::Code(EscCode::F1Press));
    }

    #[test]
    fn parse_f2() {
        assert_eq!(parse("OQ"), Esc::Code(EscCode::F2Press));
    }

    #[test]
    fn parse_f3() {
        assert_eq!(parse("OR"), Esc::Code(EscCode::F3Press));
    }

    #[test]
    fn parse_f4() {
        assert_eq!(parse("OS"), Esc::Code(EscCode::F4Press));
    }

    // --- Unspecified escape codes ---

    #[test]
    fn parse_unknown_single_byte() {
        let result = Esc::parse(None, b'Q');
        match result {
            Esc::Unspecified {
                intermediate,
                control,
            } => {
                assert_eq!(intermediate, None);
                assert_eq!(control, b'Q');
            }
            _ => panic!("expected Unspecified"),
        }
    }

    #[test]
    fn parse_unknown_two_byte() {
        let result = Esc::parse(Some(b'%'), b'G');
        match result {
            Esc::Unspecified {
                intermediate,
                control,
            } => {
                assert_eq!(intermediate, Some(b'%'));
                assert_eq!(control, b'G');
            }
            _ => panic!("expected Unspecified"),
        }
    }

    #[test]
    fn display_unspecified_no_intermediate() {
        let esc = Esc::Unspecified {
            intermediate: None,
            control: b'Q',
        };
        let s = format!("{}", esc);
        assert_eq!(s, "\x1bQ");
    }

    #[test]
    fn display_unspecified_with_intermediate() {
        let esc = Esc::Unspecified {
            intermediate: Some(b'%'),
            control: b'G',
        };
        let s = format!("{}", esc);
        assert_eq!(s, "\x1b%G");
    }

    // --- Trait impls ---

    #[test]
    fn esc_clone() {
        let a = Esc::Code(EscCode::FullReset);
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn esc_debug() {
        let esc = Esc::Code(EscCode::Index);
        let dbg = format!("{:?}", esc);
        assert!(dbg.contains("Index"));
    }

    #[test]
    fn esc_unspecified_clone() {
        let a = Esc::Unspecified {
            intermediate: Some(b'!'),
            control: b'p',
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn esc_code_clone_copy() {
        let a = EscCode::FullReset;
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn esc_code_debug() {
        let dbg = format!("{:?}", EscCode::DecSaveCursorPosition);
        assert!(dbg.contains("DecSaveCursorPosition"));
    }

    #[test]
    fn display_roundtrip_single_byte_codes() {
        // Verify display roundtrip for all single-byte codes
        let codes = [
            EscCode::FullReset,
            EscCode::Index,
            EscCode::NextLine,
            EscCode::HorizontalTabSet,
            EscCode::ReverseIndex,
            EscCode::DecBackIndex,
            EscCode::DecSaveCursorPosition,
            EscCode::DecRestoreCursorPosition,
            EscCode::DecApplicationKeyPad,
            EscCode::DecNormalKeyPad,
        ];
        for code in &codes {
            let esc = Esc::Code(*code);
            let s = format!("{}", esc);
            assert!(
                s.starts_with("\x1b"),
                "code {:?} should start with ESC",
                code
            );
            assert_eq!(s.len(), 2, "single-byte code {:?} should be 2 chars", code);
        }
    }

    #[test]
    fn display_roundtrip_two_byte_codes() {
        let codes = [
            EscCode::DecLineDrawingG0,
            EscCode::AsciiCharacterSetG0,
            EscCode::UkCharacterSetG0,
            EscCode::DecLineDrawingG1,
            EscCode::AsciiCharacterSetG1,
            EscCode::UkCharacterSetG1,
            EscCode::DecScreenAlignmentDisplay,
            EscCode::ApplicationModeArrowUpPress,
            EscCode::F1Press,
        ];
        for code in &codes {
            let esc = Esc::Code(*code);
            let s = format!("{}", esc);
            assert!(
                s.starts_with("\x1b"),
                "code {:?} should start with ESC",
                code
            );
            assert_eq!(s.len(), 3, "two-byte code {:?} should be 3 chars", code);
        }
    }
}
