use crate::{default_one_point_oh, Config, Dimension, HsbTransform, PixelUnit, RgbaColor};
use frankenterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, Value};
#[cfg(feature = "lua")]
use luahelper::impl_lua_conversion_dynamic;
use std::convert::TryFrom;
use termwiz::color::SrgbaTuple;

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct ImageFileSource {
    pub path: String,

    /// Adjust the animation rate for animated images
    #[dynamic(default = "default_one_point_oh")]
    pub speed: f32,
}

#[derive(Debug, Clone, ToDynamic)]
pub struct ImageFileSourceWrap {
    #[dynamic(flatten)]
    inner: ImageFileSource,
}

impl std::ops::Deref for ImageFileSourceWrap {
    type Target = ImageFileSource;
    fn deref(&self) -> &ImageFileSource {
        &self.inner
    }
}

impl FromDynamic for ImageFileSourceWrap {
    fn from_dynamic(
        value: &Value,
        options: FromDynamicOptions,
    ) -> Result<Self, frankenterm_dynamic::Error> {
        match value {
            Value::String(path) => Ok(Self {
                inner: ImageFileSource {
                    path: path.to_string(),
                    speed: 1.0,
                },
            }),
            _ => {
                let inner = ImageFileSource::from_dynamic(value, options)?;
                Ok(Self { inner })
            }
        }
    }
}

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub enum BackgroundSource {
    Gradient(Gradient),
    File(ImageFileSourceWrap),
    Color(RgbaColor),
}

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct BackgroundLayer {
    pub source: BackgroundSource,

    /// Where the top left corner of the background begins
    #[dynamic(default)]
    pub origin: BackgroundOrigin,

    #[dynamic(default)]
    pub attachment: BackgroundAttachment,

    #[dynamic(default)]
    pub repeat_x: BackgroundRepeat,
    #[dynamic(try_from = "crate::units::OptPixelUnit", default)]
    pub repeat_x_size: Option<Dimension>,

    #[dynamic(default)]
    pub repeat_y: BackgroundRepeat,
    #[dynamic(try_from = "crate::units::OptPixelUnit", default)]
    pub repeat_y_size: Option<Dimension>,

    #[dynamic(default)]
    pub vertical_align: BackgroundVerticalAlignment,
    #[dynamic(try_from = "crate::units::OptPixelUnit", default)]
    pub vertical_offset: Option<Dimension>,

    #[dynamic(default)]
    pub horizontal_align: BackgroundHorizontalAlignment,
    #[dynamic(try_from = "crate::units::OptPixelUnit", default)]
    pub horizontal_offset: Option<Dimension>,

    /// Additional alpha modifier
    #[dynamic(default = "default_one_point_oh")]
    pub opacity: f32,

    /// Additional hsb transform
    #[dynamic(default)]
    pub hsb: HsbTransform,

    #[dynamic(default)]
    pub width: BackgroundSize,

    #[dynamic(default)]
    pub height: BackgroundSize,
}

impl BackgroundLayer {
    pub fn with_legacy(cfg: &Config) -> Option<Self> {
        let source = if let Some(gradient) = &cfg.window_background_gradient {
            BackgroundSource::Gradient(gradient.clone())
        } else if let Some(path) = &cfg.window_background_image {
            BackgroundSource::File(ImageFileSourceWrap {
                inner: ImageFileSource {
                    path: path.to_string_lossy().to_string(),
                    speed: 1.0,
                },
            })
        } else {
            return None;
        };
        Some(BackgroundLayer {
            source,
            opacity: cfg.window_background_opacity,
            hsb: cfg.window_background_image_hsb.unwrap_or_default(),
            origin: Default::default(),
            attachment: Default::default(),
            repeat_x: Default::default(),
            repeat_y: Default::default(),
            repeat_x_size: None,
            repeat_y_size: None,
            vertical_align: Default::default(),
            horizontal_align: Default::default(),
            vertical_offset: None,
            horizontal_offset: None,
            width: BackgroundSize::Dimension(Dimension::Percent(1.)),
            height: BackgroundSize::Dimension(Dimension::Percent(1.)),
        })
    }
}

/// <https://developer.mozilla.org/en-US/docs/Web/CSS/background-size>
#[derive(Debug, Copy, Clone)]
pub enum BackgroundSize {
    /// Scales image as large as possible without cropping or stretching.
    /// If the container is larger than the image, tiles the image unless
    /// the correspond `repeat` is NoRepeat.
    Contain,
    /// Scale the image (preserving aspect ratio) to the smallest possible
    /// size to the fill the container leaving no empty space.
    /// If the aspect ratio differs from the background, the image is
    /// cropped.
    Cover,
    /// Stretches the image to the specified length in pixels
    Dimension(Dimension),
}

impl FromDynamic for BackgroundSize {
    fn from_dynamic(
        value: &Value,
        options: FromDynamicOptions,
    ) -> Result<Self, frankenterm_dynamic::Error> {
        match value {
            Value::String(label) => match label.as_str() {
                "Contain" => return Ok(Self::Contain),
                "Cover" => return Ok(Self::Cover),
                _ => {}
            },
            _ => {}
        }
        match PixelUnit::from_dynamic(value, options) {
            Ok(pix) => Ok(Self::Dimension(pix.into())),
            Err(_) => Err(frankenterm_dynamic::Error::Message(format!(
                "expected either 'Contain', 'Cover', \
                        a number, or a string of \
                        the form '123px' where 'px' is a unit and \
                        can be one of 'px', '%', 'pt' or 'cell', \
                        but got {}",
                value.variant_name()
            ))),
        }
    }
}

impl ToDynamic for BackgroundSize {
    fn to_dynamic(&self) -> Value {
        let s = match self {
            Self::Cover => "Cover".to_string(),
            Self::Contain => "Contain".to_string(),
            Self::Dimension(d) => return d.to_dynamic(),
        };
        Value::String(s)
    }
}

impl Default for BackgroundSize {
    fn default() -> Self {
        Self::Cover
    }
}

#[derive(Debug, Copy, Clone, FromDynamic, ToDynamic)]
pub enum BackgroundHorizontalAlignment {
    Left,
    Center,
    Right,
}

impl Default for BackgroundHorizontalAlignment {
    fn default() -> Self {
        Self::Left
    }
}

#[derive(Debug, Copy, Clone, FromDynamic, ToDynamic)]
pub enum BackgroundVerticalAlignment {
    Top,
    Middle,
    Bottom,
}

impl Default for BackgroundVerticalAlignment {
    fn default() -> Self {
        Self::Top
    }
}

#[derive(Debug, Copy, Clone, FromDynamic, ToDynamic, PartialEq, Eq)]
pub enum BackgroundRepeat {
    /// Repeat as much as possible to cover the area.
    /// The last image will be clipped if it doesn't fit.
    Repeat,
    /// Like Repeat, except that the image is alternately
    /// mirrored. Helpful when the image doesn't seamlessly
    /// tile.
    Mirror,
    /*
    /// Repeat as much as possible without clipping.
    /// The first and last images are aligned with the edges,
    /// with any gaps being distributed evenly between
    /// the images.
    /// The `position` property is ignored unless only
    /// a single image an be displayed without clipping.
    /// Clipping will only occur when there isn't enough
    /// room to display a single image.
    Space,
    /// As the available space increases, the images will
    /// stretch until there is room (space >= 50% of image
    /// size) for another one to be added. When adding a
    /// new image, the current images compress to allow
    /// room.
    Round,
    */
    /// The image is not repeated.
    /// The position of the image is defined by the
    /// `position` property
    NoRepeat,
}

impl Default for BackgroundRepeat {
    fn default() -> Self {
        Self::Repeat
    }
}

#[derive(Debug, Copy, Clone, FromDynamic, ToDynamic)]
pub enum BackgroundAttachment {
    Fixed,
    Scroll,
    Parallax(f32),
}

impl BackgroundAttachment {
    pub fn scroll_factor(&self) -> Option<f32> {
        match self {
            Self::Fixed => None,
            Self::Scroll => Some(1.0),
            Self::Parallax(f) => Some(*f),
        }
    }
}

impl Default for BackgroundAttachment {
    fn default() -> Self {
        Self::Fixed
    }
}

#[derive(Debug, Copy, Clone, FromDynamic, ToDynamic)]
pub enum BackgroundOrigin {
    BorderBox,
    PaddingBox,
}

impl Default for BackgroundOrigin {
    fn default() -> Self {
        Self::BorderBox
    }
}

#[derive(Debug, Copy, Clone, FromDynamic, ToDynamic, PartialEq, Default)]
pub enum SystemBackdrop {
    #[default]
    Auto,
    Disable,
    Acrylic,
    Mica,
    Tabbed,
}

pub fn default_win32_acrylic_accent_color() -> RgbaColor {
    SrgbaTuple(0.156863, 0.156863, 0.156863, 0.003922).into()
}

#[derive(Debug, Copy, Clone, FromDynamic, ToDynamic, PartialEq, Default)]
pub enum Interpolation {
    #[default]
    Linear,
    Basis,
    CatmullRom,
}

#[derive(Debug, Copy, Clone, FromDynamic, ToDynamic, PartialEq, Default)]
pub enum BlendMode {
    #[default]
    Rgb,
    LinearRgb,
    Hsv,
    Oklab,
}

#[derive(Debug, Copy, Clone, FromDynamic, ToDynamic, PartialEq)]
pub enum GradientOrientation {
    Horizontal,
    Vertical,
    Linear {
        angle: Option<f64>,
    },
    Radial {
        radius: Option<f64>,
        cx: Option<f64>,
        cy: Option<f64>,
    },
}

impl Default for GradientOrientation {
    fn default() -> Self {
        Self::Horizontal
    }
}

#[derive(Debug, Copy, Clone, FromDynamic, ToDynamic, PartialEq)]
pub enum GradientPreset {
    Blues,
    BrBg,
    BuGn,
    BuPu,
    Cividis,
    Cool,
    CubeHelixDefault,
    GnBu,
    Greens,
    Greys,
    Inferno,
    Magma,
    OrRd,
    Oranges,
    PiYg,
    Plasma,
    PrGn,
    PuBu,
    PuBuGn,
    PuOr,
    PuRd,
    Purples,
    Rainbow,
    RdBu,
    RdGy,
    RdPu,
    RdYlBu,
    RdYlGn,
    Reds,
    Sinebow,
    Spectral,
    Turbo,
    Viridis,
    Warm,
    YlGn,
    YlGnBu,
    YlOrBr,
    YlOrRd,
}

impl GradientPreset {
    fn build(self) -> Box<dyn colorgrad::Gradient> {
        use colorgrad::Gradient as _;

        match self {
            Self::Blues => colorgrad::preset::blues().boxed(),
            Self::BrBg => colorgrad::preset::br_bg().boxed(),
            Self::BuGn => colorgrad::preset::bu_gn().boxed(),
            Self::BuPu => colorgrad::preset::bu_pu().boxed(),
            Self::Cividis => colorgrad::preset::cividis().boxed(),
            Self::Cool => colorgrad::preset::cool().boxed(),
            Self::CubeHelixDefault => colorgrad::preset::cubehelix_default().boxed(),
            Self::GnBu => colorgrad::preset::gn_bu().boxed(),
            Self::Greens => colorgrad::preset::greens().boxed(),
            Self::Greys => colorgrad::preset::greys().boxed(),
            Self::Inferno => colorgrad::preset::inferno().boxed(),
            Self::Magma => colorgrad::preset::magma().boxed(),
            Self::OrRd => colorgrad::preset::or_rd().boxed(),
            Self::Oranges => colorgrad::preset::oranges().boxed(),
            Self::PiYg => colorgrad::preset::pi_yg().boxed(),
            Self::Plasma => colorgrad::preset::plasma().boxed(),
            Self::PrGn => colorgrad::preset::pr_gn().boxed(),
            Self::PuBu => colorgrad::preset::pu_bu().boxed(),
            Self::PuBuGn => colorgrad::preset::pu_bu_gn().boxed(),
            Self::PuOr => colorgrad::preset::pu_or().boxed(),
            Self::PuRd => colorgrad::preset::pu_rd().boxed(),
            Self::Purples => colorgrad::preset::purples().boxed(),
            Self::Rainbow => colorgrad::preset::rainbow().boxed(),
            Self::RdBu => colorgrad::preset::rd_bu().boxed(),
            Self::RdGy => colorgrad::preset::rd_gy().boxed(),
            Self::RdPu => colorgrad::preset::rd_pu().boxed(),
            Self::RdYlBu => colorgrad::preset::rd_yl_bu().boxed(),
            Self::RdYlGn => colorgrad::preset::rd_yl_gn().boxed(),
            Self::Reds => colorgrad::preset::reds().boxed(),
            Self::Sinebow => colorgrad::preset::sinebow().boxed(),
            Self::Spectral => colorgrad::preset::spectral().boxed(),
            Self::Turbo => colorgrad::preset::turbo().boxed(),
            Self::Viridis => colorgrad::preset::viridis().boxed(),
            Self::Warm => colorgrad::preset::warm().boxed(),
            Self::YlGn => colorgrad::preset::yl_gn().boxed(),
            Self::YlGnBu => colorgrad::preset::yl_gn_bu().boxed(),
            Self::YlOrBr => colorgrad::preset::yl_or_br().boxed(),
            Self::YlOrRd => colorgrad::preset::yl_or_rd().boxed(),
        }
    }
}

#[derive(Debug, Clone, FromDynamic, ToDynamic, PartialEq)]
pub struct Gradient {
    #[dynamic(default)]
    pub orientation: GradientOrientation,

    #[dynamic(default)]
    pub colors: Vec<String>,

    #[dynamic(default)]
    pub preset: Option<GradientPreset>,

    #[dynamic(default)]
    pub interpolation: Interpolation,

    #[dynamic(default)]
    pub blend: BlendMode,

    #[dynamic(default)]
    pub segment_size: Option<usize>,

    #[dynamic(default)]
    pub segment_smoothness: Option<f64>,

    #[dynamic(default)]
    pub noise: Option<usize>,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(Gradient);

impl Gradient {
    pub fn build(&self) -> anyhow::Result<Box<dyn colorgrad::Gradient>> {
        use colorgrad::{BlendMode as CGMode, Gradient as _};

        let g = match &self.preset {
            Some(p) => p.build(),
            None => {
                let colors: Vec<&str> = self.colors.iter().map(|s| s.as_str()).collect();
                let mut builder = colorgrad::GradientBuilder::new();
                builder.html_colors(&colors);
                if self.blend == BlendMode::Hsv {
                    anyhow::bail!("HSV gradient interpolation is not supported by colorgrad 0.8");
                }
                builder.mode(match self.blend {
                    BlendMode::Rgb => CGMode::Rgb,
                    BlendMode::LinearRgb => CGMode::LinearRgb,
                    BlendMode::Oklab => CGMode::Oklab,
                    BlendMode::Hsv => unreachable!("checked above"),
                });
                match self.interpolation {
                    Interpolation::Linear => builder.build::<colorgrad::LinearGradient>()?.boxed(),
                    Interpolation::Basis => builder.build::<colorgrad::BasisGradient>()?.boxed(),
                    Interpolation::CatmullRom => {
                        builder.build::<colorgrad::CatmullRomGradient>()?.boxed()
                    }
                }
            }
        };
        match (self.segment_size, self.segment_smoothness) {
            (Some(size), Some(smoothness)) => {
                anyhow::ensure!(smoothness.is_finite(), "gradient smoothness must be finite");
                let size = u16::try_from(size)
                    .map_err(|_| anyhow::anyhow!("gradient segment_size exceeds u16 range"))?;
                Ok(g.sharp(size, smoothness as f32).boxed())
            }
            (None, None) => Ok(g),
            _ => anyhow::bail!(
                "Gradient must either specify both segment_size and segment_smoothness, or neither"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- BackgroundSize ---

    #[test]
    fn background_size_default_is_cover() {
        match BackgroundSize::default() {
            BackgroundSize::Cover => {}
            other => panic!("expected Cover, got {:?}", other),
        }
    }

    #[test]
    fn background_size_debug() {
        let s = format!("{:?}", BackgroundSize::Contain);
        assert!(s.contains("Contain"));
        let s = format!("{:?}", BackgroundSize::Cover);
        assert!(s.contains("Cover"));
    }

    #[test]
    fn background_size_clone() {
        let a = BackgroundSize::Contain;
        let b = a;
        let _ = format!("{:?}", b);
    }

    // --- BackgroundHorizontalAlignment ---

    #[test]
    fn bg_horizontal_default_is_left() {
        match BackgroundHorizontalAlignment::default() {
            BackgroundHorizontalAlignment::Left => {}
            other => panic!("expected Left, got {:?}", other),
        }
    }

    #[test]
    fn bg_horizontal_debug() {
        assert!(format!("{:?}", BackgroundHorizontalAlignment::Center).contains("Center"));
        assert!(format!("{:?}", BackgroundHorizontalAlignment::Right).contains("Right"));
    }

    // --- BackgroundVerticalAlignment ---

    #[test]
    fn bg_vertical_default_is_top() {
        match BackgroundVerticalAlignment::default() {
            BackgroundVerticalAlignment::Top => {}
            other => panic!("expected Top, got {:?}", other),
        }
    }

    #[test]
    fn bg_vertical_debug() {
        assert!(format!("{:?}", BackgroundVerticalAlignment::Middle).contains("Middle"));
        assert!(format!("{:?}", BackgroundVerticalAlignment::Bottom).contains("Bottom"));
    }

    // --- BackgroundRepeat ---

    #[test]
    fn bg_repeat_default_is_repeat() {
        assert_eq!(BackgroundRepeat::default(), BackgroundRepeat::Repeat);
    }

    #[test]
    fn bg_repeat_equality() {
        assert_eq!(BackgroundRepeat::Repeat, BackgroundRepeat::Repeat);
        assert_ne!(BackgroundRepeat::Repeat, BackgroundRepeat::Mirror);
        assert_ne!(BackgroundRepeat::Mirror, BackgroundRepeat::NoRepeat);
    }

    // --- BackgroundAttachment ---

    #[test]
    fn bg_attachment_default_is_fixed() {
        match BackgroundAttachment::default() {
            BackgroundAttachment::Fixed => {}
            other => panic!("expected Fixed, got {:?}", other),
        }
    }

    #[test]
    fn bg_attachment_scroll_factor_fixed() {
        assert_eq!(BackgroundAttachment::Fixed.scroll_factor(), None);
    }

    #[test]
    fn bg_attachment_scroll_factor_scroll() {
        assert_eq!(BackgroundAttachment::Scroll.scroll_factor(), Some(1.0));
    }

    #[test]
    fn bg_attachment_scroll_factor_parallax() {
        assert_eq!(
            BackgroundAttachment::Parallax(0.5).scroll_factor(),
            Some(0.5)
        );
    }

    // --- BackgroundOrigin ---

    #[test]
    fn bg_origin_default_is_border_box() {
        match BackgroundOrigin::default() {
            BackgroundOrigin::BorderBox => {}
            other => panic!("expected BorderBox, got {:?}", other),
        }
    }

    #[test]
    fn bg_origin_debug() {
        assert!(format!("{:?}", BackgroundOrigin::PaddingBox).contains("PaddingBox"));
    }

    // --- SystemBackdrop ---

    #[test]
    fn system_backdrop_default_is_auto() {
        assert_eq!(SystemBackdrop::default(), SystemBackdrop::Auto);
    }

    #[test]
    fn system_backdrop_equality() {
        assert_eq!(SystemBackdrop::Auto, SystemBackdrop::Auto);
        assert_ne!(SystemBackdrop::Auto, SystemBackdrop::Disable);
        assert_ne!(SystemBackdrop::Acrylic, SystemBackdrop::Mica);
        assert_ne!(SystemBackdrop::Mica, SystemBackdrop::Tabbed);
    }

    // --- Interpolation ---

    #[test]
    fn interpolation_default_is_linear() {
        assert_eq!(Interpolation::default(), Interpolation::Linear);
    }

    #[test]
    fn interpolation_equality() {
        assert_eq!(Interpolation::Linear, Interpolation::Linear);
        assert_ne!(Interpolation::Linear, Interpolation::Basis);
        assert_ne!(Interpolation::Basis, Interpolation::CatmullRom);
    }

    // --- BlendMode ---

    #[test]
    fn blend_mode_default_is_rgb() {
        assert_eq!(BlendMode::default(), BlendMode::Rgb);
    }

    #[test]
    fn blend_mode_equality() {
        assert_eq!(BlendMode::Rgb, BlendMode::Rgb);
        assert_ne!(BlendMode::Rgb, BlendMode::LinearRgb);
        assert_ne!(BlendMode::Hsv, BlendMode::Oklab);
    }

    // --- GradientOrientation ---

    #[test]
    fn gradient_orientation_default_is_horizontal() {
        assert_eq!(
            GradientOrientation::default(),
            GradientOrientation::Horizontal
        );
    }

    #[test]
    fn gradient_orientation_equality() {
        assert_eq!(
            GradientOrientation::Horizontal,
            GradientOrientation::Horizontal
        );
        assert_ne!(
            GradientOrientation::Horizontal,
            GradientOrientation::Vertical
        );
    }

    #[test]
    fn gradient_orientation_linear_with_angle() {
        let a = GradientOrientation::Linear { angle: Some(45.0) };
        let b = GradientOrientation::Linear { angle: Some(45.0) };
        let c = GradientOrientation::Linear { angle: Some(90.0) };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn gradient_orientation_radial() {
        let a = GradientOrientation::Radial {
            radius: Some(1.0),
            cx: Some(0.5),
            cy: Some(0.5),
        };
        let b = GradientOrientation::Radial {
            radius: Some(1.0),
            cx: Some(0.5),
            cy: Some(0.5),
        };
        assert_eq!(a, b);
    }

    // --- GradientPreset ---

    #[test]
    fn gradient_preset_equality() {
        assert_eq!(GradientPreset::Blues, GradientPreset::Blues);
        assert_ne!(GradientPreset::Blues, GradientPreset::Reds);
        assert_ne!(GradientPreset::Viridis, GradientPreset::Plasma);
    }

    #[test]
    #[allow(clippy::clone_on_copy)]
    fn gradient_preset_clone_copy() {
        let a = GradientPreset::Rainbow;
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }
}
