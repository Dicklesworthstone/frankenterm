use termwiz::caps::{Capabilities, ColorLevel, ProbeHints};
use termwiz::render::terminfo::TerminfoRenderer;

lazy_static::lazy_static! {
    static ref CAPS: Capabilities = {
        let data = include_bytes!("../../termwiz/data/xterm-256color");
        let db = terminfo::Database::from_buffer(&data[..]).unwrap();
        Capabilities::new_with_hints(
            ProbeHints::new_from_env()
                .term(Some("xterm-256color".into()))
                .terminfo_db(Some(db))
                .color_level(Some(ColorLevel::TrueColor))
                .colorterm(None)
                .colorterm_bce(None)
                .term_program(Some("WezTerm".into()))
                .term_program_version(Some(config::wezterm_version().into())),
        )
        .expect("cannot fail to make internal Capabilities")
    };
}

pub fn new_frankenterm_terminfo_renderer() -> TerminfoRenderer {
    TerminfoRenderer::new(CAPS.clone())
}
