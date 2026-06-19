#[cfg(feature = "succinct_attrs")]
use termwiz::cell::AttributeRuns;
use termwiz::cell::{Cell, CellAttributes};
use termwiz::cell::{Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::surface::{Change, Line, Position, Surface};

fn attr_palette() -> Vec<CellAttributes> {
    let mut bold = CellAttributes::blank();
    bold.set_intensity(Intensity::Bold);

    let mut underlined = CellAttributes::blank();
    underlined.set_underline(Underline::Curly);

    let mut fg = CellAttributes::blank();
    fg.set_foreground(ColorAttribute::PaletteIndex(4));

    let mut bg = CellAttributes::blank();
    bg.set_background(ColorAttribute::PaletteIndex(2));

    let mut mixed = CellAttributes::blank();
    mixed.set_intensity(Intensity::Half);
    mixed.set_foreground(ColorAttribute::PaletteIndex(9));
    mixed.set_background(ColorAttribute::PaletteIndex(0));
    mixed.set_reverse(true);

    vec![CellAttributes::blank(), bold, underlined, fg, bg, mixed]
}

fn surface_dump_attrs() -> (Vec<Cell>, Vec<CellAttributes>, String) {
    let attrs = attr_palette();
    let mut surface = Surface::new(12, 3);
    surface.add_changes(vec![
        Change::AllAttributes(attrs[0].clone()),
        Change::Text("aaaa".into()),
        Change::AllAttributes(attrs[1].clone()),
        Change::Text("bb".into()),
        Change::AllAttributes(attrs[2].clone()),
        Change::Text("c".into()),
        Change::CursorPosition {
            x: Position::Absolute(0),
            y: Position::Absolute(1),
        },
        Change::AllAttributes(attrs[3].clone()),
        Change::Text("ddd".into()),
        Change::AllAttributes(attrs[4].clone()),
        Change::Text("eeee".into()),
        Change::AllAttributes(attrs[5].clone()),
        Change::Text("ff".into()),
    ]);

    let screen_text = surface.screen_chars_to_string();
    let mut cells = Vec::new();
    let mut oracle = Vec::new();
    for line in surface.screen_lines() {
        for cell in line.visible_cells() {
            let materialized = cell.as_cell();
            oracle.push(materialized.attrs().clone());
            cells.push(materialized);
        }
    }

    (cells, oracle, screen_text)
}

fn wrap_signature(lines: Vec<Line>) -> Vec<(String, bool, Vec<CellAttributes>)> {
    lines
        .into_iter()
        .map(|line| {
            let attrs = line
                .visible_cells()
                .map(|cell| cell.attrs().clone())
                .collect::<Vec<_>>();
            (
                line.as_str().into_owned(),
                line.last_cell_was_wrapped(),
                attrs,
            )
        })
        .collect()
}

#[test]
fn succinct_attrs_screen_dump_and_reflow_match_aos_oracle() {
    let (cells, oracle, screen_text) = surface_dump_attrs();
    assert_eq!(cells.len(), oracle.len());
    assert_eq!(
        screen_text, "aaaabbc     \ndddeeeeff   \n            \n",
        "screen dump text should remain fixed across feature modes"
    );

    let aos_line = Line::from_cells(cells.clone(), 7);
    let aos_wrap = wrap_signature(aos_line.clone().wrap(5, 8));
    assert!(
        aos_wrap.len() > 1,
        "oracle line must actually reflow for the byte-equivalence gate"
    );

    #[cfg(feature = "succinct_attrs")]
    {
        let runs_from_attrs = AttributeRuns::from_per_cell(&oracle);
        let runs_from_cells = AttributeRuns::from_cells(&cells);

        assert_eq!(runs_from_attrs, runs_from_cells);
        assert_eq!(runs_from_attrs.len(), oracle.len());
        assert_eq!(runs_from_attrs.to_per_cell(), oracle);
        assert_eq!(runs_from_attrs.get(oracle.len()), None);

        for (col, want) in oracle.iter().enumerate() {
            assert_eq!(
                runs_from_attrs.get(col),
                Some(want),
                "succinct attrs diverged from AoS screen dump at column {col}"
            );
        }
        assert!(
            runs_from_attrs.run_count() < oracle.len(),
            "screen dump should contain compressible attribute runs"
        );

        let rebuilt = runs_from_attrs
            .to_per_cell()
            .into_iter()
            .enumerate()
            .map(|(idx, attrs)| {
                let text = cells[idx].str().chars().next().unwrap_or(' ');
                Cell::new(text, attrs)
            })
            .collect::<Vec<_>>();
        let succinct_wrap = wrap_signature(Line::from_cells(rebuilt, 7).wrap(5, 8));
        assert_eq!(
            succinct_wrap, aos_wrap,
            "reflow from succinct attr dump must be byte-identical to AoS"
        );
    }

    #[cfg(not(feature = "succinct_attrs"))]
    {
        let roundtrip = oracle
            .iter()
            .enumerate()
            .map(|(idx, attrs)| {
                let text = cells[idx].str().chars().next().unwrap_or(' ');
                Cell::new(text, attrs.clone())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            wrap_signature(Line::from_cells(roundtrip, 7).wrap(5, 8)),
            aos_wrap,
            "default AoS oracle should exercise the same counted gate"
        );
    }
}
