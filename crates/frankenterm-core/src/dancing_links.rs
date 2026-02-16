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
            right: if num_columns > 0 { 1 } else { 0 },
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
                return true;
            }

            // Undo: uncover columns in reverse
            solution.pop();
            let mut j = self.nodes[row_node].left;
            while j != row_node {
                self.uncover(self.nodes[j].column);
                j = self.nodes[j].left;
            }

            row_node = self.nodes[row_node].down;
        }

        self.uncover(col);
        false
    }

    fn search_all(
        &mut self,
        partial: &mut Vec<usize>,
        solutions: &mut Vec<Vec<usize>>,
    ) {
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
        dlx.add_row(&[2, 4, 5]);    // row 0
        dlx.add_row(&[0, 3, 6]);    // row 1
        dlx.add_row(&[1, 2, 5]);    // row 2
        dlx.add_row(&[0, 3]);       // row 3
        dlx.add_row(&[1, 6]);       // row 4
        dlx.add_row(&[3, 4, 6]);    // row 5

        let solution = dlx.solve().unwrap();

        // Verify it's a valid exact cover
        let mut covered = vec![false; 7];
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

        let mut covered = vec![false; 3];
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
            let mut covered = vec![false; 4];
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
}
