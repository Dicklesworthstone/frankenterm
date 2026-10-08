use crate::termwindow::{PaneInformation, TabInformation, UIItem, UIItemType};
use config::{ConfigHandle, TabBarColors};
pub use frankenterm_gui::status_text::{parse_status_text, parse_status_text_with_cell_limit};
use mlua::{FromLua, IntoLua};
use std::rc::Rc;
use termwiz::cell::{Cell, CellAttributes, unicode_column_width};
use termwiz::color::{AnsiColor, ColorSpec};
use termwiz::surface::SEQ_ZERO;
use termwiz_funcs::{FormatColor, FormatItem, format_as_escapes};
use wezterm_term::{Line, Progress};
use window::{IntegratedTitleButton, IntegratedTitleButtonAlignment, IntegratedTitleButtonStyle};

#[derive(Clone, Debug, PartialEq)]
pub struct TabBarState {
    line: Line,
    items: Vec<TabEntry>,
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
    x: usize,
    width: usize,
}

#[derive(Clone, Debug)]
struct TitleText {
    items: Vec<FormatItem>,
    len: usize,
}

/// Runs the `format-tab-title` event handler for the tabs of one tab bar.
///
/// The handler receives every tab, every pane and the whole config as Lua
/// tables. Converting those costs far more than a typical handler does, so
/// they are built once, on the first call, and shared by every tab of this
/// bar instead of being rebuilt for each tab. With no handler registered
/// nothing is converted at all.
struct TabTitleFormatter<'a> {
    tab_info: &'a [TabInformation],
    pane_info: &'a [PaneInformation],
    config: &'a ConfigHandle,
    state: FormatterState,
}

enum FormatterState {
    Unprepared,
    Unavailable,
    Ready(FormatTabTitleArgs),
}

struct FormatTabTitleArgs {
    lua: Rc<mlua::Lua>,
    handler: mlua::Function,
    tabs: mlua::Table,
    panes: mlua::Table,
    config: mlua::Value,
}

impl FormatTabTitleArgs {
    fn build(
        lua: Rc<mlua::Lua>,
        tab_info: &[TabInformation],
        pane_info: &[PaneInformation],
        config: &ConfigHandle,
    ) -> mlua::Result<Option<Self>> {
        let Some(handler) = config::lua::sync_callback_handler(&lua, "format-tab-title")? else {
            return Ok(None);
        };
        let tabs = lua.create_sequence_from(tab_info.iter().cloned())?;
        let panes = lua.create_sequence_from(pane_info.iter().cloned())?;
        let config = (**config).clone().into_lua(&lua)?;
        Ok(Some(Self {
            lua,
            handler,
            tabs,
            panes,
            config,
        }))
    }
}

impl<'a> TabTitleFormatter<'a> {
    fn new(
        tab_info: &'a [TabInformation],
        pane_info: &'a [PaneInformation],
        config: &'a ConfigHandle,
    ) -> Self {
        Self {
            tab_info,
            pane_info,
            config,
            state: FormatterState::Unprepared,
        }
    }

    fn prepare(&self) -> anyhow::Result<Option<FormatTabTitleArgs>> {
        config::run_immediate_with_lua_config(|lua| match lua {
            Some(lua) => Ok(FormatTabTitleArgs::build(
                lua,
                self.tab_info,
                self.pane_info,
                self.config,
            )?),
            None => Ok(None),
        })
    }

    fn call(
        &mut self,
        tab: &TabInformation,
        hover: bool,
        tab_max_width: usize,
    ) -> Option<TitleText> {
        if matches!(self.state, FormatterState::Unprepared) {
            self.state = match self.prepare() {
                Ok(Some(args)) => FormatterState::Ready(args),
                Ok(None) => FormatterState::Unavailable,
                Err(err) => {
                    log::warn!("format-tab-title: {}", err);
                    FormatterState::Unavailable
                }
            };
        }
        let FormatterState::Ready(args) = &self.state else {
            return None;
        };
        match Self::call_handler(args, tab, hover, tab_max_width) {
            Ok(title) => title,
            Err(err) => {
                log::warn!("format-tab-title: {}", err);
                None
            }
        }
    }

    fn call_handler(
        args: &FormatTabTitleArgs,
        tab: &TabInformation,
        hover: bool,
        tab_max_width: usize,
    ) -> anyhow::Result<Option<TitleText>> {
        let lua = &*args.lua;
        let v: mlua::Value = args.handler.call((
            tab.clone(),
            args.tabs.clone(),
            args.panes.clone(),
            args.config.clone(),
            hover,
            tab_max_width,
        ))?;
        match &v {
            mlua::Value::Nil => Ok(None),
            mlua::Value::Table(_) => {
                let items = <Vec<FormatItem>>::from_lua(v, lua)?;

                let esc = format_as_escapes(items.clone())?;
                let line = parse_status_text(&esc, CellAttributes::default());

                Ok(Some(TitleText {
                    items,
                    len: line.len(),
                }))
            }
            _ => {
                let s = String::from_lua(v, lua)?;
                let line = parse_status_text(&s, CellAttributes::default());
                Ok(Some(TitleText {
                    len: line.len(),
                    items: vec![FormatItem::Text(s)],
                }))
            }
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
    formatter: &mut TabTitleFormatter,
    tab: &TabInformation,
    config: &ConfigHandle,
    hover: bool,
    tab_max_width: usize,
) -> TitleText {
    metrics::histogram!("compute_tab_title.calls").record(1.);
    let title = formatter.call(tab, hover, tab_max_width);

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

impl TabBarState {
    pub fn default() -> Self {
        Self {
            line: Line::with_width(1, SEQ_ZERO),
            items: vec![TabEntry {
                item: TabBarItem::None,
                title: Line::from_text(" ", &CellAttributes::blank(), 1, None),
                x: 1,
                width: 1,
            }],
        }
    }

    pub fn line(&self) -> &Line {
        &self.line
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
            items.push(TabEntry {
                item: TabBarItem::WindowButton(*button),
                title: title.to_owned(),
                x: *x,
                width,
            });

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
        let mut formatter = TabTitleFormatter::new(tab_info, pane_info, config);

        let tab_titles: Vec<TitleText> = if config.show_tabs_in_tab_bar {
            tab_info
                .iter()
                .map(|tab| {
                    if tab.is_active {
                        active_tab_no = tab.tab_index;
                    }
                    compute_tab_title(&mut formatter, tab, config, false, config.tab_max_width)
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
            && config.tab_bar_at_bottom == false
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
            items.push(TabEntry {
                item: TabBarItem::LeftStatus,
                title: left_status_line.clone(),
                x,
                width: left_status_line.len(),
            });
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
                    &mut formatter,
                    &tab_info[tab_idx],
                    config,
                    hover,
                    tab_title_len,
                );
                &recomputed_title
            } else {
                metrics::histogram!("compute_tab_title.second_pass.cached").record(1.);
                tab_title
            };

            let cell_attrs = if active {
                &active_cell_attrs
            } else if hover {
                &inactive_hover_attrs
            } else {
                &inactive_cell_attrs
            };

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
                item: TabBarItem::Tab { tab_idx, active },
                title,
                x: tab_start_idx,
                width,
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

            items.push(TabEntry {
                item: TabBarItem::NewTabButton,
                title: new_tab_button.clone(),
                x: button_start,
                width,
            });

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
        items.push(TabEntry {
            item: TabBarItem::RightStatus,
            title: right_status_line.clone(),
            x,
            width: status_space_available,
        });

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

        Self { line, items }
    }

    pub fn compute_ui_items(&self, y: usize, cell_height: usize, cell_width: usize) -> Vec<UIItem> {
        let mut items = vec![];

        for entry in self.items.iter() {
            items.push(UIItem {
                x: entry.x * cell_width,
                width: entry.width * cell_width,
                y,
                height: cell_height,
                item_type: UIItemType::TabBar(entry.item),
            });
        }

        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tabs(count: usize) -> Vec<TabInformation> {
        (0..count)
            .map(|idx| TabInformation {
                tab_id: idx,
                tab_index: idx,
                is_active: idx == 0,
                is_last_active: false,
                active_pane: None,
                window_id: 0,
                tab_title: String::new(),
            })
            .collect()
    }

    fn lua_with_script(script: &str) -> anyhow::Result<Rc<mlua::Lua>> {
        let lua = config::lua::make_lua_context(std::path::Path::new("testing"))?;
        promise::spawn::block_on(lua.load(script).exec_async())?;
        Ok(Rc::new(lua))
    }

    fn title_text(title: Option<TitleText>) -> String {
        match title.expect("handler returned a title").items.as_slice() {
            [FormatItem::Text(text)] => text.clone(),
            other => panic!("unexpected title items {other:?}"),
        }
    }

    #[test]
    fn format_tab_title_args_are_built_once_and_shared_by_every_tab() -> anyhow::Result<()> {
        config::use_test_configuration();
        let config = config::configuration();
        let lua = lua_with_script(
            r#"
local wezterm = require 'wezterm'
seen = { calls = 0, same = true }
wezterm.on('format-tab-title', function(tab, tabs, panes, config, hover, max_width)
  seen.calls = seen.calls + 1
  if seen.tabs == nil then
    seen.tabs, seen.panes, seen.config = tabs, panes, config
  end
  seen.same = seen.same and rawequal(seen.tabs, tabs)
    and rawequal(seen.panes, panes) and rawequal(seen.config, config)
  return 'T' .. tab.tab_index .. '/' .. #tabs .. (hover and 'h' or '') .. max_width
end)
"#,
        )?;
        let tabs = test_tabs(3);
        let args = FormatTabTitleArgs::build(lua.clone(), &tabs, &[], &config)?
            .expect("a format-tab-title handler is registered");
        let mut formatter = TabTitleFormatter {
            tab_info: &tabs,
            pane_info: &[],
            config: &config,
            state: FormatterState::Ready(args),
        };

        let titles: Vec<String> = tabs
            .iter()
            .map(|tab| title_text(formatter.call(tab, false, 32)))
            .collect();
        assert_eq!(titles, ["T0/332", "T1/332", "T2/332"]);
        assert_eq!(title_text(formatter.call(&tabs[1], true, 10)), "T1/3h10");

        let seen: mlua::Table = lua.globals().get("seen")?;
        assert_eq!(seen.get::<i64>("calls")?, 4);
        assert!(
            seen.get::<bool>("same")?,
            "every call must get the same tabs, panes and config tables"
        );
        Ok(())
    }

    #[test]
    fn format_tab_title_args_are_not_built_without_a_handler() -> anyhow::Result<()> {
        config::use_test_configuration();
        let config = config::configuration();
        let lua = lua_with_script("local wezterm = require 'wezterm'")?;
        assert!(FormatTabTitleArgs::build(lua, &test_tabs(2), &[], &config)?.is_none());
        Ok(())
    }

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
}
