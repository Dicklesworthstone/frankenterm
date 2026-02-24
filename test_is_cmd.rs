use regex::Regex;

fn main() {
    let re = Regex::new(r#"^[a-zA-Z_][a-zA-Z0-9_]*=(?:'[^']*'|"[^"]*"|\$\([^)]*\)|`[^`]*`|\\.|[^\s])*(\s+|$)"#).unwrap();
    let text = "FOO=bar;rm -rf /";
    if let Some(mat) = re.find(text) {
        println!("Matched: {}", mat.as_str());
        let trimmed = &text[mat.end()..];
        println!("Remaining: {}", trimmed);
    } else {
        println!("No match");
    }
}
