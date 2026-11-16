use frankenterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, Value};
use std::str::FromStr;

#[derive(Debug, Copy, Clone)]
pub struct OptPixelUnit(Option<Dimension>);

impl FromDynamic for OptPixelUnit {
    fn from_dynamic(
        value: &Value,
        _options: FromDynamicOptions,
    ) -> Result<Self, frankenterm_dynamic::Error> {
        match value {
            Value::Null => Ok(Self(None)),
            value => Ok(Self(Some(DefaultUnit::Pixels.from_dynamic_impl(value)?))),
        }
    }
}

impl From<OptPixelUnit> for Option<Dimension> {
    fn from(val: OptPixelUnit) -> Self {
        val.0
    }
}

#[derive(Debug, Copy, Clone)]
pub struct PixelUnit(Dimension);

impl From<PixelUnit> for Dimension {
    fn from(val: PixelUnit) -> Self {
        val.0
    }
}

impl FromDynamic for PixelUnit {
    fn from_dynamic(
        value: &Value,
        _options: FromDynamicOptions,
    ) -> Result<Self, frankenterm_dynamic::Error> {
        Ok(Self(DefaultUnit::Pixels.from_dynamic_impl(value)?))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DefaultUnit {
    Points,
    Pixels,
    Percent,
    Cells,
}

impl DefaultUnit {
    fn to_dimension(self, value: f32) -> Dimension {
        match self {
            Self::Points => Dimension::Points(value),
            Self::Pixels => Dimension::Pixels(value),
            Self::Percent => Dimension::Percent(value / 100.),
            Self::Cells => Dimension::Cells(value),
        }
    }
}

impl DefaultUnit {
    fn from_dynamic_impl(self, value: &Value) -> Result<Dimension, String> {
        match value {
            Value::F64(f) => Ok(self.to_dimension(f.into_inner() as f32)),
            Value::I64(i) => Ok(self.to_dimension(*i as f32)),
            Value::U64(u) => Ok(self.to_dimension(*u as f32)),
            Value::String(s) => {
                if let Ok(value) = s.parse::<f32>() {
                    Ok(self.to_dimension(value))
                } else {
                    fn is_unit(s: &str, unit: &'static str) -> Option<f32> {
                        let s = s.strip_suffix(unit)?.trim();
                        s.parse().ok()
                    }

                    if let Some(v) = is_unit(s, "px") {
                        Ok(DefaultUnit::Pixels.to_dimension(v))
                    } else if let Some(v) = is_unit(s, "%") {
                        Ok(DefaultUnit::Percent.to_dimension(v))
                    } else if let Some(v) = is_unit(s, "pt") {
                        Ok(DefaultUnit::Points.to_dimension(v))
                    } else if let Some(v) = is_unit(s, "cell") {
                        Ok(DefaultUnit::Cells.to_dimension(v))
                    } else {
                        Err(format!(
                            "expected either a number or a string of \
                        the form '123px' where 'px' is a unit and \
                        can be one of 'px', '%', 'pt' or 'cell', \
                        but got {}",
                            s
                        ))
                    }
                }
            }
            other => Err(format!(
                "expected either a number or a string of \
                        the form '123px' where 'px' is a unit and \
                        can be one of 'px', '%', 'pt' or 'cell', \
                        but got {}",
                other.variant_name()
            )),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Dimension {
    /// A value expressed in points, where 72 points == 1 inch.
    Points(f32),

    /// A value expressed in raw pixels
    Pixels(f32),

    /// A value expressed in terms of a fraction of the maximum
    /// value in the same direction.  For example, left padding
    /// of 10% depends on the pixel width of that element.
    /// The value is 1.0 == 100%.  It is possible to express
    /// eg: 2.0 for 200%.
    Percent(f32),

    /// A value expressed in terms of a fraction of the cell
    /// size computed from the configured font size.
    /// 1.0 == the cell size.
    Cells(f32),
}

impl Dimension {
    pub fn is_zero(&self) -> bool {
        match self {
            Self::Points(n) | Self::Pixels(n) | Self::Percent(n) | Self::Cells(n) => *n == 0.,
        }
    }
}

impl Default for Dimension {
    fn default() -> Self {
        Self::Pixels(0.)
    }
}

impl ToDynamic for Dimension {
    fn to_dynamic(&self) -> Value {
        let s = match self {
            Self::Points(n) => format!("{}pt", n),
            Self::Pixels(n) => format!("{}px", n),
            Self::Percent(n) => format!("{}%", n * 100.),
            Self::Cells(n) => format!("{}cell", n),
        };
        Value::String(s)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DimensionContext {
    pub dpi: f32,
    /// Width/Height or other upper bound on the dimension,
    /// measured in pixels.
    pub pixel_max: f32,
    /// Width/Height of the font metrics cell size in the appropriate
    /// dimension, measured in pixels.
    pub pixel_cell: f32,
}

impl Dimension {
    pub fn evaluate_as_pixels(&self, context: DimensionContext) -> f32 {
        match self {
            Self::Pixels(n) => n.floor(),
            Self::Points(pt) => (pt * context.dpi / 72.0).floor(),
            Self::Percent(p) => (p * context.pixel_max).floor(),
            Self::Cells(c) => (c * context.pixel_cell).floor(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum GeometryOrigin {
    /// x,y relative to overall screen coordinate system.
    /// Selected position might be outside of the regions covered
    /// by the user's selected monitor placement.
    ScreenCoordinateSystem,
    MainScreen,
    ActiveScreen,
    Named(String),
}

impl Default for GeometryOrigin {
    fn default() -> Self {
        Self::ScreenCoordinateSystem
    }
}

#[derive(Debug, Clone, PartialEq, FromDynamic, ToDynamic)]
pub struct GuiPosition {
    #[dynamic(try_from = "crate::units::PixelUnit")]
    pub x: Dimension,
    #[dynamic(try_from = "crate::units::PixelUnit")]
    pub y: Dimension,
    #[dynamic(default)]
    pub origin: GeometryOrigin,
}

impl GuiPosition {
    fn parse_dim(s: &str) -> anyhow::Result<Dimension> {
        if let Some(v) = s.strip_suffix("px") {
            Ok(Dimension::Pixels(v.parse()?))
        } else if let Some(v) = s.strip_suffix("%") {
            Ok(Dimension::Percent(v.parse::<f32>()? / 100.))
        } else {
            Ok(Dimension::Pixels(s.parse()?))
        }
    }

    fn parse_x_y(s: &str) -> anyhow::Result<(Dimension, Dimension)> {
        let fields: Vec<_> = s.split(',').collect();
        if fields.len() != 2 {
            anyhow::bail!("expected x,y coordinates");
        }
        Ok((Self::parse_dim(fields[0])?, Self::parse_dim(fields[1])?))
    }

    fn parse_origin(s: &str) -> GeometryOrigin {
        match s {
            "screen" => GeometryOrigin::ScreenCoordinateSystem,
            "main" => GeometryOrigin::MainScreen,
            "active" => GeometryOrigin::ActiveScreen,
            name => GeometryOrigin::Named(name.to_string()),
        }
    }
}

impl FromStr for GuiPosition {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<GuiPosition> {
        let fields: Vec<_> = s.split(':').collect();
        if fields.len() == 2 {
            let origin = Self::parse_origin(fields[0]);
            let (x, y) = Self::parse_x_y(fields[1])?;
            return Ok(GuiPosition { x, y, origin });
        }
        if fields.len() == 1 {
            let (x, y) = Self::parse_x_y(fields[0])?;
            return Ok(GuiPosition {
                x,
                y,
                origin: GeometryOrigin::ScreenCoordinateSystem,
            });
        }
        anyhow::bail!("invalid position spec {}", s);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn xy() {
        assert_eq!(
            GuiPosition::from_str("10,20").unwrap(),
            GuiPosition {
                x: Dimension::Pixels(10.),
                y: Dimension::Pixels(20.),
                origin: GeometryOrigin::ScreenCoordinateSystem
            }
        );

        assert_eq!(
            GuiPosition::from_str("screen:10,20").unwrap(),
            GuiPosition {
                x: Dimension::Pixels(10.),
                y: Dimension::Pixels(20.),
                origin: GeometryOrigin::ScreenCoordinateSystem
            }
        );
    }

    #[test]
    fn named() {
        assert_eq!(
            GuiPosition::from_str("hdmi-1:10,20").unwrap(),
            GuiPosition {
                x: Dimension::Pixels(10.),
                y: Dimension::Pixels(20.),
                origin: GeometryOrigin::Named("hdmi-1".to_string()),
            }
        );
    }

    #[test]
    fn active() {
        assert_eq!(
            GuiPosition::from_str("active:10,20").unwrap(),
            GuiPosition {
                x: Dimension::Pixels(10.),
                y: Dimension::Pixels(20.),
                origin: GeometryOrigin::ActiveScreen
            }
        );
    }

    #[test]
    fn main() {
        assert_eq!(
            GuiPosition::from_str("main:10,20").unwrap(),
            GuiPosition {
                x: Dimension::Pixels(10.),
                y: Dimension::Pixels(20.),
                origin: GeometryOrigin::MainScreen
            }
        );
    }

    #[test]
    fn dimension_default_is_zero_pixels() {
        assert_eq!(Dimension::default(), Dimension::Pixels(0.));
    }

    #[test]
    fn dimension_is_zero() {
        assert!(Dimension::Pixels(0.).is_zero());
        assert!(Dimension::Points(0.).is_zero());
        assert!(Dimension::Percent(0.).is_zero());
        assert!(Dimension::Cells(0.).is_zero());
        assert!(!Dimension::Pixels(1.).is_zero());
    }

    #[test]
    fn dimension_to_dynamic_roundtrip_format() {
        assert_eq!(
            Dimension::Pixels(10.).to_dynamic(),
            Value::String("10px".to_string())
        );
        assert_eq!(
            Dimension::Points(12.).to_dynamic(),
            Value::String("12pt".to_string())
        );
        assert_eq!(
            Dimension::Percent(0.5).to_dynamic(),
            Value::String("50%".to_string())
        );
        assert_eq!(
            Dimension::Cells(2.).to_dynamic(),
            Value::String("2cell".to_string())
        );
    }

    #[test]
    fn dimension_evaluate_pixels() {
        let ctx = DimensionContext {
            dpi: 96.0,
            pixel_max: 1920.0,
            pixel_cell: 10.0,
        };
        assert_eq!(Dimension::Pixels(100.5).evaluate_as_pixels(ctx), 100.0);
    }

    #[test]
    fn dimension_evaluate_points() {
        let ctx = DimensionContext {
            dpi: 72.0,
            pixel_max: 1920.0,
            pixel_cell: 10.0,
        };
        // 12pt at 72 DPI = 12 pixels
        assert_eq!(Dimension::Points(12.).evaluate_as_pixels(ctx), 12.0);
    }

    #[test]
    fn dimension_evaluate_points_high_dpi() {
        let ctx = DimensionContext {
            dpi: 144.0,
            pixel_max: 1920.0,
            pixel_cell: 10.0,
        };
        // 12pt at 144 DPI = 24 pixels
        assert_eq!(Dimension::Points(12.).evaluate_as_pixels(ctx), 24.0);
    }

    #[test]
    fn dimension_evaluate_percent() {
        let ctx = DimensionContext {
            dpi: 96.0,
            pixel_max: 1000.0,
            pixel_cell: 10.0,
        };
        // 50% of 1000 = 500
        assert_eq!(Dimension::Percent(0.5).evaluate_as_pixels(ctx), 500.0);
    }

    #[test]
    fn dimension_evaluate_cells() {
        let ctx = DimensionContext {
            dpi: 96.0,
            pixel_max: 1920.0,
            pixel_cell: 15.0,
        };
        // 3 cells at 15px each = 45
        assert_eq!(Dimension::Cells(3.).evaluate_as_pixels(ctx), 45.0);
    }

    #[test]
    fn default_unit_to_dimension() {
        assert_eq!(
            DefaultUnit::Points.to_dimension(10.0),
            Dimension::Points(10.0)
        );
        assert_eq!(
            DefaultUnit::Pixels.to_dimension(10.0),
            Dimension::Pixels(10.0)
        );
        assert_eq!(
            DefaultUnit::Percent.to_dimension(50.0),
            Dimension::Percent(0.5)
        );
        assert_eq!(DefaultUnit::Cells.to_dimension(3.0), Dimension::Cells(3.0));
    }

    #[test]
    fn from_dynamic_integer_uses_default_unit() {
        let dim = DefaultUnit::Pixels
            .from_dynamic_impl(&Value::I64(42))
            .unwrap();
        assert_eq!(dim, Dimension::Pixels(42.0));
    }

    #[test]
    fn from_dynamic_unsigned_uses_default_unit() {
        let dim = DefaultUnit::Points
            .from_dynamic_impl(&Value::U64(12))
            .unwrap();
        assert_eq!(dim, Dimension::Points(12.0));
    }

    #[test]
    fn from_dynamic_string_with_px_suffix() {
        let dim = DefaultUnit::Points
            .from_dynamic_impl(&Value::String("100px".to_string()))
            .unwrap();
        assert_eq!(dim, Dimension::Pixels(100.0));
    }

    #[test]
    fn from_dynamic_string_with_pt_suffix() {
        let dim = DefaultUnit::Pixels
            .from_dynamic_impl(&Value::String("14pt".to_string()))
            .unwrap();
        assert_eq!(dim, Dimension::Points(14.0));
    }

    #[test]
    fn from_dynamic_string_with_percent_suffix() {
        let dim = DefaultUnit::Pixels
            .from_dynamic_impl(&Value::String("50%".to_string()))
            .unwrap();
        assert_eq!(dim, Dimension::Percent(0.5));
    }

    #[test]
    fn from_dynamic_string_with_cell_suffix() {
        let dim = DefaultUnit::Pixels
            .from_dynamic_impl(&Value::String("3cell".to_string()))
            .unwrap();
        assert_eq!(dim, Dimension::Cells(3.0));
    }

    #[test]
    fn from_dynamic_plain_numeric_string() {
        let dim = DefaultUnit::Pixels
            .from_dynamic_impl(&Value::String("42".to_string()))
            .unwrap();
        assert_eq!(dim, Dimension::Pixels(42.0));
    }

    #[test]
    fn from_dynamic_invalid_string() {
        let result =
            DefaultUnit::Pixels.from_dynamic_impl(&Value::String("not-a-number".to_string()));
        assert!(result.is_err());
    }

    #[test]
    fn from_dynamic_invalid_type() {
        let result = DefaultUnit::Pixels.from_dynamic_impl(&Value::Bool(true));
        assert!(result.is_err());
    }

    #[test]
    fn geometry_origin_default() {
        assert_eq!(
            GeometryOrigin::default(),
            GeometryOrigin::ScreenCoordinateSystem
        );
    }

    #[test]
    fn gui_position_with_percent_dimensions() {
        let pos = GuiPosition::from_str("50%,75%").unwrap();
        assert_eq!(pos.x, Dimension::Percent(0.5));
        assert_eq!(pos.y, Dimension::Percent(0.75));
    }

    #[test]
    fn gui_position_with_px_suffix() {
        let pos = GuiPosition::from_str("100px,200px").unwrap();
        assert_eq!(pos.x, Dimension::Pixels(100.));
        assert_eq!(pos.y, Dimension::Pixels(200.));
    }

    #[test]
    fn gui_position_invalid_format() {
        assert!(GuiPosition::from_str("a:b:c").is_err());
    }

    #[test]
    fn gui_position_invalid_coordinates() {
        assert!(GuiPosition::from_str("notanumber,200").is_err());
    }

    #[test]
    fn gui_position_missing_y() {
        assert!(GuiPosition::from_str("100").is_err());
    }

    #[test]
    fn opt_pixel_unit_from_null() {
        let opu = OptPixelUnit::from_dynamic(&Value::Null, Default::default()).unwrap();
        let dim: Option<Dimension> = opu.into();
        assert!(dim.is_none());
    }

    #[test]
    fn opt_pixel_unit_from_value() {
        let opu = OptPixelUnit::from_dynamic(&Value::U64(42), Default::default()).unwrap();
        let dim: Option<Dimension> = opu.into();
        assert_eq!(dim, Some(Dimension::Pixels(42.0)));
    }

    #[test]
    fn pixel_unit_from_dynamic() {
        let pu = PixelUnit::from_dynamic(&Value::U64(10), Default::default()).unwrap();
        let dim: Dimension = pu.into();
        assert_eq!(dim, Dimension::Pixels(10.0));
    }
}
