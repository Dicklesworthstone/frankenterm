//! This is a little utility that strips escape sequences from
//! stdin and prints the result on stdout.
//! It preserves only printable characters and CR, LF and HT.
use std::io::{Read, Result, Write};
use termwiz::escape::parser::Parser;
use termwiz::escape::{Action, ControlCode};

fn append_stripped_action(output: &mut Vec<u8>, action: Action) {
    match action {
        Action::Print(c) => {
            let mut encoded = [0; 4];
            output.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
        }
        Action::PrintString(text) => output.extend_from_slice(text.as_bytes()),
        Action::Control(c) => match c {
            ControlCode::HorizontalTab | ControlCode::LineFeed | ControlCode::CarriageReturn => {
                output.push(c as u8)
            }
            _ => {}
        },
        _ => {}
    }
}

fn main() -> Result<()> {
    let mut buf = [0u8; 4096];
    let mut parser = Parser::new();
    let mut stripped = Vec::with_capacity(buf.len());
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    loop {
        let len = std::io::stdin().read(&mut buf)?;
        if len == 0 {
            return stdout.flush();
        }

        stripped.clear();
        parser.parse(&buf[..len], |action| {
            append_stripped_action(&mut stripped, action)
        });
        stdout.write_all(&stripped)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_batched_print_strings_and_allowed_controls() {
        let mut output = Vec::new();
        append_stripped_action(&mut output, Action::PrintString("batched text".to_string()));
        append_stripped_action(&mut output, Action::Control(ControlCode::HorizontalTab));
        append_stripped_action(&mut output, Action::Print('λ'));
        append_stripped_action(&mut output, Action::Control(ControlCode::LineFeed));
        append_stripped_action(&mut output, Action::Control(ControlCode::Bell));

        assert_eq!(output, "batched text\tλ\n".as_bytes());
    }

    #[test]
    fn default_parser_batching_does_not_erase_plain_text() {
        let mut parser = Parser::new();
        let mut output = Vec::new();
        parser.parse(b"hello\x1b[31m world\x1b[0m\r\n", |action| {
            append_stripped_action(&mut output, action)
        });

        assert_eq!(output, b"hello world\r\n");
    }
}
