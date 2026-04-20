use window::parameters::Border;
use window::ResizeIncrement;

pub struct ResizeIncrementCalculator {
    pub x: u16,
    pub y: u16,
    pub padding_left: usize,
    pub padding_top: usize,
    pub padding_right: usize,
    pub padding_bottom: usize,
    pub border: Border,
    pub tab_bar_height: usize,
}

fn saturating_resize_base(value: usize) -> u16 {
    value.min(usize::from(u16::MAX)) as u16
}

impl Into<ResizeIncrement> for ResizeIncrementCalculator {
    fn into(self) -> ResizeIncrement {
        let base_width = self
            .padding_left
            .saturating_add(self.padding_right)
            .saturating_add((self.border.left + self.border.right).get());
        let base_height = self
            .padding_top
            .saturating_add(self.padding_bottom)
            .saturating_add((self.border.top + self.border.bottom).get())
            .saturating_add(self.tab_bar_height);
        ResizeIncrement {
            x: self.x,
            y: self.y,
            base_width: saturating_resize_base(base_width),
            base_height: saturating_resize_base(base_height),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use window::color::LinearRgba;
    use window::parameters::Border;
    use window::ULength;

    fn test_border(top: usize, left: usize, bottom: usize, right: usize) -> Border {
        Border {
            top: ULength::new(top),
            left: ULength::new(left),
            bottom: ULength::new(bottom),
            right: ULength::new(right),
            color: LinearRgba::default(),
        }
    }

    proptest! {
        #[test]
        fn resize_increment_conversion_preserves_steps_and_base_geometry(
            x in 1u16..=u16::MAX,
            y in 1u16..=u16::MAX,
            padding_left in 0usize..120_000,
            padding_top in 0usize..120_000,
            padding_right in 0usize..120_000,
            padding_bottom in 0usize..120_000,
            border_top in 0usize..40_000,
            border_left in 0usize..40_000,
            border_bottom in 0usize..40_000,
            border_right in 0usize..40_000,
            tab_bar_height in 0usize..80_000,
        ) {
            let expected_base_width = padding_left
                .saturating_add(padding_right)
                .saturating_add(border_left)
                .saturating_add(border_right)
                .min(usize::from(u16::MAX));
            let expected_base_height = padding_top
                .saturating_add(padding_bottom)
                .saturating_add(border_top)
                .saturating_add(border_bottom)
                .saturating_add(tab_bar_height)
                .min(usize::from(u16::MAX));

            let increment: ResizeIncrement = ResizeIncrementCalculator {
                x,
                y,
                padding_left,
                padding_top,
                padding_right,
                padding_bottom,
                border: test_border(border_top, border_left, border_bottom, border_right),
                tab_bar_height,
            }
            .into();

            prop_assert_eq!(increment.x, x);
            prop_assert_eq!(increment.y, y);
            prop_assert_eq!(usize::from(increment.base_width), expected_base_width);
            prop_assert_eq!(usize::from(increment.base_height), expected_base_height);
        }
    }

    #[test]
    fn resize_increment_conversion_includes_tab_bar_in_base_height() {
        let increment: ResizeIncrement = ResizeIncrementCalculator {
            x: 9,
            y: 18,
            padding_left: 4,
            padding_top: 5,
            padding_right: 6,
            padding_bottom: 7,
            border: test_border(8, 3, 2, 1),
            tab_bar_height: 11,
        }
        .into();

        assert_eq!(increment.x, 9);
        assert_eq!(increment.y, 18);
        assert_eq!(increment.base_width, 14);
        assert_eq!(increment.base_height, 33);
    }

    #[test]
    fn resize_increment_conversion_saturates_large_base_geometry() {
        let increment: ResizeIncrement = ResizeIncrementCalculator {
            x: 9,
            y: 18,
            padding_left: 50_000,
            padding_top: 48_000,
            padding_right: 40_000,
            padding_bottom: 39_000,
            border: test_border(2_000, 3_000, 4_000, 5_000),
            tab_bar_height: 12_000,
        }
        .into();

        assert_eq!(increment.x, 9);
        assert_eq!(increment.y, 18);
        assert_eq!(increment.base_width, u16::MAX);
        assert_eq!(increment.base_height, u16::MAX);
    }
}
