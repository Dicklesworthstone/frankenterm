use frankenterm_escape_parser::parser::Parser;
use frankenterm_escape_parser::Action;
use std::path::PathBuf;

const GOLDEN_CASE: &[u8] =
    b"hello\r\n\x1b[12;34H\x1b[1;31mred\x1b[0m\x1b]0;termwiz-golden\x07\x1b(0q\x1b(B";

fn parse_actions(input: &[u8]) -> Vec<Action> {
    let mut parser = Parser::new();
    parser.parse_as_vec(input)
}

fn render_actions(actions: &[Action]) -> String {
    let mut rendered = actions
        .iter()
        .enumerate()
        .map(|(idx, action)| format!("{idx:02}: {action:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    rendered.push('\n');
    rendered
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/termwiz_escape_parser/basic_sequences.actions")
}

#[test]
fn basic_escape_sequences_match_golden_actions() {
    let actual = render_actions(&parse_actions(GOLDEN_CASE));
    let path = golden_path();

    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        std::fs::write(&path, &actual).unwrap_or_else(|err| {
            panic!("failed to update golden artifact {}: {err}", path.display())
        });
    }

    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read golden artifact {}: {err}", path.display()));
    assert_eq!(actual, expected, "golden artifact {}", path.display());
}
