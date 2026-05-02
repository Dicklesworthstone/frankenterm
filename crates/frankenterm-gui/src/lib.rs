#![cfg_attr(not(feature = "headless-render"), allow(dead_code))]

pub mod accessibility_preferences;
pub mod adaptive_fps_loop;
pub mod floating_panes;
pub mod gpu_regression;
pub mod gpu_regression_fuzz;
pub mod input_loop;
pub mod osc8_gui;
pub mod plugins;
pub mod rollout_env;
pub mod smart_selection_a11y;
pub mod status_bar;
pub mod triple_buffer_gui;

#[cfg(any(feature = "debug-cell-crc", test))]
pub mod cell_crc;

#[cfg(feature = "headless-render")]
pub mod headless_render;
