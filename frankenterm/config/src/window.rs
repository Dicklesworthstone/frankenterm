use frankenterm_dynamic::{FromDynamic, ToDynamic};

#[derive(Debug, Default, Clone, ToDynamic, PartialEq, Eq, FromDynamic)]
pub enum WindowLevel {
    AlwaysOnBottom = -1,
    #[default]
    Normal = 0,
    AlwaysOnTop = 3,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_window_level_is_normal() {
        assert_eq!(WindowLevel::default(), WindowLevel::Normal);
    }

    #[test]
    fn window_level_discriminants_are_stable() {
        assert_eq!(WindowLevel::AlwaysOnBottom as i32, -1);
        assert_eq!(WindowLevel::Normal as i32, 0);
        assert_eq!(WindowLevel::AlwaysOnTop as i32, 3);
    }

    #[test]
    fn window_level_equality() {
        assert_eq!(WindowLevel::Normal, WindowLevel::Normal);
        assert_ne!(WindowLevel::Normal, WindowLevel::AlwaysOnTop);
        assert_ne!(WindowLevel::AlwaysOnBottom, WindowLevel::AlwaysOnTop);
    }

    #[test]
    fn window_level_clone() {
        let level = WindowLevel::AlwaysOnTop;
        let cloned = level.clone();
        assert_eq!(level, cloned);
    }
}
