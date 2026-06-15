use crate::TermWindow;
use crate::termwindow::DimensionContext;
use crate::termwindow::box_model::*;
use crate::termwindow::modal::Modal;
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use config::Dimension;
use config::keyassignment::{KeyAssignment, PaneSelectArguments, PaneSelectMode};
use mux::Mux;
use std::cell::{Ref, RefCell};
use std::fmt::Display;
use wezterm_term::{KeyCode, KeyModifiers, MouseEvent};

pub struct PaneSelector {
    element: RefCell<Option<Vec<ComputedElement>>>,
    labels: RefCell<Vec<PaneSelectLabel>>,
    selection: RefCell<String>,
    alphabet: String,
    mode: PaneSelectMode,
    was_zoomed: bool,
    show_pane_ids: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PaneSelectLabel {
    label: String,
    pane_index: usize,
}

impl PaneSelectLabel {
    fn caption(&self, pane_id: impl Display, show_pane_ids: bool) -> String {
        if show_pane_ids {
            format!("{}: {pane_id}", self.label)
        } else {
            self.label.clone()
        }
    }
}

fn pane_select_label_for_pane(labels: &[String], pane_index: usize) -> Option<PaneSelectLabel> {
    labels.get(pane_index).map(|label| PaneSelectLabel {
        label: label.clone(),
        pane_index,
    })
}

impl PaneSelector {
    pub fn new(term_window: &mut TermWindow, args: &PaneSelectArguments) -> Self {
        let alphabet = if args.alphabet.is_empty() {
            term_window.config.quick_select_alphabet.clone()
        } else {
            args.alphabet.clone()
        };

        // Ensure that we are un-zoomed and remember the original state
        let was_zoomed = {
            Mux::try_get()
                .and_then(|mux| mux.get_active_tab_for_window(term_window.mux_window_id))
                .map(|tab| tab.set_zoomed(false))
                .unwrap_or(false)
        };

        Self {
            element: RefCell::new(None),
            labels: RefCell::new(vec![]),
            selection: RefCell::new(String::new()),
            alphabet,
            mode: args.mode,
            was_zoomed,
            show_pane_ids: args.show_pane_ids,
        }
    }

    fn compute(
        term_window: &mut TermWindow,
        alphabet: &str,
        show_pane_ids: bool,
    ) -> anyhow::Result<(Vec<ComputedElement>, Vec<PaneSelectLabel>)> {
        let font = term_window
            .fonts
            .pane_select_font()
            .context("failed to resolve pane selection font")?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());

        let top_bar_height = if term_window.show_tab_bar && !term_window.config.tab_bar_at_bottom {
            term_window
                .tab_bar_pixel_height()
                .context("failed to compute tab bar height")?
        } else {
            0.
        };
        let (padding_left, padding_top) = term_window.padding_left_top();
        let border = term_window.get_os_border();
        let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;

        let panes = term_window.get_panes_to_render();
        let labels =
            crate::overlay::quickselect::compute_labels_for_alphabet(alphabet, panes.len());

        let mut elements = vec![];
        let mut pane_labels = vec![];
        for pos in panes {
            let Some(label) = pane_select_label_for_pane(&labels, pos.index) else {
                log::warn!(
                    "pane select skipped pane index {} because quick-select alphabet produced only {} labels",
                    pos.index,
                    labels.len()
                );
                continue;
            };
            let caption = label.caption(pos.pane.pane_id(), show_pane_ids);
            let element = Element::new(&font, ElementContent::Text(caption))
                .colors(ElementColors {
                    border: BorderColor::new(
                        term_window.config.pane_select_bg_color.to_linear().into(),
                    ),
                    bg: term_window.config.pane_select_bg_color.to_linear().into(),
                    text: term_window.config.pane_select_fg_color.to_linear().into(),
                })
                .padding(BoxDimension {
                    left: Dimension::Cells(0.25),
                    right: Dimension::Cells(0.25),
                    top: Dimension::Cells(0.),
                    bottom: Dimension::Cells(0.),
                })
                .border(BoxDimension::new(Dimension::Pixels(1.)))
                .border_corners(Some(Corners {
                    top_left: SizedPoly {
                        width: Dimension::Cells(0.25),
                        height: Dimension::Cells(0.25),
                        poly: TOP_LEFT_ROUNDED_CORNER,
                    },
                    top_right: SizedPoly {
                        width: Dimension::Cells(0.25),
                        height: Dimension::Cells(0.25),
                        poly: TOP_RIGHT_ROUNDED_CORNER,
                    },
                    bottom_left: SizedPoly {
                        width: Dimension::Cells(0.25),
                        height: Dimension::Cells(0.25),
                        poly: BOTTOM_LEFT_ROUNDED_CORNER,
                    },
                    bottom_right: SizedPoly {
                        width: Dimension::Cells(0.25),
                        height: Dimension::Cells(0.25),
                        poly: BOTTOM_RIGHT_ROUNDED_CORNER,
                    },
                }));

            let dimensions = term_window.dimensions;
            let pane_dims = pos.pane.get_dimensions();

            let computed = term_window.compute_element(
                &LayoutContext {
                    height: DimensionContext {
                        dpi: dimensions.dpi as f32,
                        pixel_max: dimensions.pixel_height as f32,
                        pixel_cell: metrics.cell_size.height as f32,
                    },
                    width: DimensionContext {
                        dpi: dimensions.dpi as f32,
                        pixel_max: dimensions.pixel_width as f32,
                        pixel_cell: metrics.cell_size.width as f32,
                    },
                    bounds: euclid::rect(
                        padding_left
                            + ((pos.left as f32 + pane_dims.cols as f32 / 2.)
                                * term_window.render_metrics.cell_size.width as f32),
                        top_pixel_y
                            + ((pos.top as f32 + pane_dims.viewport_rows as f32 / 2.)
                                * term_window.render_metrics.cell_size.height as f32),
                        pane_dims.cols as f32 * term_window.render_metrics.cell_size.width as f32,
                        pane_dims.viewport_rows as f32
                            * term_window.render_metrics.cell_size.height as f32,
                    ),
                    metrics: &metrics,
                    gl_state: term_window
                        .render_state
                        .as_ref()
                        .context("render state is not initialized")?,
                    zindex: 100,
                },
                &element,
            )?;
            elements.push(computed);
            pane_labels.push(label);
        }

        Ok((elements, pane_labels))
    }

    fn perform_selection(
        &self,
        pane_index: usize,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<()> {
        let Some(mux) = Mux::try_get() else {
            return Ok(());
        };
        let tab = match mux.get_active_tab_for_window(term_window.mux_window_id) {
            Some(tab) => tab,
            None => return Ok(()),
        };

        let tab_id = tab.tab_id();

        if term_window.tab_state(tab_id).overlay.is_none() {
            let panes = tab.iter_panes();

            match self.mode {
                PaneSelectMode::Activate => {
                    if panes.iter().position(|p| p.index == pane_index).is_some() {
                        tab.set_active_idx(pane_index);
                    }
                }
                PaneSelectMode::SwapWithActiveKeepFocus | PaneSelectMode::SwapWithActive => {
                    tab.swap_active_with_index(
                        pane_index,
                        self.mode == PaneSelectMode::SwapWithActiveKeepFocus,
                    );
                }
                PaneSelectMode::MoveToNewWindow => {
                    if let Some(pos) = panes.iter().find(|p| p.index == pane_index) {
                        let pane_id = pos.pane.pane_id();
                        promise::spawn::spawn(async move {
                            if let Err(err) = mux.move_pane_to_new_tab(pane_id, None, None).await {
                                log::error!("failed to move_pane_to_new_tab: {err:#}");
                            }
                        })
                        .detach();
                    }
                }
                PaneSelectMode::MoveToNewTab => {
                    if let Some(pos) = panes.iter().find(|p| p.index == pane_index) {
                        let pane_id = pos.pane.pane_id();
                        let window_id = term_window.mux_window_id;
                        promise::spawn::spawn(async move {
                            if let Err(err) = mux
                                .move_pane_to_new_tab(pane_id, Some(window_id), None)
                                .await
                            {
                                log::error!("failed to move_pane_to_new_tab: {err:#}");
                            }

                            mux.focus_pane_and_containing_tab(pane_id).ok();
                        })
                        .detach();
                    }
                }
            }
        }

        if self.was_zoomed {
            tab.set_zoomed(true);
        }

        term_window.cancel_modal();
        Ok(())
    }
}

impl Modal for PaneSelector {
    fn perform_assignment(
        &self,
        _assignment: &KeyAssignment,
        _term_window: &mut TermWindow,
    ) -> bool {
        false
    }

    fn mouse_event(&self, _event: MouseEvent, _term_window: &mut TermWindow) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<bool> {
        match (key, mods) {
            (KeyCode::Escape, KeyModifiers::NONE) | (KeyCode::Char('g'), KeyModifiers::CTRL) => {
                term_window.cancel_modal();
            }
            (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                // Type to add to the selection
                let mut selection = self.selection.borrow_mut();
                selection.push(c);

                // and if we have a complete match, activate that pane
                let pane_index = self
                    .labels
                    .borrow()
                    .iter()
                    .find(|label| label.label == *selection)
                    .map(|label| label.pane_index);
                if let Some(pane_index) = pane_index {
                    self.perform_selection(pane_index, term_window)?;
                    return Ok(true);
                }
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => {
                // Backspace to edit the selection
                let mut selection = self.selection.borrow_mut();
                selection.pop();
            }
            (KeyCode::Char('u'), KeyModifiers::CTRL) => {
                // CTRL-u to clear the selection
                let mut selection = self.selection.borrow_mut();
                selection.clear();
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn computed_element(
        &self,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<Ref<'_, [ComputedElement]>> {
        if self.element.borrow().is_none() {
            let (element, labels) = Self::compute(term_window, &self.alphabet, self.show_pane_ids)?;
            self.element.borrow_mut().replace(element);
            *self.labels.borrow_mut() = labels;
        }
        Ok(Ref::map(self.element.borrow(), |v| {
            v.as_deref().unwrap_or(&[])
        }))
    }

    fn reconfigure(&self, _term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::quickselect::compute_labels_for_alphabet;

    #[test]
    fn pane_select_empty_alphabet_has_no_selectable_labels() {
        let labels = compute_labels_for_alphabet("", 2);

        assert!(labels.is_empty());
        assert!(pane_select_label_for_pane(&labels, 0).is_none());
        assert!(pane_select_label_for_pane(&labels, 1).is_none());
    }

    #[test]
    fn pane_select_short_alphabet_skips_panes_without_labels() {
        let labels = compute_labels_for_alphabet("ab", 5);

        assert_eq!(labels, vec!["aa", "ab", "ba", "bb"]);
        assert_eq!(
            pane_select_label_for_pane(&labels, 3),
            Some(PaneSelectLabel {
                label: "bb".to_string(),
                pane_index: 3,
            })
        );
        assert!(pane_select_label_for_pane(&labels, 4).is_none());
    }

    #[test]
    fn pane_select_caption_optionally_includes_pane_id() {
        let label = PaneSelectLabel {
            label: "aa".to_string(),
            pane_index: 0,
        };

        assert_eq!(label.caption(42, false), "aa");
        assert_eq!(label.caption(42, true), "aa: 42");
    }
}
