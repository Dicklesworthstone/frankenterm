use crate::line::cellref::CellRef;
use crate::line::clusterline::{ClusterLineCellIter, ClusteredLine};
use crate::line::vecstorage::{CellViewIter, VecStorage};
use alloc::sync::Arc;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub(crate) enum CellStorage {
    V(VecStorage),
    // ClusteredLine still owns and zeroizes its text on final-owner drop.
    // Mutable access must detach through Arc::make_mut in Line.
    C(Arc<ClusteredLine>),
}

impl PartialEq for CellStorage {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::V(left), Self::V(right)) => left == right,
            (Self::C(left), Self::C(right)) => {
                Arc::ptr_eq(left, right) || left.as_ref() == right.as_ref()
            }
            _ => false,
        }
    }
}

pub(crate) enum VisibleCellIter<'a> {
    V(CellViewIter<'a>),
    C(ClusterLineCellIter<'a>),
}

impl<'a> Iterator for VisibleCellIter<'a> {
    type Item = CellRef<'a>;

    fn next(&mut self) -> Option<CellRef<'a>> {
        match self {
            Self::V(iter) => iter.next(),
            Self::C(iter) => iter.next(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn clustered_storage_keeps_zeroizing_owner_alive_until_last_clone() {
        fn requires_zeroizing_drop<T: zeroize::ZeroizeOnDrop>() {}
        requires_zeroizing_drop::<ClusteredLine>();
        let mut payload = ClusteredLine::new();
        payload.append_ascii_run("owned", frankenterm_cell::CellAttributes::blank());
        let original = CellStorage::C(Arc::new(payload));
        let weak = match &original {
            CellStorage::C(payload) => Arc::downgrade(payload),
            _ => unreachable!(),
        };
        let snapshot = original.clone();
        drop(original);
        assert_eq!(weak.upgrade().unwrap().text, "owned");
        drop(snapshot);
        assert!(
            weak.upgrade().is_none(),
            "last owner runs ClusteredLine Drop"
        );
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn memory_usage() {
        assert_eq!(core::mem::size_of::<CellStorage>(), 16);
        assert_eq!(core::mem::size_of::<VecStorage>(), 8);
    }
}
