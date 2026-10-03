use crate::termwindow::{PaneInformation, TabInformation, UIItem, UIItemType};
use config::{ConfigHandle, TabBarColors, TabBarPosition};
pub use frankenterm_gui::status_text::{parse_status_text, parse_status_text_with_cell_limit};
use mlua::FromLua;
use termwiz::cell::{Cell, CellAttributes, unicode_column_width};
use termwiz::color::{AnsiColor, ColorSpec};
use termwiz::surface::SEQ_ZERO;
use termwiz_funcs::{FormatColor, FormatItem, format_as_escapes};
use wezterm_term::{Line, Progress};
use window::{IntegratedTitleButton, IntegratedTitleButtonAlignment, IntegratedTitleButtonStyle};

#[derive(Clone, Debug, PartialEq)]
pub struct TabBarState {
    /// The single row used by the horizontal tab bar.
    line: Line,
    items: Vec<TabEntry>,
    /// One row per cell line of the vertical (left/right) tab bar.
    /// Empty for the horizontal tab bar.
    vertical_lines: Vec<Line>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabBarItem {
    None,
    LeftStatus,
    RightStatus,
    Tab { tab_idx: usize, active: bool },
    NewTabButton,
    WindowButton(IntegratedTitleButton),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TabEntry {
    pub item: TabBarItem,
    pub title: Line,
    /// Position and size in cells.  `y` and `height` are only
    /// meaningful for the vertical tab bar; the horizontal bar is one row.
    x: usize,
    width: usize,
    y: usize,
    height: usize,
    /// For a tab entry: the bell rang while the tab was inactive.
    pub has_bell: bool,
}

impl TabEntry {
    fn horizontal(item: TabBarItem, title: Line, x: usize, width: usize) -> Self {
        Self {
            item,
            title,
            x,
            width,
            y: 0,
            height: 1,
            has_bell: false,
        }
    }
}

#[derive(Clone, Debug)]
struct TitleText {
    items: Vec<FormatItem>,
    len: usize,
}

fn call_format_tab_title(
    tab: &TabInformation,
    tab_info: &[TabInformation],
    pane_info: &[PaneInformation],
    config: &ConfigHandle,
    hover: bool,
    tab_max_width: usize,
) -> Option<TitleText> {
    match config::run_immediate_with_lua_config(|lua| {
        if let Some(lua) = lua {
            let tabs = lua.create_sequence_from(tab_info.iter().cloned())?;
            let panes = lua.create_sequence_from(pane_info.iter().cloned())?;

            let v = config::lua::emit_sync_callback(
                &*lua,
                (
                    "format-tab-title".to_string(),
                    (
                        tab.clone(),
                        tabs,
                        panes,
                        (**config).clone(),
                        hover,
                        tab_max_width,
                    ),
                ),
            )?;
            match &v {
                mlua::Value::Nil => Ok(None),
                mlua::Value::Table(_) => {
                    let items = <Vec<FormatItem>>::from_lua(v, &*lua)?;

                    let esc = format_as_escapes(items.clone())?;
                    let line = parse_status_text(&esc, CellAttributes::default());

                    Ok(Some(TitleText {
                        items,
                        len: line.len(),
                    }))
                }
                _ => {
                    let s = String::from_lua(v, &*lua)?;
                    let line = parse_status_text(&s, CellAttributes::default());
                    Ok(Some(TitleText {
                        len: line.len(),
                        items: vec![FormatItem::Text(s)],
                    }))
                }
            }
        } else {
            Ok(None)
        }
    }) {
        Ok(s) => s,
        Err(err) => {
            log::warn!("format-tab-title: {}", err);
            None
        }
    }
}

/// pct is a percentage in the range 0-100.
/// We want to map it to one of the nerdfonts:
///
/// * `md-checkbox_blank_circle_outline` (0xf0130) for an empty circle
/// * `md_circle_slice_1..=7` (0xf0a9e ..= 0xf0aa4) for a partly filled
///   circle
/// * `md_circle_slice_8` (0xf0aa5) for a filled circle
///
/// We use an empty circle for values close to 0%, a filled circle for values
/// close to 100%, and a partly filled circle for the rest (roughly evenly
/// distributed).
fn pct_to_glyph(pct: u8) -> char {
    match pct {
        0..=5 => '\u{f0130}',    // empty circle
        6..=18 => '\u{f0a9e}',   // centered at 12 (slightly smaller than 12.5)
        19..=31 => '\u{f0a9f}',  // centered at 25
        32..=43 => '\u{f0aa0}',  // centered at 37.5
        44..=56 => '\u{f0aa1}',  // half-filled circle, centered at 50
        57..=68 => '\u{f0aa2}',  // centered at 62.5
        69..=81 => '\u{f0aa3}',  // centered at 75
        82..=94 => '\u{f0aa4}',  // centered at 88 (slightly larger than 87.5)
        95..=100 => '\u{f0aa5}', // filled circle
        // Any other value is mapped to a filled circle.
        _ => '\u{f0aa5}',
    }
}

const INDETERMINATE_PROGRESS_GLYPH: char = '\u{f110}'; // fa-spinner

fn progress_indicator(progress: &Progress) -> Option<(FormatColor, String)> {
    match progress {
        Progress::None => None,
        Progress::Percentage(pct) => Some((
            FormatColor::AnsiColor(AnsiColor::Green),
            format!("{} ", pct_to_glyph(*pct)),
        )),
        Progress::Error(pct) => Some((
            FormatColor::AnsiColor(AnsiColor::Red),
            format!("{} ", pct_to_glyph(*pct)),
        )),
        Progress::Indeterminate => Some((
            FormatColor::AnsiColor(AnsiColor::Yellow),
            format!("{INDETERMINATE_PROGRESS_GLYPH} "),
        )),
    }
}

fn compute_tab_title(
    tab: &TabInformation,
    tab_info: &[TabInformation],
    pane_info: &[PaneInformation],
    config: &ConfigHandle,
    hover: bool,
    tab_max_width: usize,
) -> TitleText {
    metrics::histogram!("compute_tab_title.calls").record(1.);
    let title = call_format_tab_title(tab, tab_info, pane_info, config, hover, tab_max_width);

    match title {
        Some(title) => title,
        None => {
            let mut items = vec![];
            let mut len = 0;

            if let Some(pane) = &tab.active_pane {
                let mut title = if tab.tab_title.is_empty() {
                    pane.title.clone()
                } else {
                    tab.tab_title.clone()
                };

                let classic_spacing = if config.use_fancy_tab_bar { "" } else { " " };
                if config.show_tab_index_in_tab_bar {
                    let index = format!(
                        "{classic_spacing}{}: ",
                        tab.tab_index
                            + if config.tab_and_split_indices_are_zero_based {
                                0
                            } else {
                                1
                            }
                    );
                    len += unicode_column_width(&index, None);
                    items.push(FormatItem::Text(index));

                    title = format!("{}{classic_spacing}", title);
                }

                if let Some((color, graphic)) = progress_indicator(&pane.progress) {
                    len += unicode_column_width(&graphic, None);
                    items.push(FormatItem::Foreground(color));
                    items.push(FormatItem::Text(graphic));
                    items.push(FormatItem::Foreground(FormatColor::Default));
                }

                // We have a preferred soft minimum on tab width to make it
                // easier to click on tab titles, but we'll still go below
                // this if there are too many tabs to fit the window at
                // this width.
                if !config.use_fancy_tab_bar {
                    while len + unicode_column_width(&title, None) < 5 {
                        title.push(' ');
                    }
                }

                len += unicode_column_width(&title, None);
                items.push(FormatItem::Text(title));
            } else {
                let title = " no pane ".to_string();
                len += unicode_column_width(&title, None);
                items.push(FormatItem::Text(title));
            };

            TitleText { len, items }
        }
    }
}

fn is_tab_hover(mouse_x: Option<usize>, x: usize, tab_title_len: usize) -> bool {
    return mouse_x
        .map(|mouse_x| mouse_x >= x && mouse_x < x + tab_title_len)
        .unwrap_or(false);
}

/// Pick the retro tab bar cell attributes for a tab.
#[allow(clippy::too_many_arguments)]
fn tab_cell_attrs<'a>(
    active: bool,
    hover: bool,
    has_bell: bool,
    active_attrs: &'a CellAttributes,
    inactive_attrs: &'a CellAttributes,
    inactive_hover_attrs: &'a CellAttributes,
    inactive_bell_attrs: &'a CellAttributes,
    inactive_bell_hover_attrs: &'a CellAttributes,
) -> &'a CellAttributes {
    match (active, has_bell, hover) {
        (true, _, _) => active_attrs,
        (false, true, true) => inactive_bell_hover_attrs,
        (false, true, false) => inactive_bell_attrs,
        (false, false, true) => inactive_hover_attrs,
        (false, false, false) => inactive_attrs,
    }
}

/// Make `line` exactly `width` cells wide, padding with blanks in `attrs`.
fn fit_line_to_width(line: &mut Line, width: usize, attrs: &CellAttributes) {
    if line.len() > width {
        line.resize(width, SEQ_ZERO);
    }
    let pad_cell = Cell::blank_with_attrs(attrs.clone());
    while line.len() < width {
        line.insert_cell(line.len(), pad_cell.clone(), width, SEQ_ZERO);
    }
}

impl TabBarState {
    pub fn default() -> Self {
        Self {
            line: Line::with_width(1, SEQ_ZERO),
            items: vec![TabEntry::horizontal(
                TabBarItem::None,
                Line::from_text(" ", &CellAttributes::blank(), 1, None),
                1,
                1,
            )],
            vertical_lines: vec![],
        }
    }

    pub fn line(&self) -> &Line {
        &self.line
    }

    pub fn vertical_lines(&self) -> &[Line] {
        &self.vertical_lines
    }

    pub fn items(&self) -> &[TabEntry] {
        &self.items
    }

    fn integrated_title_buttons(
        mouse_x: Option<usize>,
        x: &mut usize,
        config: &ConfigHandle,
        items: &mut Vec<TabEntry>,
        line: &mut Line,
        colors: &TabBarColors,
    ) {
        let default_cell = if config.use_fancy_tab_bar {
            CellAttributes::default()
        } else {
            colors.new_tab().as_cell_attributes()
        };

        let default_cell_hover = if config.use_fancy_tab_bar {
            CellAttributes::default()
        } else {
            colors.new_tab_hover().as_cell_attributes()
        };

        let window_hide =
            parse_status_text(&config.tab_bar_style.window_hide, default_cell.clone());
        let window_hide_hover = parse_status_text(
            &config.tab_bar_style.window_hide_hover,
            default_cell_hover.clone(),
        );

        let window_maximize =
            parse_status_text(&config.tab_bar_style.window_maximize, default_cell.clone());
        let window_maximize_hover = parse_status_text(
            &config.tab_bar_style.window_maximize_hover,
            default_cell_hover.clone(),
        );

        let window_close =
            parse_status_text(&config.tab_bar_style.window_close, default_cell.clone());
        let window_close_hover = parse_status_text(
            &config.tab_bar_style.window_close_hover,
            default_cell_hover.clone(),
        );

        for button in &config.integrated_title_buttons {
            use IntegratedTitleButton as Button;
            let title = match button {
                Button::Hide => {
                    let hover = is_tab_hover(mouse_x, *x, window_hide_hover.len());

                    if hover {
                        &window_hide_hover
                    } else {
                        &window_hide
                    }
                }
                Button::Maximize => {
                    let hover = is_tab_hover(mouse_x, *x, window_maximize_hover.len());

                    if hover {
                        &window_maximize_hover
                    } else {
                        &window_maximize
                    }
                }
                Button::Close => {
                    let hover = is_tab_hover(mouse_x, *x, window_close_hover.len());

                    if hover {
                        &window_close_hover
                    } else {
                        &window_close
                    }
                }
            };

            line.append_line(title.to_owned(), SEQ_ZERO);

            let width = title.len();
            items.push(TabEntry::horizontal(
                TabBarItem::WindowButton(*button),
                title.to_owned(),
                *x,
                width,
            ));

            *x += width;
        }
    }

    /// Build a new tab bar from the current state
    /// mouse_x is some if the mouse is on the same row as the tab bar.
    /// title_width is the total number of cell columns in the window.
    /// window allows access to the tabs associated with the window.
    pub fn new(
        title_width: usize,
        mouse_x: Option<usize>,
        tab_info: &[TabInformation],
        pane_info: &[PaneInformation],
        colors: Option<&TabBarColors>,
        config: &ConfigHandle,
        left_status: &str,
        right_status: &str,
    ) -> Self {
        let colors = colors.cloned().unwrap_or_else(TabBarColors::default);

        let active_cell_attrs = colors.active_tab().as_cell_attributes();
        let inactive_hover_attrs = colors.inactive_tab_hover().as_cell_attributes();
        let inactive_cell_attrs = colors.inactive_tab().as_cell_attributes();
        let inactive_bell_attrs = colors.inactive_tab_bell().as_cell_attributes();
        let inactive_bell_hover_attrs = colors.inactive_tab_bell_hover().as_cell_attributes();
        let new_tab_hover_attrs = colors.new_tab_hover().as_cell_attributes();
        let new_tab_attrs = colors.new_tab().as_cell_attributes();

        let new_tab = parse_status_text(
            &config.tab_bar_style.new_tab,
            if config.use_fancy_tab_bar {
                CellAttributes::default()
            } else {
                new_tab_attrs.clone()
            },
        );
        let new_tab_hover = parse_status_text(
            &config.tab_bar_style.new_tab_hover,
            if config.use_fancy_tab_bar {
                CellAttributes::default()
            } else {
                new_tab_hover_attrs.clone()
            },
        );

        let use_integrated_title_buttons = config
            .window_decorations
            .contains(window::WindowDecorations::INTEGRATED_BUTTONS);

        // We ultimately want to produce a line looking like this:
        // ` | tab1-title x | tab2-title x |  +      . - X `
        // Where the `+` sign will spawn a new tab (or show a context
        // menu with tab creation options) and the other three chars
        // are symbols representing minimize, maximize and close.

        let mut active_tab_no = 0;

        let tab_titles: Vec<TitleText> = if config.show_tabs_in_tab_bar {
            tab_info
                .iter()
                .map(|tab| {
                    if tab.is_active {
                        active_tab_no = tab.tab_index;
                    }
                    compute_tab_title(
                        tab,
                        tab_info,
                        pane_info,
                        config,
                        false,
                        config.tab_max_width,
                    )
                })
                .collect()
        } else {
            vec![]
        };
        let titles_len: usize = tab_titles.iter().map(|s| s.len).sum();
        let number_of_tabs = tab_titles.len();

        let available_cells =
            title_width.saturating_sub(number_of_tabs.saturating_sub(1) + new_tab.len());
        let tab_width_max = if config.use_fancy_tab_bar || available_cells >= titles_len {
            // We can render each title with its full width
            usize::MAX
        } else {
            // We need to clamp the length to balance them out
            available_cells / number_of_tabs.max(1)
        }
        .min(config.tab_max_width);

        let mut line = Line::with_width(0, SEQ_ZERO);

        let mut x = 0;
        let mut items = vec![];

        let black_cell = Cell::blank_with_attrs(
            CellAttributes::default()
                .set_background(ColorSpec::TrueColor(*colors.background()))
                .clone(),
        );

        if use_integrated_title_buttons
            && config.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative
            && config.use_fancy_tab_bar == false
            && config.effective_tab_bar_position() == TabBarPosition::Top
        {
            for _ in 0..10 as usize {
                line.insert_cell(0, black_cell.clone(), title_width, SEQ_ZERO);
                x += 1;
            }
        }

        if use_integrated_title_buttons
            && config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
            && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Left
        {
            Self::integrated_title_buttons(mouse_x, &mut x, config, &mut items, &mut line, &colors);
        }

        let left_status_line = parse_status_text_with_cell_limit(
            left_status,
            black_cell.attrs().clone(),
            title_width.saturating_sub(x),
        );
        if left_status_line.len() > 0 {
            items.push(TabEntry::horizontal(
                TabBarItem::LeftStatus,
                left_status_line.clone(),
                x,
                left_status_line.len(),
            ));
            x += left_status_line.len();
            line.append_line(left_status_line, SEQ_ZERO);
        }

        for (tab_idx, tab_title) in tab_titles.iter().enumerate() {
            let tab_title_len = tab_title.len.min(tab_width_max);
            let active = tab_idx == active_tab_no;
            let hover = !active && is_tab_hover(mouse_x, x, tab_title_len);

            // Recompute the title only when the inputs differ from the first pass
            // (which used hover=false, max=config.tab_max_width). Skipping when
            // possible avoids a Lua roundtrip plus tab/pane HashMap clones per
            // tab on every update_title. See ft-ke3u9.
            let recomputed_title;
            let tab_title: &TitleText = if hover || tab_title_len != config.tab_max_width {
                metrics::histogram!("compute_tab_title.second_pass.recomputed").record(1.);
                recomputed_title = compute_tab_title(
                    &tab_info[tab_idx],
                    tab_info,
                    pane_info,
                    config,
                    hover,
                    tab_title_len,
                );
                &recomputed_title
            } else {
                metrics::histogram!("compute_tab_title.second_pass.cached").record(1.);
                tab_title
            };

            let cell_attrs = tab_cell_attrs(
                active,
                hover,
                tab_info[tab_idx].has_bell,
                &active_cell_attrs,
                &inactive_cell_attrs,
                &inactive_hover_attrs,
                &inactive_bell_attrs,
                &inactive_bell_hover_attrs,
            );

            let tab_start_idx = x;

            let esc = format_as_escapes(tab_title.items.clone()).expect("already parsed ok above");
            let mut tab_line = parse_status_text_with_cell_limit(
                &esc,
                if config.use_fancy_tab_bar {
                    CellAttributes::default()
                } else {
                    cell_attrs.clone()
                },
                tab_width_max,
            );

            let title = tab_line.clone();
            if tab_line.len() > tab_width_max {
                tab_line.resize(tab_width_max, SEQ_ZERO);
            }

            let width = tab_line.len();

            items.push(TabEntry {
                has_bell: tab_info[tab_idx].has_bell,
                ..TabEntry::horizontal(
                    TabBarItem::Tab { tab_idx, active },
                    title,
                    tab_start_idx,
                    width,
                )
            });

            line.append_line(tab_line, SEQ_ZERO);
            x += width;
        }

        // New tab button
        if config.show_new_tab_button_in_tab_bar {
            let hover = is_tab_hover(mouse_x, x, new_tab_hover.len());

            let new_tab_button = if hover { &new_tab_hover } else { &new_tab };

            let button_start = x;
            let width = new_tab_button.len();

            line.append_line(new_tab_button.clone(), SEQ_ZERO);

            items.push(TabEntry::horizontal(
                TabBarItem::NewTabButton,
                new_tab_button.clone(),
                button_start,
                width,
            ));

            x += width;
        }

        // Reserve place for integrated title buttons
        let title_width = if use_integrated_title_buttons
            && config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
            && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Right
        {
            let window_hide =
                parse_status_text(&config.tab_bar_style.window_hide, CellAttributes::default());
            let window_hide_hover = parse_status_text(
                &config.tab_bar_style.window_hide_hover,
                CellAttributes::default(),
            );

            let window_maximize = parse_status_text(
                &config.tab_bar_style.window_maximize,
                CellAttributes::default(),
            );
            let window_maximize_hover = parse_status_text(
                &config.tab_bar_style.window_maximize_hover,
                CellAttributes::default(),
            );
            let window_close = parse_status_text(
                &config.tab_bar_style.window_close,
                CellAttributes::default(),
            );
            let window_close_hover = parse_status_text(
                &config.tab_bar_style.window_close_hover,
                CellAttributes::default(),
            );

            let hide_len = window_hide.len().max(window_hide_hover.len());
            let maximize_len = window_maximize.len().max(window_maximize_hover.len());
            let close_len = window_close.len().max(window_close_hover.len());

            let mut width_to_reserve = 0;
            for button in &config.integrated_title_buttons {
                use IntegratedTitleButton as Button;
                let button_len = match button {
                    Button::Hide => hide_len,
                    Button::Maximize => maximize_len,
                    Button::Close => close_len,
                };
                width_to_reserve += button_len;
            }

            title_width.saturating_sub(width_to_reserve)
        } else {
            title_width
        };

        let status_space_available = title_width.saturating_sub(x);

        let mut right_status_line = parse_status_text_with_cell_limit(
            right_status,
            black_cell.attrs().clone(),
            status_space_available,
        );
        items.push(TabEntry::horizontal(
            TabBarItem::RightStatus,
            right_status_line.clone(),
            x,
            status_space_available,
        ));

        while right_status_line.len() > status_space_available {
            right_status_line.remove_cell(0, SEQ_ZERO);
        }

        line.append_line(right_status_line, SEQ_ZERO);
        while line.len() < title_width {
            line.insert_cell(x, black_cell.clone(), title_width, SEQ_ZERO);
        }

        if use_integrated_title_buttons
            && config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
            && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Right
        {
            x = title_width;
            Self::integrated_title_buttons(mouse_x, &mut x, config, &mut items, &mut line, &colors);
        }

        Self {
            line,
            items,
            vertical_lines: vec![],
        }
    }

    /// Build a vertical tab bar, drawn as a column on the left or right
    /// of the window in the retro style.
    /// `rows` is the number of cell rows available to the tab bar.
    /// `width` is the width of the column in cells.
    /// `mouse_row` is the row under the mouse, if the mouse is over the column.
    /// Left/right status and integrated title buttons are not shown.
    pub fn new_vertical(
        rows: usize,
        width: usize,
        mouse_row: Option<usize>,
        tab_info: &[TabInformation],
        pane_info: &[PaneInformation],
        colors: Option<&TabBarColors>,
        config: &ConfigHandle,
    ) -> Self {
        let colors = colors.cloned().unwrap_or_else(TabBarColors::default);
        let width = width.max(1);
        let tab_rows = config.vertical_tab_cell_height.max(1);

        let active_cell_attrs = colors.active_tab().as_cell_attributes();
        let inactive_hover_attrs = colors.inactive_tab_hover().as_cell_attributes();
        let inactive_cell_attrs = colors.inactive_tab().as_cell_attributes();
        let inactive_bell_attrs = colors.inactive_tab_bell().as_cell_attributes();
        let inactive_bell_hover_attrs = colors.inactive_tab_bell_hover().as_cell_attributes();
        let new_tab_hover_attrs = colors.new_tab_hover().as_cell_attributes();
        let new_tab_attrs = colors.new_tab().as_cell_attributes();
        let background_attrs = CellAttributes::default()
            .set_background(ColorSpec::TrueColor(*colors.background()))
            .clone();

        // Leave a little room at the right edge of each tab title.
        let title_max_width = width.saturating_sub(2).max(1);

        let mut vertical_lines: Vec<Line> = Vec::with_capacity(rows);
        let mut items = vec![];
        let mut y = 0;

        let is_hover = |y: usize| mouse_row.is_some_and(|row| row >= y && row < y + tab_rows);

        // Append an entry of `tab_rows` rows, clipped to the rows available.
        let push_entry = |vertical_lines: &mut Vec<Line>,
                          items: &mut Vec<TabEntry>,
                          y: &mut usize,
                          item: TabBarItem,
                          title: Line,
                          line: Line,
                          has_bell: bool| {
            let height = tab_rows.min(rows.saturating_sub(*y));
            if height == 0 {
                return;
            }
            for _ in 0..height {
                vertical_lines.push(line.clone());
            }
            items.push(TabEntry {
                item,
                title,
                x: 0,
                width,
                y: *y,
                height,
                has_bell,
            });
            *y += height;
        };

        if config.show_tabs_in_tab_bar {
            for (tab_idx, tab) in tab_info.iter().enumerate() {
                if y >= rows {
                    break;
                }
                let active = tab.is_active;
                let hover = !active && is_hover(y);
                let tab_title =
                    compute_tab_title(tab, tab_info, pane_info, config, hover, title_max_width);
                let cell_attrs = tab_cell_attrs(
                    active,
                    hover,
                    tab.has_bell,
                    &active_cell_attrs,
                    &inactive_cell_attrs,
                    &inactive_hover_attrs,
                    &inactive_bell_attrs,
                    &inactive_bell_hover_attrs,
                );

                let esc =
                    format_as_escapes(tab_title.items.clone()).expect("already parsed ok above");
                let mut tab_line =
                    parse_status_text_with_cell_limit(&esc, cell_attrs.clone(), width);
                let title = tab_line.clone();
                fit_line_to_width(&mut tab_line, width, cell_attrs);

                push_entry(
                    &mut vertical_lines,
                    &mut items,
                    &mut y,
                    TabBarItem::Tab { tab_idx, active },
                    title,
                    tab_line,
                    tab.has_bell,
                );
            }
        }

        if config.show_new_tab_button_in_tab_bar && y < rows {
            let hover = is_hover(y);
            let (style, attrs) = if hover {
                (&config.tab_bar_style.new_tab_hover, &new_tab_hover_attrs)
            } else {
                (&config.tab_bar_style.new_tab, &new_tab_attrs)
            };
            let mut new_tab_line = parse_status_text(style, attrs.clone());
            let title = new_tab_line.clone();
            fit_line_to_width(&mut new_tab_line, width, attrs);
            push_entry(
                &mut vertical_lines,
                &mut items,
                &mut y,
                TabBarItem::NewTabButton,
                title,
                new_tab_line,
                false,
            );
        }

        // The rest of the column is empty tab bar background.  Register it
        // as a tab bar item so that clicks there do not reach the pane.
        if y < rows {
            let mut empty_line = Line::with_width(0, SEQ_ZERO);
            fit_line_to_width(&mut empty_line, width, &background_attrs);
            items.push(TabEntry {
                item: TabBarItem::None,
                title: Line::with_width(0, SEQ_ZERO),
                x: 0,
                width,
                y,
                height: rows - y,
                has_bell: false,
            });
            while y < rows {
                vertical_lines.push(empty_line.clone());
                y += 1;
            }
        }

        Self {
            line: Line::with_width(0, SEQ_ZERO),
            items,
            vertical_lines,
        }
    }

    /// Compute the clickable areas of the tab bar in pixels.
    /// `left` and `top` are the pixel origin of the tab bar.
    pub fn compute_ui_items(
        &self,
        left: usize,
        top: usize,
        cell_width: usize,
        cell_height: usize,
    ) -> Vec<UIItem> {
        self.items
            .iter()
            .map(|entry| UIItem {
                x: left + entry.x * cell_width,
                width: entry.width * cell_width,
                y: top + entry.y * cell_height,
                height: entry.height * cell_height,
                item_type: UIItemType::TabBar(entry.item),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indeterminate_progress_has_visible_indicator() {
        let (color, graphic) = progress_indicator(&Progress::Indeterminate)
            .expect("indeterminate progress should render an indicator");

        assert_eq!(graphic, format!("{INDETERMINATE_PROGRESS_GLYPH} "));
        assert!(unicode_column_width(&graphic, None) > 0);
        assert!(matches!(color, FormatColor::AnsiColor(AnsiColor::Yellow)));
    }

    #[test]
    fn percentage_and_error_progress_keep_existing_colors() {
        let (percentage_color, percentage_graphic) =
            progress_indicator(&Progress::Percentage(50)).expect("percentage progress indicator");
        let (error_color, error_graphic) =
            progress_indicator(&Progress::Error(50)).expect("error progress indicator");

        assert_eq!(percentage_graphic, error_graphic);
        assert!(matches!(
            percentage_color,
            FormatColor::AnsiColor(AnsiColor::Green)
        ));
        assert!(matches!(
            error_color,
            FormatColor::AnsiColor(AnsiColor::Red)
        ));
    }

    #[test]
    fn no_progress_has_no_indicator() {
        assert!(progress_indicator(&Progress::None).is_none());
    }

    fn tab(tab_index: usize, is_active: bool, has_bell: bool) -> TabInformation {
        TabInformation {
            tab_id: tab_index,
            tab_index,
            is_active,
            is_last_active: false,
            active_pane: None,
            window_id: 0,
            tab_title: String::new(),
            has_bell,
        }
    }

    fn vertical_config(cell_height: usize) -> ConfigHandle {
        let mut config = config::Config::default_config();
        config.use_fancy_tab_bar = false;
        config.tab_bar_position = TabBarPosition::Left;
        config.vertical_tab_width = 12;
        config.vertical_tab_cell_height = cell_height;
        config::ConfigHandle::detached(config)
    }

    fn bg_at(line: &Line, col: usize) -> termwiz::color::ColorAttribute {
        line.get_cell(col).expect("cell").attrs().background()
    }

    #[test]
    fn vertical_tab_bar_lays_out_tabs_as_rows() {
        config::designate_this_as_the_main_thread();
        let config = vertical_config(1);
        let tabs = vec![tab(0, false, false), tab(1, true, false), tab(2, false, true)];
        let bar = TabBarState::new_vertical(10, 12, None, &tabs, &[], None, &config);

        assert_eq!(bar.vertical_lines().len(), 10);
        assert!(bar.vertical_lines().iter().all(|line| line.len() == 12));

        let layout: Vec<(TabBarItem, usize, usize)> = bar
            .items()
            .iter()
            .map(|entry| (entry.item, entry.y, entry.height))
            .collect();
        assert_eq!(
            layout,
            vec![
                (
                    TabBarItem::Tab {
                        tab_idx: 0,
                        active: false
                    },
                    0,
                    1
                ),
                (
                    TabBarItem::Tab {
                        tab_idx: 1,
                        active: true
                    },
                    1,
                    1
                ),
                (
                    TabBarItem::Tab {
                        tab_idx: 2,
                        active: false
                    },
                    2,
                    1
                ),
                (TabBarItem::NewTabButton, 3, 1),
                (TabBarItem::None, 4, 6),
            ]
        );
        assert!(bar.items()[2].has_bell);

        // Tab colors fill the whole row, including the padding.
        let colors = TabBarColors::default();
        let bell_bg = colors.inactive_tab_bell().as_cell_attributes().background();
        let active_bg = colors.active_tab().as_cell_attributes().background();
        let inactive_bg = colors.inactive_tab().as_cell_attributes().background();
        assert_eq!(bg_at(&bar.vertical_lines()[0], 11), inactive_bg);
        assert_eq!(bg_at(&bar.vertical_lines()[1], 11), active_bg);
        assert_eq!(bg_at(&bar.vertical_lines()[2], 0), bell_bg);
        assert_eq!(bg_at(&bar.vertical_lines()[2], 11), bell_bg);
    }

    #[test]
    fn vertical_tab_bar_hover_and_clipping() {
        config::designate_this_as_the_main_thread();
        let config = vertical_config(2);
        let tabs = vec![tab(0, true, false), tab(1, false, true), tab(2, false, false)];
        // Mouse over the second row of tab 1 (rows 2..4).
        let bar = TabBarState::new_vertical(5, 12, Some(3), &tabs, &[], None, &config);

        assert_eq!(bar.vertical_lines().len(), 5);
        let layout: Vec<(TabBarItem, usize, usize)> = bar
            .items()
            .iter()
            .map(|entry| (entry.item, entry.y, entry.height))
            .collect();
        // Tab 2 only has one of its two rows visible; there is no room
        // for the new tab button or empty space.
        assert_eq!(layout.len(), 3);
        assert_eq!(layout[1].1, 2);
        assert_eq!(layout[1].2, 2);
        assert_eq!(layout[2].1, 4);
        assert_eq!(layout[2].2, 1);

        let colors = TabBarColors::default();
        let bell_hover_bg = colors
            .inactive_tab_bell_hover()
            .as_cell_attributes()
            .background();
        assert_eq!(bg_at(&bar.vertical_lines()[2], 5), bell_hover_bg);
        assert_eq!(bg_at(&bar.vertical_lines()[3], 5), bell_hover_bg);
    }

    #[test]
    fn vertical_ui_items_are_offset_by_origin() {
        config::designate_this_as_the_main_thread();
        let config = vertical_config(1);
        let tabs = vec![tab(0, true, false), tab(1, false, false)];
        let bar = TabBarState::new_vertical(4, 12, None, &tabs, &[], None, &config);
        let ui = bar.compute_ui_items(3, 5, 8, 16);
        assert_eq!(ui.len(), 4);
        assert_eq!((ui[1].x, ui[1].y, ui[1].width, ui[1].height), (3, 21, 96, 16));
        // Empty space below the new tab button.
        assert_eq!((ui[3].y, ui[3].height), (5 + 3 * 16, 16));
    }
}
