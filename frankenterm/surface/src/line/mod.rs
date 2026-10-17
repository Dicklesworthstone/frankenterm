mod cellref;
mod clusterline;
mod line;
mod linebits;
mod storage;
mod test;
mod vecstorage;

pub use cellref::CellRef;
#[doc(hidden)]
pub use clusterline::clustered_append_breaks;
pub use line::{
    DoubleClickRange, Line, LineWrapGeometry, LineWrapGeometryJoinError, LineWrapLayout,
    LineWrapReport, LineWrapScorecard, LineWrapWidthPrefixScratch, MonospaceKpCostModel,
    MonospaceWrapMode, MonospaceWrapPlan, KP_BADNESS_INF, KP_DEFAULT_LOOKAHEAD_LIMIT,
    KP_DEFAULT_MAX_DP_STATES,
};
