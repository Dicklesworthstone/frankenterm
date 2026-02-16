//! Property-based tests for `dancing_links` module.
//!
//! Verifies correctness invariants:
//! - Solutions are valid exact covers (each column hit exactly once)
//! - solve() and solve_all() agree on solvability
//! - solve_limited respects limit
//! - Identity matrices always solvable
//! - Serde roundtrip

use frankenterm_core::dancing_links::DancingLinks;
use proptest::prelude::*;

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
fn verify_exact_cover(
    num_cols: usize,
    rows: &[Vec<usize>],
    solution: &[usize],
) -> bool {
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
        let mut seen = vec![false; 6];
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
}
