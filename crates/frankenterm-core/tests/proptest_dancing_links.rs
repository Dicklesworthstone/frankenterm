//! Property-based tests for `dancing_links` module.
//!
//! Verifies correctness invariants:
//! - Solutions are valid exact covers (each column hit exactly once)
//! - solve() and solve_all() agree on solvability
//! - solve_limited respects limit
//! - Identity matrices always solvable
//! - Serde roundtrip
//! - Clone consistency
//! - Monotonicity and idempotency

use frankenterm_core::dancing_links::DancingLinks;
use proptest::prelude::*;
use std::collections::HashSet;

// ── Strategies ─────────────────────────────────────────────────────────

fn sparse_matrix_strategy(
    max_rows: usize,
    max_cols: usize,
) -> impl Strategy<Value = (usize, Vec<Vec<usize>>)> {
    (2..max_cols).prop_flat_map(move |num_cols| {
        let col_range = 0..num_cols;
        let row_strategy = prop::collection::vec(
            prop::collection::vec(col_range.clone(), 1..=num_cols.min(4)),
            1..max_rows,
        )
        .prop_map(|rows| {
            // Deduplicate columns within each row
            rows.into_iter()
                .map(|mut r| {
                    r.sort();
                    r.dedup();
                    r
                })
                .collect::<Vec<_>>()
        });

        (Just(num_cols), row_strategy)
    })
}

// Helper: verify a solution is a valid exact cover
fn verify_exact_cover(num_cols: usize, rows: &[Vec<usize>], solution: &[usize]) -> bool {
    let mut covered = vec![false; num_cols];
    for &row_idx in solution {
        if row_idx >= rows.len() {
            return false;
        }
        for &col in &rows[row_idx] {
            if covered[col] {
                return false; // Double cover
            }
            covered[col] = true;
        }
    }
    covered.iter().all(|&c| c) // All covered
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── Solutions are valid exact covers ──────────────────────────

    #[test]
    fn solutions_are_valid(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let solutions = dlx.solve_all();
        for solution in &solutions {
            let is_valid = verify_exact_cover(num_cols, &rows, solution);
            prop_assert!(is_valid, "invalid exact cover: {:?}", solution);
        }
    }

    // ── solve and solve_all agree ────────────────────────────────

    #[test]
    fn solve_and_solve_all_agree(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        let mut dlx1 = DancingLinks::new(num_cols);
        let mut dlx2 = DancingLinks::new(num_cols);
        for row in &rows {
            dlx1.add_row(row);
            dlx2.add_row(row);
        }

        let single = dlx1.solve();
        let all = dlx2.solve_all();

        match single {
            Some(_) => prop_assert!(!all.is_empty(), "solve found solution but solve_all didn't"),
            None => prop_assert!(all.is_empty(), "solve_all found solutions but solve didn't"),
        }
    }

    // ── solve_limited respects limit ─────────────────────────────

    #[test]
    fn solve_limited_respects_limit(
        (num_cols, rows) in sparse_matrix_strategy(8, 6),
        limit in 1usize..5
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let limited = dlx.solve_limited(limit);
        prop_assert!(limited.len() <= limit);
    }

    // ── Identity matrix always solvable ──────────────────────────

    #[test]
    fn identity_always_solvable(n in 1usize..8) {
        let mut dlx = DancingLinks::new(n);
        for i in 0..n {
            dlx.add_row(&[i]);
        }

        let solution = dlx.solve().unwrap();
        prop_assert_eq!(solution.len(), n);
    }

    // ── Permuted identity always solvable ────────────────────────

    #[test]
    fn permuted_identity_solvable(perm in prop::collection::vec(0..6usize, 6)) {
        // Make a valid permutation
        let mut cols: Vec<usize> = (0..6).collect();
        for (i, &p) in perm.iter().enumerate() {
            let j = p % (6 - i) + i;
            cols.swap(i, j.min(5));
        }

        let mut dlx = DancingLinks::new(6);
        for &col in &cols {
            dlx.add_row(&[col]);
        }

        // If all columns are covered exactly once, it should be solvable
        let mut seen = [false; 6];
        let mut all_unique = true;
        for &c in &cols {
            if seen[c] {
                all_unique = false;
                break;
            }
            seen[c] = true;
        }

        if all_unique && seen.iter().all(|&s| s) {
            let solution = dlx.solve();
            prop_assert!(solution.is_some());
        }
    }

    // ── from_matrix consistency ───────────────────────────────────

    #[test]
    fn from_matrix_consistent(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        // Build from matrix
        let matrix: Vec<Vec<bool>> = rows.iter().map(|row| {
            let mut bools = vec![false; num_cols];
            for &col in row {
                bools[col] = true;
            }
            bools
        }).collect();

        let mut dlx_manual = DancingLinks::new(num_cols);
        for row in &rows {
            dlx_manual.add_row(row);
        }

        let mut dlx_matrix = DancingLinks::from_matrix(&matrix);

        let sol1 = dlx_manual.solve_all();
        let sol2 = dlx_matrix.solve_all();

        prop_assert_eq!(sol1.len(), sol2.len(), "different solution counts");
    }

    // ── No duplicate rows in solution ────────────────────────────

    #[test]
    fn no_duplicate_rows(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let solutions = dlx.solve_all();
        for solution in &solutions {
            let mut sorted = solution.clone();
            sorted.sort();
            sorted.dedup();
            prop_assert_eq!(sorted.len(), solution.len(), "duplicate rows in solution");
        }
    }

    // ── Serde roundtrip preserves solvability ────────────────────

    #[test]
    fn serde_roundtrip(
        (num_cols, rows) in sparse_matrix_strategy(6, 5)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let json = serde_json::to_string(&dlx).unwrap();
        let mut restored: DancingLinks = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.num_columns(), dlx.num_columns());
        prop_assert_eq!(restored.num_rows(), dlx.num_rows());

        let orig_solutions = dlx.solve_all();
        let rest_solutions = restored.solve_all();
        prop_assert_eq!(orig_solutions.len(), rest_solutions.len());
    }

    // ── Disjoint rows covering all columns = solvable ────────────

    #[test]
    fn disjoint_covering_rows(n in 2usize..6) {
        // Create rows that partition columns into pairs/singles
        let mut dlx = DancingLinks::new(n);
        for i in 0..n {
            dlx.add_row(&[i]);
        }

        // Add some extra rows that partially overlap
        if n >= 4 {
            dlx.add_row(&[0, 1]);
            dlx.add_row(&[2, 3]);
        }

        let solutions = dlx.solve_all();
        // At minimum, the identity rows form a solution
        prop_assert!(!solutions.is_empty());
    }

    // ══════════════════════════════════════════════════════════════
    // ── NEW PROPERTY TESTS (10–25) ──────────────────────────────
    // ══════════════════════════════════════════════════════════════

    // ── 10. solve_limited solutions are each valid exact covers ──

    #[test]
    fn solve_limited_subset(
        (num_cols, rows) in sparse_matrix_strategy(8, 6),
        limit in 1usize..6
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let limited = dlx.solve_limited(limit);
        for solution in &limited {
            let is_valid = verify_exact_cover(num_cols, &rows, solution);
            prop_assert!(is_valid, "solve_limited returned invalid cover: {:?}", solution);
        }
    }

    // ── 11. solve() result appears in solve_all() ────────────────

    #[test]
    fn solve_result_in_solve_all(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let single = dlx.solve();
        let all = dlx.solve_all();

        if let Some(sol) = single {
            let mut sol_sorted = sol.clone();
            sol_sorted.sort();

            let found = all.iter().any(|s| {
                let mut s_sorted = s.clone();
                s_sorted.sort();
                s_sorted == sol_sorted
            });
            prop_assert!(found, "solve() result not found in solve_all()");
        }
    }

    // ── 12. clone produces same solutions ────────────────────────

    #[test]
    fn clone_produces_same_solutions(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let mut cloned = dlx.clone();

        let orig_all = dlx.solve_all();
        let clone_all = cloned.solve_all();

        // Normalize both for comparison
        let mut orig_norm: Vec<Vec<usize>> = orig_all.iter().map(|s| {
            let mut sorted = s.clone();
            sorted.sort();
            sorted
        }).collect();
        let mut clone_norm: Vec<Vec<usize>> = clone_all.iter().map(|s| {
            let mut sorted = s.clone();
            sorted.sort();
            sorted
        }).collect();
        orig_norm.sort();
        clone_norm.sort();

        prop_assert_eq!(orig_norm, clone_norm);
    }

    // ── 13. num_rows tracks additions ────────────────────────────

    #[test]
    fn num_rows_tracks_additions(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        let mut expected_count = 0usize;
        for row in &rows {
            // All rows from sparse_matrix_strategy have at least 1 column
            // (generated with 1..=num_cols.min(4) elements)
            dlx.add_row(row);
            expected_count += 1;
        }

        let actual = dlx.num_rows();
        prop_assert_eq!(actual, expected_count, "num_rows mismatch");
    }

    // ── 14. solve_all solutions are distinct ─────────────────────

    #[test]
    fn solve_all_distinct(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let solutions = dlx.solve_all();
        let normalized: Vec<Vec<usize>> = solutions.iter().map(|s| {
            let mut sorted = s.clone();
            sorted.sort();
            sorted
        }).collect();

        let unique: HashSet<Vec<usize>> = normalized.iter().cloned().collect();
        prop_assert_eq!(
            unique.len(),
            normalized.len(),
            "solve_all returned duplicate solutions"
        );
    }

    // ── 15. single column: N rows → exactly N solutions ──────────

    #[test]
    fn single_column_solutions(n in 1usize..12) {
        let mut dlx = DancingLinks::new(1);
        for _ in 0..n {
            dlx.add_row(&[0]);
        }

        let solutions = dlx.solve_all();
        prop_assert_eq!(solutions.len(), n, "1-col matrix with {} rows should have {} solutions", n, n);

        // Each solution should be exactly one row
        for sol in &solutions {
            prop_assert_eq!(sol.len(), 1usize, "single-column solution should use 1 row");
        }
    }

    // ── 16. block diagonal: solution count = product of per-block counts ──

    #[test]
    fn block_diagonal_independence(
        n_a in 1usize..5,
        n_b in 1usize..5
    ) {
        // Block A: n_a rows covering column 0
        // Block B: n_b rows covering column 1
        // Independent blocks → solution count = n_a * n_b
        let mut dlx = DancingLinks::new(2);
        for _ in 0..n_a {
            dlx.add_row(&[0]);
        }
        for _ in 0..n_b {
            dlx.add_row(&[1]);
        }

        let solutions = dlx.solve_all();
        let expected = n_a * n_b;
        prop_assert_eq!(solutions.len(), expected, "block diagonal: expected {} * {} = {}", n_a, n_b, expected);
    }

    // ── 17. solve_all().is_empty() ↔ solve().is_none() ───────────

    #[test]
    fn solve_all_then_solve_consistent(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let all = dlx.solve_all();
        let single = dlx.solve();

        let all_empty = all.is_empty();
        let single_none = single.is_none();
        prop_assert_eq!(all_empty, single_none, "solve_all empty={} but solve none={}", all_empty, single_none);
    }

    // ── 18. solve_limited monotone count ─────────────────────────

    #[test]
    fn solve_limited_monotone_count(
        (num_cols, rows) in sparse_matrix_strategy(8, 6),
        n in 1usize..5
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let count_n = dlx.solve_limited(n).len();
        let count_n1 = dlx.solve_limited(n + 1).len();

        prop_assert!(
            count_n <= count_n1,
            "solve_limited({}) = {} > solve_limited({}) = {}",
            n, count_n, n + 1, count_n1
        );
    }

    // ── 19. identity matrix has exactly 1 solution ───────────────

    #[test]
    fn identity_unique_solution(n in 1usize..10) {
        let mut dlx = DancingLinks::new(n);
        for i in 0..n {
            dlx.add_row(&[i]);
        }

        let solutions = dlx.solve_all();
        prop_assert_eq!(solutions.len(), 1usize, "identity {}x{} should have exactly 1 solution", n, n);

        let mut sol_sorted = solutions[0].clone();
        sol_sorted.sort();
        let expected: Vec<usize> = (0..n).collect();
        prop_assert_eq!(sol_sorted, expected);
    }

    // ── 20. every solution covers all columns exactly once ───────

    #[test]
    fn all_solutions_cover_all_columns(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let solutions = dlx.solve_all();
        for solution in &solutions {
            let mut col_counts = vec![0usize; num_cols];
            for &row_idx in solution {
                prop_assert!(row_idx < rows.len(), "row index out of bounds: {}", row_idx);
                for &col in &rows[row_idx] {
                    col_counts[col] += 1;
                }
            }
            for (col, &count) in col_counts.iter().enumerate() {
                prop_assert_eq!(count, 1usize, "column {} covered {} times (expected 1)", col, count);
            }
        }
    }

    // ── 21. every solution from solve_limited is a valid exact cover ──

    #[test]
    fn solve_limited_all_valid(
        (num_cols, rows) in sparse_matrix_strategy(8, 6),
        limit in 1usize..8
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let limited = dlx.solve_limited(limit);
        for solution in &limited {
            // Check row indices in range
            for &row_idx in solution {
                prop_assert!(row_idx < rows.len(), "row_idx {} out of range", row_idx);
            }

            // Check exact cover: each column exactly once
            let mut col_counts = vec![0usize; num_cols];
            for &row_idx in solution {
                for &col in &rows[row_idx] {
                    col_counts[col] += 1;
                }
            }
            for (col, &count) in col_counts.iter().enumerate() {
                prop_assert_eq!(count, 1usize, "solve_limited: col {} covered {} times", col, count);
            }
        }
    }

    // ── 22. serde roundtrip after solve preserves solve_all ──────

    #[test]
    fn serde_after_solve(
        (num_cols, rows) in sparse_matrix_strategy(6, 5)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        // Solve first (internally covers/uncovers)
        let _first = dlx.solve();

        // Roundtrip after solve
        let json = serde_json::to_string(&dlx).unwrap();
        let mut restored: DancingLinks = serde_json::from_str(&json).unwrap();

        // Compare solve_all on original vs restored
        let orig_all = dlx.solve_all();
        let rest_all = restored.solve_all();

        let mut orig_norm: Vec<Vec<usize>> = orig_all.iter().map(|s| {
            let mut sorted = s.clone();
            sorted.sort();
            sorted
        }).collect();
        let mut rest_norm: Vec<Vec<usize>> = rest_all.iter().map(|s| {
            let mut sorted = s.clone();
            sorted.sort();
            sorted
        }).collect();
        orig_norm.sort();
        rest_norm.sort();

        prop_assert_eq!(orig_norm, rest_norm);
    }

    // ── 23. all-false matrix: rows reserved, no solutions ────────

    #[test]
    fn from_matrix_all_false_no_solutions(n_rows in 1usize..6, n_cols in 1usize..6) {
        let matrix: Vec<Vec<bool>> = vec![vec![false; n_cols]; n_rows];
        let mut dlx = DancingLinks::from_matrix(&matrix);

        // from_matrix calls add_row for every row, even all-false ones.
        // add_row(&[]) reserves a row index, so num_rows == n_rows.
        prop_assert_eq!(dlx.num_rows(), n_rows, "all-false rows should still reserve indices");

        // With columns but no covering rows, there is no way to cover any column
        let result = dlx.solve();
        prop_assert!(result.is_none(), "all-false matrix with {} cols should have no solution", n_cols);
    }

    // ── 24. solve twice returns same result (idempotent) ─────────

    #[test]
    fn solve_twice_idempotent(
        (num_cols, rows) in sparse_matrix_strategy(8, 6)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let sol1 = dlx.solve();
        let sol2 = dlx.solve();

        prop_assert_eq!(sol1, sol2, "solve() called twice returned different results");
    }

    // ── 25. add_row(&[]) reserves index (no nodes added) ────────

    #[test]
    fn add_row_empty_reserves_index(
        num_cols in 1usize..8,
        n_empty in 1usize..5,
        n_real in 0usize..4
    ) {
        let mut dlx = DancingLinks::new(num_cols);

        // Add some real rows first
        for i in 0..n_real {
            dlx.add_row(&[i % num_cols]);
        }
        let rows_before = dlx.num_rows();
        prop_assert_eq!(rows_before, n_real, "real rows mismatch before empty adds");

        // Empty rows still reserve their row index (no nodes added though)
        for _ in 0..n_empty {
            dlx.add_row(&[]);
        }
        let rows_after = dlx.num_rows();
        prop_assert_eq!(rows_after, rows_before + n_empty,
            "add_row(&[]) should reserve index, incrementing num_rows");
    }
}

// ══════════════════════════════════════════════════════════════════════
// ── ADDITIONAL PROPERTY TESTS (26–31) ──────────────────────────────
// ══════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── 26. Default DancingLinks is empty: 0 rows, 0 cols ────────

    #[test]
    fn new_zero_columns_is_empty(num_cols in 0usize..10) {
        let dlx = DancingLinks::new(num_cols);

        let cols = dlx.num_columns();
        let rows = dlx.num_rows();
        prop_assert_eq!(cols, num_cols);
        prop_assert_eq!(rows, 0usize, "freshly created DLX should have 0 rows");

        // is_empty semantics: num_rows == 0
        let is_empty = dlx.num_rows() == 0;
        prop_assert!(is_empty, "new DancingLinks should be empty");
    }

    // ── 27. Debug format is non-empty and contains struct name ───

    #[test]
    fn debug_format_nonempty(
        (num_cols, rows) in sparse_matrix_strategy(6, 5)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let debug_str = format!("{:?}", dlx);
        prop_assert!(!debug_str.is_empty(), "Debug output should not be empty");
        prop_assert!(
            debug_str.contains("DancingLinks"),
            "Debug output should contain 'DancingLinks', got: {}",
            &debug_str[..debug_str.len().min(100)]
        );
    }

    // ── 28. Display format includes dimensions ───────────────────

    #[test]
    fn display_format_includes_dimensions(
        (num_cols, rows) in sparse_matrix_strategy(6, 5)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let display_str = format!("{}", dlx);
        let num_rows = dlx.num_rows();
        let expected_dim = format!("{}x{}", num_rows, num_cols);
        prop_assert!(
            display_str.contains(&expected_dim),
            "Display should contain '{}', got '{}'",
            expected_dim,
            display_str
        );
        prop_assert!(
            display_str.contains("nodes"),
            "Display should contain 'nodes', got '{}'",
            display_str
        );
    }

    // ── 29. Clone independence: mutating clone doesn't affect original ──

    #[test]
    fn clone_independence_after_mutation(
        (num_cols, rows) in sparse_matrix_strategy(6, 5)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let orig_json = serde_json::to_string(&dlx).unwrap();
        let mut cloned = dlx.clone();

        // Mutate the clone by solving (which covers/uncovers internally)
        let _clone_sol = cloned.solve_all();

        // Original should be unchanged
        let after_json = serde_json::to_string(&dlx).unwrap();
        prop_assert_eq!(
            orig_json,
            after_json,
            "original DLX state should not change when clone is mutated"
        );
    }

    // ── 30. with_names preserves column count and solvability ────

    #[test]
    fn with_names_preserves_structure(n in 1usize..8) {
        let names: Vec<String> = (0..n).map(|i| format!("col_{}", i)).collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();

        let mut dlx_named = DancingLinks::with_names(&name_refs);
        let mut dlx_plain = DancingLinks::new(n);

        prop_assert_eq!(dlx_named.num_columns(), dlx_plain.num_columns());
        prop_assert_eq!(dlx_named.num_rows(), dlx_plain.num_rows());

        // Add identity rows to both and verify same solvability
        for i in 0..n {
            dlx_named.add_row(&[i]);
            dlx_plain.add_row(&[i]);
        }

        let named_sol = dlx_named.solve();
        let plain_sol = dlx_plain.solve();
        let named_some = named_sol.is_some();
        let plain_some = plain_sol.is_some();
        prop_assert_eq!(
            named_some,
            plain_some,
            "with_names and new should produce same solvability"
        );
    }

    // ── 31. solve_all restores internal state (serialization stable) ──

    #[test]
    fn solve_all_restores_state(
        (num_cols, rows) in sparse_matrix_strategy(6, 5)
    ) {
        let mut dlx = DancingLinks::new(num_cols);
        for row in &rows {
            dlx.add_row(row);
        }

        let before = serde_json::to_string(&dlx).unwrap();
        let _solutions = dlx.solve_all();
        let after = serde_json::to_string(&dlx).unwrap();

        prop_assert_eq!(
            before,
            after,
            "solve_all should fully restore internal state"
        );
    }
}
