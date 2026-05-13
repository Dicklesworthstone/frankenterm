#![cfg_attr(not(feature = "headless-render"), allow(dead_code))]
// Keep this in sync with Cargo.toml: the vendored GUI crate is not yet a
// pedantic-clean primary lint target.
#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

pub mod accessibility_preferences;
pub mod adaptive_fps_loop;
pub mod floating_panes;
pub mod gpu_regression;
pub mod gpu_regression_fuzz;
pub mod input_loop;
pub mod osc8_gui;
pub mod plugins;
pub mod renderer_slo;
pub mod rollout_env;
pub mod smart_selection_a11y;
pub mod status_bar;
pub mod triple_buffer_gui;

#[cfg(any(feature = "debug-cell-crc", test))]
pub mod cell_crc;

#[cfg(feature = "headless-render")]
pub mod headless_render;
