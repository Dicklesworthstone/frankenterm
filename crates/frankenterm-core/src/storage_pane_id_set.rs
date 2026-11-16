//! br-ft-l87np: storage-domain pane_id bitmap set substrate.
//!
//! Query paths that need to filter large pane fleets currently build
//! long `pane_id IN (?, ?, ?)` placeholder lists. [`PaneIdSet`] keeps
//! those pane ids sorted and deduplicated with a compressed Roaring
//! bitmap, lets small sets inline a deterministic SQL predicate, and
//! gives large sets a temp-table plan so callers can avoid oversized
//! SQL strings and bind lists.

use roaring::RoaringTreemap;

const TEMP_TABLE_NAME: &str = "temp_pane_id_set";

/// Storage-friendly set of pane ids backed by a compressed Roaring
/// treemap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneIdSet {
    inner: RoaringTreemap,
}

/// SQL plan for materializing a large [`PaneIdSet`] into a temporary
/// table before joining or filtering storage queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneIdTempTablePlan {
    pub table_name: &'static str,
    pub create_sql: &'static str,
    pub clear_sql: &'static str,
    pub insert_sql: &'static str,
    pub pane_ids: Vec<u64>,
}

impl PaneIdSet {
    /// Create an empty pane id set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RoaringTreemap::new(),
        }
    }

    /// Build a sorted, unique set from any pane id iterator.
    #[must_use]
    pub fn from_pane_ids<I>(pane_ids: I) -> Self
    where
        I: IntoIterator<Item = u64>,
    {
        let mut set = Self::new();
        for pane_id in pane_ids {
            set.insert(pane_id);
        }
        set
    }

    /// Number of unique pane ids in the set.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.inner.len()
    }

    /// Whether the set contains no pane ids.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Serialized bitmap footprint in bytes.
    ///
    /// This is the stable storage/transit size of the compressed
    /// bitmap, which is the relevant bound for deciding whether this
    /// substrate is smaller than a naive `Vec<u64>` representation.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.inner.serialized_size()
    }

    /// Return true when `pane_id` is present.
    #[must_use]
    pub fn contains(&self, pane_id: u64) -> bool {
        self.inner.contains(pane_id)
    }

    /// Insert `pane_id`, returning true if it was not already present.
    pub fn insert(&mut self, pane_id: u64) -> bool {
        self.inner.insert(pane_id)
    }

    /// Remove `pane_id`, returning true if it was present.
    pub fn remove(&mut self, pane_id: u64) -> bool {
        self.inner.remove(pane_id)
    }

    /// Intersect this set with `other` in place.
    pub fn intersect_with(&mut self, other: &Self) {
        self.inner &= &other.inner;
    }

    /// Union this set with `other` in place.
    pub fn union_with(&mut self, other: &Self) {
        self.inner |= &other.inner;
    }

    /// Iterate pane ids in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.inner.iter()
    }

    /// Return a sorted, unique pane id vector.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u64> {
        self.iter().collect()
    }

    /// Build a deterministic SQL predicate for small sets.
    ///
    /// Returns `None` when the set has more than `max_inline` entries
    /// or contains a value that cannot be represented as SQLite's
    /// signed INTEGER literal. Those cases should use
    /// [`Self::temp_table_plan`] instead.
    #[must_use]
    pub fn as_sql_in_clause(&self, max_inline: usize) -> Option<String> {
        self.as_sql_in_clause_for_column("pane_id", max_inline)
    }

    /// Build a deterministic SQL predicate for a caller-supplied pane id column.
    ///
    /// `column_sql` is intentionally plain SQL so storage callsites can use
    /// aliases such as `e.pane_id`. It must be a trusted static column
    /// expression, not user input.
    #[must_use]
    pub fn as_sql_in_clause_for_column(
        &self,
        column_sql: &'static str,
        max_inline: usize,
    ) -> Option<String> {
        if self.len() as usize > max_inline {
            return None;
        }

        if self.is_empty() {
            return Some("1 = 0".to_string());
        }

        let mut literals = Vec::with_capacity(self.len() as usize);
        for pane_id in self.iter() {
            if pane_id > i64::MAX as u64 {
                return None;
            }
            literals.push(pane_id.to_string());
        }

        Some(format!("{column_sql} IN ({})", literals.join(",")))
    }

    /// Plan for loading this set into a temp table.
    #[must_use]
    pub fn temp_table_plan(&self) -> PaneIdTempTablePlan {
        PaneIdTempTablePlan {
            table_name: TEMP_TABLE_NAME,
            create_sql: "CREATE TEMP TABLE IF NOT EXISTS temp_pane_id_set (pane_id INTEGER PRIMARY KEY) WITHOUT ROWID",
            clear_sql: "DELETE FROM temp_pane_id_set",
            insert_sql: "INSERT OR IGNORE INTO temp_pane_id_set (pane_id) VALUES (?1)",
            pane_ids: self.to_vec(),
        }
    }
}

impl FromIterator<u64> for PaneIdSet {
    fn from_iter<T: IntoIterator<Item = u64>>(iter: T) -> Self {
        Self::from_pane_ids(iter)
    }
}

impl IntoIterator for PaneIdSet {
    type Item = u64;
    type IntoIter = <RoaringTreemap as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(values: &[u64]) -> PaneIdSet {
        values.iter().copied().collect()
    }

    #[test]
    fn pane_id_set_round_trips_sorted_unique_values() {
        let s = set(&[42, 7, 42, u64::MAX, 1, 7, 4096]);

        assert_eq!(s.to_vec(), vec![1, 7, 42, 4096, u64::MAX]);
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn pane_id_set_insert_remove_and_contains_are_idempotent() {
        let mut s = PaneIdSet::new();

        assert!(s.insert(9));
        assert!(!s.insert(9));
        assert!(s.contains(9));
        assert!(s.remove(9));
        assert!(!s.remove(9));
        assert!(!s.contains(9));
        assert!(s.is_empty());
    }

    #[test]
    fn pane_id_set_ops_are_commutative_and_associative() {
        let a = set(&[1, 2, 3, 100]);
        let b = set(&[3, 4, 100, 500]);
        let c = set(&[1, 500, 900]);

        let mut ab = a.clone();
        ab.union_with(&b);
        let mut ba = b.clone();
        ba.union_with(&a);
        assert_eq!(ab, ba);

        let mut left = ab.clone();
        left.union_with(&c);
        let mut bc = b.clone();
        bc.union_with(&c);
        let mut right = a.clone();
        right.union_with(&bc);
        assert_eq!(left, right);

        let mut ai_b = a.clone();
        ai_b.intersect_with(&b);
        let mut bi_a = b.clone();
        bi_a.intersect_with(&a);
        assert_eq!(ai_b, bi_a);

        let mut left_i = a.clone();
        left_i.intersect_with(&b);
        left_i.intersect_with(&c);
        let mut right_i = b.clone();
        right_i.intersect_with(&c);
        let mut a_right_i = a.clone();
        a_right_i.intersect_with(&right_i);
        assert_eq!(left_i, a_right_i);
    }

    #[test]
    fn pane_id_set_compresses_dense_randomish_ids_under_naive_vec_bound() {
        let values = (0..1000).map(|i| ((i * 37) % 4096) as u64);
        let s = PaneIdSet::from_pane_ids(values);

        assert_eq!(s.len(), 1000);
        assert!(
            s.memory_bytes() < 8 * 1024,
            "bitmap footprint {} should stay below 8 KiB naive Vec<u64> bound",
            s.memory_bytes()
        );
    }

    #[test]
    fn pane_id_set_inlines_small_sql_predicates_only() {
        assert_eq!(
            PaneIdSet::new().as_sql_in_clause(4).as_deref(),
            Some("1 = 0")
        );

        let small = set(&[9, 1, 9, 4]);
        assert_eq!(
            small.as_sql_in_clause(4).as_deref(),
            Some("pane_id IN (1,4,9)")
        );
        assert_eq!(
            small.as_sql_in_clause_for_column("e.pane_id", 4).as_deref(),
            Some("e.pane_id IN (1,4,9)")
        );
        assert_eq!(small.as_sql_in_clause(2), None);

        let too_wide_for_sqlite_integer = set(&[i64::MAX as u64 + 1]);
        assert_eq!(too_wide_for_sqlite_integer.as_sql_in_clause(4), None);
    }

    #[test]
    fn pane_id_set_temp_table_plan_preserves_sorted_unique_rows() {
        let plan = set(&[5, 2, 5, 3]).temp_table_plan();

        assert_eq!(plan.table_name, "temp_pane_id_set");
        assert_eq!(plan.pane_ids, vec![2, 3, 5]);
        assert!(plan.create_sql.contains("CREATE TEMP TABLE IF NOT EXISTS"));
        assert!(plan.clear_sql.contains("DELETE FROM"));
        assert!(plan.insert_sql.contains("INSERT OR IGNORE"));
    }
}
