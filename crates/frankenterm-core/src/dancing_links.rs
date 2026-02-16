//! Dancing Links (DLX) — Knuth's Algorithm X with efficient backtracking.
//!
//! Solves the exact cover problem: given a binary matrix, find a subset
//! of rows such that each column has exactly one 1. Uses doubly-linked
//! circular lists that can be efficiently "covered" and "uncovered"
//! for backtracking search.
//!
//! # Complexity
//!
//! - **Exact cover**: NP-complete in general, but efficient in practice
//!   for sparse matrices with good column-selection heuristics
//! - **Cover/uncover**: O(1) per link operation
//!
//! # Design
//!
//! Arena-allocated nodes with 4-directional links (up, down, left, right).
//! Column headers track size for the "minimum remaining values" heuristic.
//! Solutions are collected via recursive search with cover/uncover.
//!
//! # Use in FrankenTerm
//!
//! Layout constraint solving (pane placement with non-overlap constraints),
//! resource allocation (assigning panes to capture threads), and
//! scheduling problems.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── DLX Node ──────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DlxNode {
    left: usize,
    right: usize,
    up: usize,
    down: usize,
    column: usize, // column header index
    row: usize,    // original row index (0 for headers)
}

// ── Column header ─────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ColumnHeader {
    node: usize, // index into nodes array
    size: usize, // number of 1s in this column
    name: String,
}

// ── DancingLinks ──────────────────────────────────────────────────────

/// Exact cover solver using Knuth's Algorithm X with Dancing Links.
///
/// Build a problem matrix with `add_row`, then call `solve` or
/// `solve_all` to find exact covers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DancingLinks {
    nodes: Vec<DlxNode>,
    columns: Vec<ColumnHeader>,
    num_columns: usize,
    num_rows: usize,
    root: usize, // root header node
}

impl DancingLinks {
    /// Creates a new DLX instance with the given number of columns.
    pub fn new(num_columns: usize) -> Self {
        let mut nodes = Vec::with_capacity(num_columns + 1);
        let mut columns = Vec::with_capacity(num_columns);

        // Root header node (index 0)
        nodes.push(DlxNode {
            left: num_columns,
            right: usize::from(num_columns > 0),
            up: 0,
            down: 0,
            column: 0,
            row: 0,
        });

        // Column header nodes (indices 1..=num_columns)
        for i in 0..num_columns {
            let idx = i + 1;
            let left = if i == 0 { 0 } else { i };
            let right = if i == num_columns - 1 { 0 } else { i + 2 };

            nodes.push(DlxNode {
                left,
                right,
                up: idx,
                down: idx,
                column: idx,
                row: 0,
            });

            columns.push(ColumnHeader {
                node: idx,
                size: 0,
                name: format!("C{}", i),
            });
        }

        Self {
            nodes,
            columns,
            num_columns,
            num_rows: 0,
            root: 0,
        }
    }

    /// Creates a DLX instance with named columns.
    pub fn with_names(names: &[&str]) -> Self {
        let mut dlx = Self::new(names.len());
        for (i, name) in names.iter().enumerate() {
            dlx.columns[i].name = name.to_string();
        }
        dlx
    }

    /// Adds a row to the problem matrix.
    ///
    /// `columns` is a list of column indices (0-based) that have a 1
    /// in this row.
    pub fn add_row(&mut self, columns: &[usize]) -> usize {
        if columns.is_empty() {
            return self.num_rows;
        }

        let row_id = self.num_rows + 1; // 1-based row IDs
        self.num_rows += 1;

        let first_node = self.nodes.len();

        for (i, &col) in columns.iter().enumerate() {
            assert!(col < self.num_columns, "column index out of bounds");
            let col_header = col + 1; // 1-based column headers

            let node_idx = self.nodes.len();

            // Insert node into column (circular vertical list)
            let col_up = self.nodes[col_header].up;

            self.nodes.push(DlxNode {
                left: if i == 0 {
                    first_node + columns.len() - 1
                } else {
                    node_idx - 1
                },
                right: if i == columns.len() - 1 {
                    first_node
                } else {
                    node_idx + 1
                },
                up: col_up,
                down: col_header,
                column: col_header,
                row: row_id,
            });

            self.nodes[col_up].down = node_idx;
            self.nodes[col_header].up = node_idx;

            self.columns[col].size += 1;
        }

        row_id - 1 // return 0-based row index
    }

    /// Builds a DLX instance from a complete binary matrix.
    ///
    /// Each inner vector represents a row, with `true` indicating a 1.
    pub fn from_matrix(matrix: &[Vec<bool>]) -> Self {
        if matrix.is_empty() {
            return Self::new(0);
        }

        let num_cols = matrix[0].len();
        let mut dlx = Self::new(num_cols);

        for row in matrix {
            let cols: Vec<usize> = row
                .iter()
                .enumerate()
                .filter(|(_, v)| **v)
                .map(|(i, _)| i)
                .collect();
            if !cols.is_empty() {
                dlx.add_row(&cols);
            }
        }

        dlx
    }

    /// Finds one exact cover solution.
    ///
    /// Returns `Some(rows)` where `rows` are the 0-based row indices
    /// forming an exact cover, or `None` if no solution exists.
    pub fn solve(&mut self) -> Option<Vec<usize>> {
        let mut solution = Vec::new();
        if self.search(&mut solution) {
            Some(solution)
        } else {
            None
        }
    }

    /// Finds all exact cover solutions.
    pub fn solve_all(&mut self) -> Vec<Vec<usize>> {
        let mut solutions = Vec::new();
        let mut partial = Vec::new();
        self.search_all(&mut partial, &mut solutions);
        solutions
    }

    /// Finds up to `limit` exact cover solutions.
    pub fn solve_limited(&mut self, limit: usize) -> Vec<Vec<usize>> {
        let mut solutions = Vec::new();
        let mut partial = Vec::new();
        self.search_limited(&mut partial, &mut solutions, limit);
        solutions
    }

    /// Returns the number of columns.
    pub fn num_columns(&self) -> usize {
        self.num_columns
    }

    /// Returns the number of rows added.
    pub fn num_rows(&self) -> usize {
        self.num_rows
    }

    // ── Algorithm X core ──────────────────────────────────────────

    fn search(&mut self, solution: &mut Vec<usize>) -> bool {
        if self.nodes[self.root].right == self.root {
            return true; // All columns covered
        }

        let col = self.choose_column();
        if self.columns[col - 1].size == 0 {
            return false; // Dead end
        }

        self.cover(col);

        let mut found = false;
        let mut row_node = self.nodes[col].down;
        while row_node != col {
            let row_id = self.nodes[row_node].row - 1; // 0-based
            solution.push(row_id);

            // Cover all other columns in this row
            let mut j = self.nodes[row_node].right;
            while j != row_node {
                self.cover(self.nodes[j].column);
                j = self.nodes[j].right;
            }

            if self.search(solution) {
                found = true;
            }

            // Undo: uncover columns in reverse (always, even on success)
            if !found {
                solution.pop();
            }
            let mut j = self.nodes[row_node].left;
            while j != row_node {
                self.uncover(self.nodes[j].column);
                j = self.nodes[j].left;
            }

            if found {
                break;
            }

            row_node = self.nodes[row_node].down;
        }

        self.uncover(col);
        found
    }

    fn search_all(&mut self, partial: &mut Vec<usize>, solutions: &mut Vec<Vec<usize>>) {
        if self.nodes[self.root].right == self.root {
            solutions.push(partial.clone());
            return;
        }

        let col = self.choose_column();
        if self.columns[col - 1].size == 0 {
            return;
        }

        self.cover(col);

        let mut row_node = self.nodes[col].down;
        while row_node != col {
            let row_id = self.nodes[row_node].row - 1;
            partial.push(row_id);

            let mut j = self.nodes[row_node].right;
            while j != row_node {
                self.cover(self.nodes[j].column);
                j = self.nodes[j].right;
            }

            self.search_all(partial, solutions);

            partial.pop();
            let mut j = self.nodes[row_node].left;
            while j != row_node {
                self.uncover(self.nodes[j].column);
                j = self.nodes[j].left;
            }

            row_node = self.nodes[row_node].down;
        }

        self.uncover(col);
    }

    fn search_limited(
        &mut self,
        partial: &mut Vec<usize>,
        solutions: &mut Vec<Vec<usize>>,
        limit: usize,
    ) {
        if solutions.len() >= limit {
            return;
        }

        if self.nodes[self.root].right == self.root {
            solutions.push(partial.clone());
            return;
        }

        let col = self.choose_column();
        if self.columns[col - 1].size == 0 {
            return;
        }

        self.cover(col);

        let mut row_node = self.nodes[col].down;
        while row_node != col {
            if solutions.len() >= limit {
                break;
            }

            let row_id = self.nodes[row_node].row - 1;
            partial.push(row_id);

            let mut j = self.nodes[row_node].right;
            while j != row_node {
                self.cover(self.nodes[j].column);
                j = self.nodes[j].right;
            }

            self.search_limited(partial, solutions, limit);

            partial.pop();
            let mut j = self.nodes[row_node].left;
            while j != row_node {
                self.uncover(self.nodes[j].column);
                j = self.nodes[j].left;
            }

            row_node = self.nodes[row_node].down;
        }

        self.uncover(col);
    }

    /// Choose column with minimum size (MRV heuristic).
    fn choose_column(&self) -> usize {
        let mut best = self.nodes[self.root].right;
        let mut best_size = self.columns[best - 1].size;

        let mut col = self.nodes[best].right;
        while col != self.root {
            if self.columns[col - 1].size < best_size {
                best = col;
                best_size = self.columns[col - 1].size;
            }
            col = self.nodes[col].right;
        }
        best
    }

    /// Cover a column: remove it from headers and remove all rows
    /// that have a 1 in this column.
    fn cover(&mut self, col: usize) {
        // Remove column header from horizontal list
        let left = self.nodes[col].left;
        let right = self.nodes[col].right;
        self.nodes[left].right = right;
        self.nodes[right].left = left;

        // For each row in this column
        let mut row = self.nodes[col].down;
        while row != col {
            // Remove each node in this row from its column
            let mut j = self.nodes[row].right;
            while j != row {
                let up = self.nodes[j].up;
                let down = self.nodes[j].down;
                self.nodes[up].down = down;
                self.nodes[down].up = up;
                self.columns[self.nodes[j].column - 1].size -= 1;
                j = self.nodes[j].right;
            }
            row = self.nodes[row].down;
        }
    }

    /// Uncover a column: restore it (reverse of cover).
    fn uncover(&mut self, col: usize) {
        let mut row = self.nodes[col].up;
        while row != col {
            let mut j = self.nodes[row].left;
            while j != row {
                self.columns[self.nodes[j].column - 1].size += 1;
                let up = self.nodes[j].up;
                let down = self.nodes[j].down;
                self.nodes[up].down = j;
                self.nodes[down].up = j;
                j = self.nodes[j].left;
            }
            row = self.nodes[row].up;
        }

        let left = self.nodes[col].left;
        let right = self.nodes[col].right;
        self.nodes[left].right = col;
        self.nodes[right].left = col;
    }
}

impl fmt::Display for DancingLinks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DancingLinks({}x{}, {} nodes)",
            self.num_rows,
            self.num_columns,
            self.nodes.len()
        )
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_matrix() {
        let mut dlx = DancingLinks::new(3);
        let result = dlx.solve();
        // Empty matrix with columns means no exact cover possible
        // Actually, with 0 rows and 3 columns, it's unsolvable
        assert!(result.is_none());
    }

    #[test]
    fn identity_matrix() {
        // 3x3 identity = one exact cover (all rows)
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        dlx.add_row(&[2]);

        let solution = dlx.solve().unwrap();
        assert_eq!(solution.len(), 3);
        let mut sorted = solution.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2]);
    }

    #[test]
    fn knuth_example() {
        // Classic example from Knuth's paper
        // Columns: 0 1 2 3 4 5 6
        // Row 0: 0 0 1 0 1 1 0
        // Row 1: 1 0 0 1 0 0 1
        // Row 2: 0 1 1 0 0 1 0
        // Row 3: 1 0 0 1 0 0 0
        // Row 4: 0 1 0 0 0 0 1
        // Row 5: 0 0 0 1 1 0 1
        // Solution: rows {0, 3, 4} or similar
        let mut dlx = DancingLinks::new(7);
        dlx.add_row(&[2, 4, 5]); // row 0
        dlx.add_row(&[0, 3, 6]); // row 1
        dlx.add_row(&[1, 2, 5]); // row 2
        dlx.add_row(&[0, 3]); // row 3
        dlx.add_row(&[1, 6]); // row 4
        dlx.add_row(&[3, 4, 6]); // row 5

        let solution = dlx.solve().unwrap();

        // Verify it's a valid exact cover
        let mut covered = [false; 7];
        for &row in &solution {
            let cols = match row {
                0 => vec![2, 4, 5],
                1 => vec![0, 3, 6],
                2 => vec![1, 2, 5],
                3 => vec![0, 3],
                4 => vec![1, 6],
                5 => vec![3, 4, 6],
                _ => panic!("unexpected row"),
            };
            for col in cols {
                assert!(!covered[col], "column {} covered twice", col);
                covered[col] = true;
            }
        }
        assert!(covered.iter().all(|&c| c), "not all columns covered");
    }

    #[test]
    fn no_solution() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[0, 2]);
        // Column 0 is in both rows, so no exact cover can use both
        // And columns 1, 2 need separate rows
        let result = dlx.solve();
        assert!(result.is_none());
    }

    #[test]
    fn multiple_solutions() {
        // Two possible covers
        let mut dlx = DancingLinks::new(2);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);

        let solutions = dlx.solve_all();
        // Solutions: {0,1}, {0,3}, {2,1}, {2,3}
        assert_eq!(solutions.len(), 4);
    }

    #[test]
    fn solve_limited() {
        let mut dlx = DancingLinks::new(2);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);

        let solutions = dlx.solve_limited(2);
        assert_eq!(solutions.len(), 2);
    }

    #[test]
    fn from_matrix() {
        let matrix = vec![
            vec![true, false, true],
            vec![false, true, false],
            vec![true, false, false],
            vec![false, false, true],
        ];
        let mut dlx = DancingLinks::from_matrix(&matrix);
        let solution = dlx.solve();

        // Row 1 covers col 1, rows 2+3 cover cols 0,2
        // or row 0 covers cols 0,2 and row 1 covers col 1
        assert!(solution.is_some());
        let sol = solution.unwrap();

        let mut covered = [false; 3];
        for &row in &sol {
            for (col, &val) in matrix[row].iter().enumerate() {
                if val {
                    assert!(!covered[col]);
                    covered[col] = true;
                }
            }
        }
        assert!(covered.iter().all(|&c| c));
    }

    #[test]
    fn with_names() {
        let dlx = DancingLinks::with_names(&["A", "B", "C"]);
        assert_eq!(dlx.num_columns(), 3);
        assert_eq!(dlx.columns[0].name, "A");
        assert_eq!(dlx.columns[2].name, "C");
    }

    #[test]
    fn single_row_covering_all() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0, 1, 2]);
        let solution = dlx.solve().unwrap();
        assert_eq!(solution, vec![0]);
    }

    #[test]
    fn num_rows_num_columns() {
        let mut dlx = DancingLinks::new(5);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[2, 3]);
        dlx.add_row(&[4]);
        assert_eq!(dlx.num_columns(), 5);
        assert_eq!(dlx.num_rows(), 3);
    }

    #[test]
    fn display_format() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[2]);
        let display = format!("{}", dlx);
        assert!(display.contains("2x3"));
    }

    #[test]
    fn serde_roundtrip() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[1, 2]);
        dlx.add_row(&[0]);
        dlx.add_row(&[2]);

        let json = serde_json::to_string(&dlx).unwrap();
        let mut restored: DancingLinks = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.num_columns(), dlx.num_columns());
        assert_eq!(restored.num_rows(), dlx.num_rows());

        // Both should find same solutions
        let orig_solutions = dlx.solve_all();
        let rest_solutions = restored.solve_all();
        assert_eq!(orig_solutions.len(), rest_solutions.len());
    }

    #[test]
    fn solve_all_consistency() {
        // Every solution in solve_all should be a valid exact cover
        let mut dlx = DancingLinks::new(4);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[2, 3]);
        dlx.add_row(&[0, 2]);
        dlx.add_row(&[1, 3]);

        let solutions = dlx.solve_all();
        for solution in &solutions {
            let mut covered = [false; 4];
            for &row in solution {
                let cols = match row {
                    0 => vec![0, 1],
                    1 => vec![2, 3],
                    2 => vec![0, 2],
                    3 => vec![1, 3],
                    _ => panic!(),
                };
                for col in cols {
                    assert!(!covered[col], "double cover in solution");
                    covered[col] = true;
                }
            }
            assert!(covered.iter().all(|&c| c), "incomplete cover");
        }
    }

    // ── New expanded tests ──────────────────────────────────────────

    #[test]
    fn zero_columns_trivial_cover() {
        // Zero-column matrix: trivially solved (empty set covers nothing)
        let mut dlx = DancingLinks::new(0);
        assert_eq!(dlx.num_columns(), 0);
        assert_eq!(dlx.num_rows(), 0);
        let solution = dlx.solve().unwrap();
        assert!(solution.is_empty());
    }

    #[test]
    fn zero_columns_solve_all() {
        let mut dlx = DancingLinks::new(0);
        let solutions = dlx.solve_all();
        assert_eq!(solutions.len(), 1);
        assert!(solutions[0].is_empty());
    }

    #[test]
    fn solve_limited_zero() {
        // limit=0 should return no solutions
        let mut dlx = DancingLinks::new(2);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        let solutions = dlx.solve_limited(0);
        assert!(solutions.is_empty());
    }

    #[test]
    fn solve_limited_one() {
        // limit=1 should return exactly one solution
        let mut dlx = DancingLinks::new(2);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        // 4 possible solutions but we only want 1
        let solutions = dlx.solve_limited(1);
        assert_eq!(solutions.len(), 1);
    }

    #[test]
    fn add_row_returns_sequential_indices() {
        let mut dlx = DancingLinks::new(3);
        assert_eq!(dlx.add_row(&[0, 1]), 0);
        assert_eq!(dlx.add_row(&[2]), 1);
        assert_eq!(dlx.add_row(&[0, 2]), 2);
        assert_eq!(dlx.num_rows(), 3);
    }

    #[test]
    fn add_empty_row_noop() {
        // Empty column list should be a no-op (no nodes added)
        let mut dlx = DancingLinks::new(3);
        let before_nodes = dlx.nodes.len();
        let idx = dlx.add_row(&[]);
        assert_eq!(idx, 0); // returns num_rows (0)
        assert_eq!(dlx.num_rows(), 0); // not incremented
        assert_eq!(dlx.nodes.len(), before_nodes); // no nodes added
    }

    #[test]
    fn solve_on_clone_same_result() {
        // solve() leaves internal state modified (columns covered).
        // Verify that cloning before solve gives consistent results.
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[2]);
        dlx.add_row(&[0]);
        dlx.add_row(&[1, 2]);

        let mut clone1 = dlx.clone();
        let mut clone2 = dlx.clone();
        let sol1 = clone1.solve();
        let sol2 = clone2.solve();
        assert_eq!(sol1, sol2);
    }

    #[test]
    fn solve_all_twice_same_result() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        dlx.add_row(&[2]);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[1, 2]);

        let all1 = dlx.solve_all();
        let all2 = dlx.solve_all();
        assert_eq!(all1.len(), all2.len());
        assert_eq!(all1, all2);
    }

    #[test]
    fn solve_all_no_duplicates() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        dlx.add_row(&[2]);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        dlx.add_row(&[2]);

        let solutions = dlx.solve_all();
        // Normalize: sort each solution so order doesn't matter
        let normalized: Vec<Vec<usize>> = solutions
            .iter()
            .map(|s| {
                let mut sorted = s.clone();
                sorted.sort();
                sorted
            })
            .collect();
        // Check no two normalized solutions are identical
        for i in 0..normalized.len() {
            for j in (i + 1)..normalized.len() {
                assert_ne!(normalized[i], normalized[j], "duplicate solution found");
            }
        }
    }

    #[test]
    fn from_matrix_empty() {
        let mut dlx = DancingLinks::from_matrix(&[]);
        assert_eq!(dlx.num_columns(), 0);
        assert_eq!(dlx.num_rows(), 0);
        let solution = dlx.solve().unwrap();
        assert!(solution.is_empty());
    }

    #[test]
    fn from_matrix_all_false_rows_skipped() {
        // Rows with no 1s should be skipped
        let matrix = vec![
            vec![false, false, false],
            vec![true, false, false],
            vec![false, false, false],
            vec![false, true, true],
        ];
        let mut dlx = DancingLinks::from_matrix(&matrix);
        // Only rows 1 and 3 have 1s
        assert_eq!(dlx.num_rows(), 2);
        let solution = dlx.solve().unwrap();
        // Solution covers all 3 columns with 2 rows
        assert_eq!(solution.len(), 2);
    }

    #[test]
    fn from_matrix_single_cell_true() {
        let matrix = vec![vec![true]];
        let mut dlx = DancingLinks::from_matrix(&matrix);
        assert_eq!(dlx.num_columns(), 1);
        assert_eq!(dlx.num_rows(), 1);
        let solution = dlx.solve().unwrap();
        assert_eq!(solution, vec![0]);
    }

    #[test]
    fn from_matrix_single_cell_false() {
        let matrix = vec![vec![false]];
        let mut dlx = DancingLinks::from_matrix(&matrix);
        assert_eq!(dlx.num_columns(), 1);
        assert_eq!(dlx.num_rows(), 0);
        // No rows to cover column 0
        assert!(dlx.solve().is_none());
    }

    #[test]
    fn all_ones_row_wins() {
        // Single row covering all columns should be the only solution
        let mut dlx = DancingLinks::new(4);
        dlx.add_row(&[0, 1, 2, 3]); // covers everything
        dlx.add_row(&[0, 1]); // partial
        dlx.add_row(&[2, 3]); // partial

        let solutions = dlx.solve_all();
        // {0} and {1,2} are both valid
        assert_eq!(solutions.len(), 2);
    }

    #[test]
    fn all_ones_row_sole_solution() {
        // When partial rows overlap, only the full row works
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0, 1, 2]); // covers everything
        dlx.add_row(&[0, 1]); // partial, overlaps
        dlx.add_row(&[1, 2]); // partial, overlaps

        let solutions = dlx.solve_all();
        assert_eq!(solutions.len(), 1);
        assert_eq!(solutions[0], vec![0]);
    }

    #[test]
    fn large_identity_matrix() {
        // 20x20 identity: exactly one solution (all rows)
        let n = 20;
        let mut dlx = DancingLinks::new(n);
        for i in 0..n {
            dlx.add_row(&[i]);
        }

        let solution = dlx.solve().unwrap();
        assert_eq!(solution.len(), n);
        let mut sorted = solution.clone();
        sorted.sort();
        assert_eq!(sorted, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn large_sparse_block_diagonal() {
        // 4 independent 2x2 blocks = 1 solution per block = 1 total
        let mut dlx = DancingLinks::new(8);
        // Block 0: cols 0,1
        dlx.add_row(&[0, 1]);
        // Block 1: cols 2,3
        dlx.add_row(&[2, 3]);
        // Block 2: cols 4,5
        dlx.add_row(&[4, 5]);
        // Block 3: cols 6,7
        dlx.add_row(&[6, 7]);

        let solution = dlx.solve().unwrap();
        assert_eq!(solution.len(), 4);
    }

    #[test]
    fn pentomino_style_competing_rows() {
        // 6 columns, multiple overlapping rows, verify all solutions valid
        let mut dlx = DancingLinks::new(6);
        dlx.add_row(&[0, 1, 2]); // 0
        dlx.add_row(&[3, 4, 5]); // 1
        dlx.add_row(&[0, 3]); // 2
        dlx.add_row(&[1, 4]); // 3
        dlx.add_row(&[2, 5]); // 4
        dlx.add_row(&[0, 1]); // 5
        dlx.add_row(&[2, 3]); // 6
        dlx.add_row(&[4, 5]); // 7

        let row_cols: Vec<Vec<usize>> = vec![
            vec![0, 1, 2],
            vec![3, 4, 5],
            vec![0, 3],
            vec![1, 4],
            vec![2, 5],
            vec![0, 1],
            vec![2, 3],
            vec![4, 5],
        ];

        let solutions = dlx.solve_all();
        assert!(!solutions.is_empty());

        for solution in &solutions {
            let mut covered = [false; 6];
            for &row in solution {
                for &col in &row_cols[row] {
                    assert!(!covered[col], "column {} double-covered", col);
                    covered[col] = true;
                }
            }
            assert!(covered.iter().all(|&c| c), "not all columns covered");
        }
    }

    #[test]
    fn serde_roundtrip_preserves_solutions() {
        let mut dlx = DancingLinks::new(4);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[2, 3]);
        dlx.add_row(&[0, 2]);
        dlx.add_row(&[1, 3]);
        dlx.add_row(&[0, 3]);
        dlx.add_row(&[1, 2]);

        let orig_solutions = dlx.solve_all();

        let json = serde_json::to_string(&dlx).unwrap();
        let mut restored: DancingLinks = serde_json::from_str(&json).unwrap();
        let rest_solutions = restored.solve_all();

        // Normalize and compare
        let mut orig_norm: Vec<Vec<usize>> = orig_solutions
            .iter()
            .map(|s| {
                let mut sorted = s.clone();
                sorted.sort();
                sorted
            })
            .collect();
        let mut rest_norm: Vec<Vec<usize>> = rest_solutions
            .iter()
            .map(|s| {
                let mut sorted = s.clone();
                sorted.sort();
                sorted
            })
            .collect();
        orig_norm.sort();
        rest_norm.sort();
        assert_eq!(orig_norm, rest_norm);
    }

    #[test]
    fn serde_roundtrip_before_solve() {
        // Serde roundtrip on pristine (unsolved) DLX preserves solvability.
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        dlx.add_row(&[2]);

        let json = serde_json::to_string(&dlx).unwrap();
        let mut restored: DancingLinks = serde_json::from_str(&json).unwrap();
        let sol = restored.solve().unwrap();
        assert_eq!(sol.len(), 3);
    }

    #[test]
    fn solve_all_restores_state() {
        // solve_all() fully backtracks, so state should be restored
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        dlx.add_row(&[2]);

        let before = serde_json::to_string(&dlx).unwrap();
        let _solutions = dlx.solve_all();
        let after = serde_json::to_string(&dlx).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn display_various_sizes() {
        let dlx0 = DancingLinks::new(0);
        assert_eq!(format!("{}", dlx0), "DancingLinks(0x0, 1 nodes)");

        let dlx1 = DancingLinks::new(1);
        assert!(format!("{}", dlx1).contains("0x1"));

        let mut dlx5 = DancingLinks::new(5);
        dlx5.add_row(&[0, 1, 2, 3, 4]);
        let display = format!("{}", dlx5);
        assert!(display.contains("1x5"));
        assert!(display.contains("nodes"));
    }

    #[test]
    fn with_names_default_names() {
        let dlx = DancingLinks::new(3);
        assert_eq!(dlx.columns[0].name, "C0");
        assert_eq!(dlx.columns[1].name, "C1");
        assert_eq!(dlx.columns[2].name, "C2");
    }

    #[test]
    fn single_column_many_rows() {
        // One column, multiple rows — each row alone is a solution
        let mut dlx = DancingLinks::new(1);
        for _ in 0..5 {
            dlx.add_row(&[0]);
        }
        assert_eq!(dlx.num_rows(), 5);

        let solutions = dlx.solve_all();
        // Each row individually is a valid solution
        assert_eq!(solutions.len(), 5);
        for sol in &solutions {
            assert_eq!(sol.len(), 1);
        }
    }

    #[test]
    fn solve_limited_exceeds_total() {
        // limit > total solutions → returns all
        let mut dlx = DancingLinks::new(2);
        dlx.add_row(&[0, 1]);
        // Only 1 solution
        let solutions = dlx.solve_limited(100);
        assert_eq!(solutions.len(), 1);
    }

    #[test]
    fn column_count_after_add_row() {
        let mut dlx = DancingLinks::new(5);
        dlx.add_row(&[0, 4]);
        dlx.add_row(&[1, 2, 3]);
        // Adding rows doesn't change column count
        assert_eq!(dlx.num_columns(), 5);
    }

    #[test]
    #[should_panic(expected = "column index out of bounds")]
    fn add_row_out_of_bounds_panics() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[3]); // column 3 doesn't exist (0..2)
    }

    #[test]
    fn four_queens_as_exact_cover() {
        // Encode 4-queens as exact cover:
        // 4 rows + 4 cols + diagonals
        // Columns: R0..R3 (row constraints), C0..C3 (col constraints)
        //   + diag constraints (optional, we'll use mandatory row+col only)
        // Each queen placement (r, c) covers row-r and col-c
        let mut dlx = DancingLinks::new(8); // R0..R3, C0..C3

        // All 16 possible placements
        for r in 0..4 {
            for c in 0..4 {
                dlx.add_row(&[r, 4 + c]); // covers row r, col c
            }
        }
        assert_eq!(dlx.num_rows(), 16);

        let solutions = dlx.solve_all();
        // Without diagonal constraints, this is just "one queen per row, one per col"
        // = number of permutations of {0,1,2,3} = 4! = 24
        assert_eq!(solutions.len(), 24);

        // Verify each solution has exactly 4 rows
        for sol in &solutions {
            assert_eq!(sol.len(), 4);
        }
    }

    #[test]
    fn clone_independence() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[2]);

        let mut cloned = dlx.clone();
        // Solving clone doesn't affect original
        let sol_clone = cloned.solve().unwrap();
        let sol_orig = dlx.solve().unwrap();
        assert_eq!(sol_clone, sol_orig);
    }

    #[test]
    fn from_matrix_correctness() {
        // Build via from_matrix and via add_row, compare solutions
        let matrix = vec![
            vec![true, false, true, false],
            vec![false, true, false, true],
            vec![true, true, false, false],
            vec![false, false, true, true],
        ];
        let mut dlx_matrix = DancingLinks::from_matrix(&matrix);

        let mut dlx_manual = DancingLinks::new(4);
        dlx_manual.add_row(&[0, 2]);
        dlx_manual.add_row(&[1, 3]);
        dlx_manual.add_row(&[0, 1]);
        dlx_manual.add_row(&[2, 3]);

        let mut sols_matrix = dlx_matrix.solve_all();
        let mut sols_manual = dlx_manual.solve_all();

        // Normalize
        for s in &mut sols_matrix {
            s.sort();
        }
        for s in &mut sols_manual {
            s.sort();
        }
        sols_matrix.sort();
        sols_manual.sort();

        assert_eq!(sols_matrix, sols_manual);
    }

    #[test]
    fn unsolvable_column_unreachable() {
        // Column 2 has no rows → unsolvable
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        // No row covers column 2

        assert!(dlx.solve().is_none());
        assert!(dlx.solve_all().is_empty());
    }

    #[test]
    fn solve_limited_respects_exact_limit() {
        // Create problem with many solutions
        let mut dlx = DancingLinks::new(1);
        for _ in 0..10 {
            dlx.add_row(&[0]);
        }
        // 10 solutions total
        assert_eq!(dlx.solve_all().len(), 10);

        // Request exactly 5
        let mut dlx2 = dlx.clone();
        let limited = dlx2.solve_limited(5);
        assert_eq!(limited.len(), 5);
    }

    #[test]
    fn solve_after_solve_all_consistent() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[2]);
        dlx.add_row(&[0]);
        dlx.add_row(&[1, 2]);

        let all_solutions = dlx.solve_all();
        let first = dlx.solve();

        // First solution from solve() should be one of the solve_all() solutions
        assert!(first.is_some());
        let first = first.unwrap();
        let mut first_sorted = first.clone();
        first_sorted.sort();

        let is_in_all = all_solutions.iter().any(|s| {
            let mut sorted = s.clone();
            sorted.sort();
            sorted == first_sorted
        });
        assert!(is_in_all, "solve() result not found in solve_all()");
    }

    #[test]
    fn choose_column_uses_minimum_size_heuristic() {
        let mut dlx = DancingLinks::new(4);
        dlx.add_row(&[0]);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[2, 3]);

        // Column sizes are [2, 1, 1, 1], so the first minimum is column 1 (header index 2).
        assert_eq!(dlx.choose_column(), 2);
    }

    #[test]
    fn cover_then_uncover_restores_serialized_state() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0, 1]);
        dlx.add_row(&[1, 2]);
        dlx.add_row(&[0, 2]);

        let before = serde_json::to_string(&dlx).unwrap();
        let chosen = dlx.choose_column();
        dlx.cover(chosen);
        dlx.uncover(chosen);
        let after = serde_json::to_string(&dlx).unwrap();

        assert_eq!(before, after);
    }

    #[test]
    fn single_column_without_rows_is_unsolvable() {
        let mut dlx = DancingLinks::new(1);
        assert!(dlx.solve().is_none());
        assert!(dlx.solve_all().is_empty());
    }

    #[test]
    fn single_row_partial_cover_is_unsolvable() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[1]);

        assert!(dlx.solve().is_none());
        assert!(dlx.solve_all().is_empty());
    }

    #[test]
    fn duplicate_full_rows_yield_distinct_single_row_solutions() {
        let mut dlx = DancingLinks::new(3);
        dlx.add_row(&[0, 1, 2]);
        dlx.add_row(&[0, 1, 2]);
        dlx.add_row(&[0, 1, 2]);

        let mut solutions = dlx.solve_all();
        for solution in &solutions {
            assert_eq!(solution.len(), 1);
        }
        for solution in &mut solutions {
            solution.sort();
        }
        solutions.sort();
        assert_eq!(solutions, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn solve_limited_then_solve_all_returns_full_solution_set() {
        let mut dlx = DancingLinks::new(2);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);
        dlx.add_row(&[0]);
        dlx.add_row(&[1]);

        let limited = dlx.solve_limited(2);
        assert_eq!(limited.len(), 2);

        let all = dlx.solve_all();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn with_names_empty_has_trivial_solution() {
        let mut dlx = DancingLinks::with_names(&[]);
        assert_eq!(dlx.num_columns(), 0);
        assert_eq!(dlx.num_rows(), 0);
        let solution = dlx.solve().unwrap();
        assert!(solution.is_empty());
    }

    #[test]
    #[should_panic(expected = "column index out of bounds")]
    fn from_matrix_jagged_longer_row_panics() {
        let matrix = vec![vec![true, false], vec![false, true, true]];
        let _ = DancingLinks::from_matrix(&matrix);
    }

    #[test]
    fn from_matrix_jagged_shorter_rows_treated_as_missing_false() {
        let matrix = vec![
            vec![true, false, false],
            vec![false], // shorter row: no true values, should be skipped
            vec![false, true, true],
        ];
        let mut dlx = DancingLinks::from_matrix(&matrix);
        assert_eq!(dlx.num_columns(), 3);
        assert_eq!(dlx.num_rows(), 2);

        let solution = dlx.solve().unwrap();
        assert_eq!(solution.len(), 2);
    }

    #[test]
    fn column_sizes_track_add_row_counts() {
        let mut dlx = DancingLinks::new(4);
        dlx.add_row(&[0, 2]);
        dlx.add_row(&[2, 3]);
        dlx.add_row(&[1]);

        assert_eq!(dlx.columns[0].size, 1);
        assert_eq!(dlx.columns[1].size, 1);
        assert_eq!(dlx.columns[2].size, 2);
        assert_eq!(dlx.columns[3].size, 1);
    }
}
