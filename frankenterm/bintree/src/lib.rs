//! This crate implements a binary tree with a Zipper based Cursor implementation.
//!
//! For more details on the Zipper concept, check out these resources:
//! * <https://www.st.cs.uni-saarland.de//edu/seminare/2005/advanced-fp/docs/huet-zipper.pdf>
//! * <https://donsbot.wordpress.com/2007/05/17/roll-your-own-window-manager-tracking-focus-with-a-zipper/>
//! * <https://stackoverflow.com/a/36168919/149111>

use std::cmp::PartialEq;
use std::fmt::Debug;

/// Represents a (mostly) "proper" binary tree; each Node has 0 or 2 children,
/// but there is a special case where the tree is rooted with a single leaf node.
/// Non-leaf nodes in the tree can be labelled with an optional node data type `N`,
/// which defaults to `()`.
/// Leaf nodes have a required leaf data type `L`.
pub enum Tree<L, N = ()> {
    Empty,
    Node {
        left: Box<Self>,
        right: Box<Self>,
        data: Option<N>,
    },
    Leaf(L),
}

impl<L, N> PartialEq for Tree<L, N>
where
    L: PartialEq,
    N: PartialEq,
{
    fn eq(&self, rhs: &Self) -> bool {
        match (self, rhs) {
            (Self::Empty, Self::Empty) => true,
            (
                Self::Node {
                    left: l_left,
                    right: l_right,
                    data: l_data,
                },
                Self::Node {
                    left: r_left,
                    right: r_right,
                    data: r_data,
                },
            ) => (l_left == r_left) && (l_right == r_right) && (l_data == r_data),
            (Self::Leaf(l), Self::Leaf(r)) => l == r,
            _ => false,
        }
    }
}

impl<L, N> Debug for Tree<L, N>
where
    L: Debug,
    N: Debug,
{
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        match self {
            Self::Empty => fmt.write_str("Empty"),
            Self::Node { left, right, data } => fmt
                .debug_struct("Node")
                .field("left", &left)
                .field("right", &right)
                .field("data", &data)
                .finish(),
            Self::Leaf(l) => fmt.debug_tuple("Leaf").field(&l).finish(),
        }
    }
}

/// Represents a location in the tree for the Zipper; the path contains directions
/// from the current position back towards the root of the tree.
enum Path<L, N> {
    /// The current position is the top of the tree
    Top,
    /// The current position is the left hand side of its parent node;
    /// Cursor::it holds the left node of the tree with the fields here
    /// in Path::Left representing the partially constructed state of
    /// the parent Tree::Node
    Left {
        right: Box<Tree<L, N>>,
        data: Option<N>,
        up: Box<Self>,
    },
    /// The current position is the right hand side of its parent node;
    /// Cursor::it holds the right node of the tree with the fields here
    /// in Path::Right representing the partially constructed state of
    /// the parent Tree::Node
    Right {
        left: Box<Tree<L, N>>,
        data: Option<N>,
        up: Box<Self>,
    },
}

impl<L, N> Debug for Path<L, N>
where
    L: Debug,
    N: Debug,
{
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        match self {
            Self::Top => fmt.write_str("Top"),
            Self::Left { right, data, up } => fmt
                .debug_struct("Left")
                .field("right", &right)
                .field("data", &data)
                .field("up", &up)
                .finish(),
            Self::Right { left, data, up } => fmt
                .debug_struct("Right")
                .field("left", &left)
                .field("data", &data)
                .field("up", &up)
                .finish(),
        }
    }
}

/// The cursor is used to indicate the current position within the tree and enable
/// constant time mutation operations on that position as well as movement around
/// the tree.
/// The cursor isn't a reference to a location within the tree; it is an alternate
/// representation of the tree and thus requires ownership of the tree to create.
/// When you are done using the cursor you may wish to transform it back into
/// a tree.
pub struct Cursor<L, N> {
    it: Box<Tree<L, N>>,
    path: Box<Path<L, N>>,
}

impl<L, N> Debug for Cursor<L, N>
where
    L: Debug,
    N: Debug,
{
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        fmt.debug_struct("Cursor")
            .field("it", &self.it)
            .field("path", &self.path)
            .finish()
    }
}

pub struct ParentIterator<'a, L, N> {
    path: &'a Path<L, N>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathBranch {
    IsLeft,
    IsRight,
}

impl<'a, L, N> std::iter::Iterator for ParentIterator<'a, L, N> {
    type Item = (PathBranch, &'a Option<N>);

    fn next(&mut self) -> Option<Self::Item> {
        match self.path {
            Path::Top => None,
            Path::Left { data, up, .. } => {
                self.path = up;
                Some((PathBranch::IsLeft, data))
            }
            Path::Right { data, up, .. } => {
                self.path = up;
                Some((PathBranch::IsRight, data))
            }
        }
    }
}

impl<L, N> Tree<L, N> {
    /// Construct a new empty tree
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::Empty
    }

    /// Returns true if the tree is empty
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Transform the tree into its Zipper based Cursor representation
    pub fn cursor(self) -> Cursor<L, N> {
        Cursor {
            it: Box::new(self),
            path: Box::new(Path::Top),
        }
    }

    pub fn num_leaves(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Leaf(_) => 1,
            Self::Node { left, right, .. } => left.num_leaves() + right.num_leaves(),
        }
    }
}

impl<L, N> Cursor<L, N> {
    /// Construct a cursor representing a new empty tree
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            it: Box::new(Tree::Empty),
            path: Box::new(Path::Top),
        }
    }

    /// References the subtree at the current cursor position
    pub fn subtree(&self) -> &Tree<L, N> {
        &self.it
    }

    /// Returns true if the current position is a leaf node
    pub fn is_leaf(&self) -> bool {
        matches!(&*self.it, Tree::Leaf(_))
    }

    /// Returns true if the current position is the left child of its parent
    pub fn is_left(&self) -> bool {
        matches!(&*self.path, Path::Left { .. })
    }

    /// Returns true if the current position is the right child of its parent
    pub fn is_right(&self) -> bool {
        matches!(&*self.path, Path::Right { .. })
    }

    pub fn is_top(&self) -> bool {
        matches!(&*self.path, Path::Top)
    }

    /// If the current position is the root of the empty tree,
    /// assign an initial leaf value.
    /// Consumes the cursor and returns a new cursor representing
    /// the mutated tree.
    /// If the current position isn't the top of the empty tree,
    /// yields `Err` containing the unchanged cursor.
    pub fn assign_top(self, leaf: L) -> Result<Self, Self> {
        match (&*self.it, &*self.path) {
            (Tree::Empty, Path::Top) => Ok(Self {
                it: Box::new(Tree::Leaf(leaf)),
                path: self.path,
            }),
            _ => Err(self),
        }
    }

    /// If the current position is a leaf node, return a mutable
    /// reference to the leaf data, else `None`.
    pub fn leaf_mut(&mut self) -> Option<&mut L> {
        match &mut *self.it {
            Tree::Leaf(l) => Some(l),
            _ => None,
        }
    }

    /// If the current position is not a leaf node, return a mutable
    /// reference to the node data container, else yields `Err`.
    #[allow(clippy::result_unit_err)]
    pub fn node_mut(&mut self) -> Result<&mut Option<N>, ()> {
        match &mut *self.it {
            Tree::Node { data, .. } => Ok(data),
            _ => Err(()),
        }
    }

    /// Return an iterator that will visit the chain of nodes leading
    /// to the root from the current position and yield their node
    /// data at each step of iteration.
    pub fn path_to_root(&self) -> ParentIterator<'_, L, N> {
        ParentIterator { path: &*self.path }
    }

    /// If the current position is not a leaf node, assign the
    /// node data to the supplied value.
    /// Consumes the cursor and returns a new cursor representing the
    /// mutated tree.
    /// If the current position is a leaf node then yields `Err`
    /// containing the unchanged cursor.
    pub fn assign_node(mut self, value: Option<N>) -> Result<Self, Self> {
        match &mut *self.it {
            Tree::Node { data, .. } => {
                *data = value;
                Ok(self)
            }
            _ => Err(self),
        }
    }

    /// If the current position is a non-root leaf node, remove it
    /// and unsplit its parent by replacing its parent with either
    /// the opposite branch of the tree from this leaf.
    /// On success, yields the revised cursor, which now points to
    /// the newly unsplit node, along with the leaf value and prior
    /// parent node value.
    /// On failure, yields `Err` containing the unchanged cursor.
    pub fn unsplit_leaf(self) -> Result<(Self, L, Option<N>), Self> {
        if !self.is_leaf() || self.is_top() {
            return Err(self);
        }

        match (*self.it, *self.path) {
            (Tree::Leaf(l), Path::Left { right, data, up }) => Ok((
                Self {
                    it: right,
                    path: up,
                },
                l,
                data,
            )),
            (Tree::Leaf(l), Path::Right { left, data, up }) => {
                Ok((Self { it: left, path: up }, l, data))
            }
            (Tree::Leaf(_), Path::Top) => unreachable!(),
            (Tree::Empty, _) => unreachable!(),
            (Tree::Node { .. }, _) => unreachable!(),
        }
    }

    pub fn split_node_and_insert_left(self, to_insert: L) -> Result<Self, Self> {
        match *self.it {
            Tree::Node { left, right, data } => Ok(Self {
                it: Box::new(Tree::Node {
                    data: None,
                    right: Box::new(Tree::Node { left, right, data }),
                    left: Box::new(Tree::Leaf(to_insert)),
                }),
                path: self.path,
            }),
            _ => Err(self),
        }
    }

    pub fn split_node_and_insert_right(self, to_insert: L) -> Result<Self, Self> {
        match *self.it {
            Tree::Node { left, right, data } => Ok(Self {
                it: Box::new(Tree::Node {
                    data: None,
                    left: Box::new(Tree::Node { left, right, data }),
                    right: Box::new(Tree::Leaf(to_insert)),
                }),
                path: self.path,
            }),
            _ => Err(self),
        }
    }

    /// If the current position is a leaf, split it into a Node where
    /// the left side holds the current leaf value and the right side
    /// holds the provided `right` value.
    /// The cursor position remains unchanged.
    /// Consumes the cursor and returns a new cursor representing the
    /// mutated tree.
    /// If the current position is not a leaf, yields `Err` containing
    /// the unchanged cursor.
    pub fn split_leaf_and_insert_right(self, right: L) -> Result<Self, Self> {
        match *self.it {
            Tree::Leaf(left) => Ok(Self {
                it: Box::new(Tree::Node {
                    data: None,
                    left: Box::new(Tree::Leaf(left)),
                    right: Box::new(Tree::Leaf(right)),
                }),
                path: self.path,
            }),
            _ => Err(self),
        }
    }

    /// If the current position is a leaf, split it into a Node where
    /// the right side holds the current leaf value and the left side
    /// holds the provided `left` value.
    /// The cursor position remains unchanged.
    /// Consumes the cursor and returns a new cursor representing the
    /// mutated tree.
    /// If the current position is not a leaf, yields `Err` containing
    /// the unchanged cursor.
    pub fn split_leaf_and_insert_left(self, left: L) -> Result<Self, Self> {
        match *self.it {
            Tree::Leaf(right) => Ok(Self {
                it: Box::new(Tree::Node {
                    data: None,
                    left: Box::new(Tree::Leaf(left)),
                    right: Box::new(Tree::Leaf(right)),
                }),
                path: self.path,
            }),
            _ => Err(self),
        }
    }

    /// If the current position is not a leaf, move the cursor to
    /// its left child.
    /// Consumes the cursor and returns a new cursor representing the
    /// mutated tree.
    /// If the current position is a Leaf, yields `Err` containing
    /// the unchanged cursor.
    pub fn go_left(self) -> Result<Self, Self> {
        match *self.it {
            Tree::Node { left, right, data } => Ok(Self {
                it: left,
                path: Box::new(Path::Left {
                    data,
                    right,
                    up: self.path,
                }),
            }),
            _ => Err(self),
        }
    }

    /// If the current position is not a leaf, move the cursor to
    /// its right child.
    /// Consumes the cursor and returns a new cursor representing the
    /// mutated tree.
    /// If the current position is a Leaf, yields `Err` containing
    /// the unchanged cursor.
    pub fn go_right(self) -> Result<Self, Self> {
        match *self.it {
            Tree::Node { left, right, data } => Ok(Self {
                it: right,
                path: Box::new(Path::Right {
                    data,
                    left,
                    up: self.path,
                }),
            }),
            _ => Err(self),
        }
    }

    /// If the current position is not at the root of the tree,
    /// move up to the parent of the current position.
    /// Consumes the cursor and returns a new cursor representing the
    /// new location.
    /// If the current position is the top of the tree,
    /// yields `Err` containing the unchanged cursor.
    pub fn go_up(self) -> Result<Self, Self> {
        match *self.path {
            Path::Top => Err(self),
            Path::Right { left, data, up } => Ok(Self {
                it: Box::new(Tree::Node {
                    left,
                    right: self.it,
                    data,
                }),
                path: up,
            }),
            Path::Left { right, data, up } => Ok(Self {
                it: Box::new(Tree::Node {
                    right,
                    left: self.it,
                    data,
                }),
                path: up,
            }),
        }
    }

    /// Move the current position to the next in a preorder traversal.
    /// Returns the modified cursor position.
    ///
    /// In the case where there are no more nodes in the preorder traversal,
    /// yields `Err` with the newly adjusted cursor; calling `preorder_next`
    /// after it has yielded `Err` can potentially yield `Ok` with previously
    /// visited nodes, so the caller must take care to stop iterating when
    /// `Err` is received!
    pub fn preorder_next(mut self) -> Result<Self, Self> {
        // Since we are a "proper" binary tree, we know we cannot have
        // difficult cases such as a left without a right or vice versa.

        if self.is_leaf() {
            if self.is_left() {
                return self.go_up()?.go_right();
            }

            // while (We were on the right)
            loop {
                self = self.go_up()?;

                if self.is_top() {
                    return Err(self);
                }

                if self.is_left() {
                    return self.go_up()?.go_right();
                }
            }
        } else {
            self.go_left()
        }
    }

    /// Move the current position to the next in a postorder traversal.
    /// Returns the modified cursor position.
    ///
    /// In the case where there are no more nodes in the postorder traversal,
    /// yields `Err` with the newly adjusted cursor; calling `postorder_next`
    /// after it has yielded `Err` can potentially yield `Ok` with previously
    /// visited nodes, so the caller must take care to stop iterating when
    /// `Err` is received!
    pub fn postorder_next(mut self) -> Result<Self, Self> {
        // Since we are a "proper" binary tree, we know we cannot have
        // difficult cases such as a left without a right or vice versa.

        if self.is_leaf() {
            if self.is_right() {
                return self.go_up()?.go_left();
            }

            // while (We were on the left)
            loop {
                self = self.go_up()?;

                if self.is_top() {
                    return Err(self);
                }

                if self.is_right() {
                    return self.go_up()?.go_left();
                }
            }
        } else {
            self.go_right()
        }
    }

    /// Move to the nth (preorder) leaf from the current position.
    pub fn go_to_nth_leaf(mut self, n: usize) -> Result<Self, Self> {
        let mut next = 0;
        loop {
            if self.is_leaf() {
                if next == n {
                    return Ok(self);
                }
                next += 1;
            }
            self = self.preorder_next()?;
        }
    }

    /// Consume the cursor and return the root of the Tree
    pub fn tree(mut self) -> Tree<L, N> {
        loop {
            self = match self.go_up() {
                Ok(up) => up,
                Err(top) => return *top.it,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_split_and_iterate() {
        let t: Tree<i32, i32> = Tree::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();

        let t = t
            .cursor()
            .go_to_nth_leaf(1)
            .unwrap()
            .split_leaf_and_insert_right(3)
            .unwrap()
            .tree();

        let mut leaves = vec![];

        let mut cursor = t.cursor();
        loop {
            eprintln!("cursor: {:?}", cursor);
            if cursor.is_leaf() {
                leaves.push(*cursor.leaf_mut().unwrap());
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(_) => break,
            }
        }

        assert_eq!(leaves, vec![1, 2, 3]);
    }

    #[test]
    fn populate() {
        let t: Tree<i32, i32> = Tree::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();

        assert_eq!(
            t,
            Tree::Node {
                left: Box::new(Tree::Leaf(1)),
                right: Box::new(Tree::Leaf(2)),
                data: None
            }
        );

        let t = t.cursor().assign_node(Some(100)).unwrap().tree();

        assert_eq!(
            t,
            Tree::Node {
                left: Box::new(Tree::Leaf(1)),
                right: Box::new(Tree::Leaf(2)),
                data: Some(100),
            }
        );

        let t = t
            .cursor()
            .go_left()
            .unwrap()
            .split_leaf_and_insert_left(3)
            .unwrap()
            .assign_node(Some(101))
            .unwrap()
            .go_left()
            .unwrap()
            .split_leaf_and_insert_right(4)
            .unwrap()
            .assign_node(Some(102))
            .unwrap()
            .go_left()
            .unwrap()
            .split_leaf_and_insert_right(5)
            .unwrap()
            .assign_node(Some(103))
            .unwrap()
            .tree();

        assert_eq!(
            t,
            Tree::Node {
                left: Box::new(Tree::Node {
                    left: Box::new(Tree::Node {
                        left: Box::new(Tree::Node {
                            left: Box::new(Tree::Leaf(3)),
                            right: Box::new(Tree::Leaf(5)),
                            data: Some(103)
                        }),
                        right: Box::new(Tree::Leaf(4)),
                        data: Some(102)
                    }),
                    right: Box::new(Tree::Leaf(1)),
                    data: Some(101)
                }),
                right: Box::new(Tree::Leaf(2)),
                data: Some(100),
            }
        );

        let mut cursor = t.cursor();
        assert_eq!(100, cursor.node_mut().unwrap().unwrap());

        cursor = cursor.preorder_next().unwrap();
        assert_eq!(101, cursor.node_mut().unwrap().unwrap());

        cursor = cursor.preorder_next().unwrap();
        assert_eq!(102, cursor.node_mut().unwrap().unwrap());

        cursor = cursor.preorder_next().unwrap();
        assert_eq!(103, cursor.node_mut().unwrap().unwrap());

        cursor = cursor.preorder_next().unwrap();
        assert_eq!(3, cursor.leaf_mut().copied().unwrap());

        cursor = cursor.preorder_next().unwrap();
        assert_eq!(5, cursor.leaf_mut().copied().unwrap());

        cursor = cursor.preorder_next().unwrap();
        assert_eq!(4, cursor.leaf_mut().copied().unwrap());

        cursor = cursor.preorder_next().unwrap();
        assert_eq!(1, cursor.leaf_mut().copied().unwrap());

        cursor = cursor.preorder_next().unwrap();
        assert_eq!(2, cursor.leaf_mut().copied().unwrap());

        assert!(cursor.preorder_next().is_err());
    }

    // ── Tree construction ──────────────────────────────────────

    #[test]
    fn tree_new_is_empty() {
        let t: Tree<i32> = Tree::new();
        assert!(t.is_empty());
    }

    #[test]
    fn tree_leaf_is_not_empty() {
        let t = Tree::<i32>::Leaf(42);
        assert!(!t.is_empty());
    }

    // ── num_leaves ─────────────────────────────────────────────

    #[test]
    fn empty_tree_has_zero_leaves() {
        let t: Tree<i32> = Tree::new();
        assert_eq!(t.num_leaves(), 0);
    }

    #[test]
    fn single_leaf_has_one() {
        let t = Tree::<i32>::Leaf(1);
        assert_eq!(t.num_leaves(), 1);
    }

    #[test]
    fn node_with_two_leaves() {
        let t = Tree::<i32>::Node {
            left: Box::new(Tree::Leaf(1)),
            right: Box::new(Tree::Leaf(2)),
            data: None,
        };
        assert_eq!(t.num_leaves(), 2);
    }

    // ── Cursor construction ────────────────────────────────────

    #[test]
    fn cursor_new_is_top_and_empty() {
        let c: Cursor<i32, ()> = Cursor::new();
        assert!(c.is_top());
        assert!(!c.is_leaf());
    }

    // ── Position queries ───────────────────────────────────────

    #[test]
    fn cursor_at_leaf_reports_is_leaf() {
        let c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.is_leaf());
        assert!(c.is_top());
    }

    #[test]
    fn cursor_at_node_not_leaf() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap();
        assert!(!c.is_leaf());
        assert!(c.is_top());
    }

    #[test]
    fn go_left_reports_is_left() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .go_left()
            .unwrap();
        assert!(c.is_left());
        assert!(!c.is_right());
        assert!(!c.is_top());
    }

    #[test]
    fn go_right_reports_is_right() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .go_right()
            .unwrap();
        assert!(c.is_right());
        assert!(!c.is_left());
    }

    // ── Navigation errors ──────────────────────────────────────

    #[test]
    fn go_left_on_leaf_fails() {
        let c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.go_left().is_err());
    }

    #[test]
    fn go_right_on_leaf_fails() {
        let c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.go_right().is_err());
    }

    #[test]
    fn go_up_at_top_fails() {
        let c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.go_up().is_err());
    }

    // ── go_up ──────────────────────────────────────────────────

    #[test]
    fn go_up_from_left_child() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .go_left()
            .unwrap()
            .go_up()
            .unwrap();
        assert!(c.is_top());
    }

    #[test]
    fn go_up_from_right_child() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .go_right()
            .unwrap()
            .go_up()
            .unwrap();
        assert!(c.is_top());
    }

    // ── assign_top errors ──────────────────────────────────────

    #[test]
    fn assign_top_on_nonempty_fails() {
        let c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.assign_top(2).is_err());
    }

    // ── leaf_mut / node_mut ────────────────────────────────────

    #[test]
    fn leaf_mut_returns_none_on_node() {
        let mut c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap();
        assert!(c.leaf_mut().is_none());
    }

    #[test]
    fn node_mut_returns_err_on_leaf() {
        let mut c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.node_mut().is_err());
    }

    #[test]
    fn leaf_mut_can_mutate() {
        let mut c = Tree::<i32>::Leaf(1).cursor();
        *c.leaf_mut().unwrap() = 99;
        assert_eq!(*c.leaf_mut().unwrap(), 99);
    }

    // ── unsplit_leaf ───────────────────────────────────────────

    #[test]
    fn unsplit_leaf_at_top_fails() {
        let c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.unsplit_leaf().is_err());
    }

    #[test]
    fn unsplit_leaf_on_node_fails() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap();
        assert!(c.unsplit_leaf().is_err());
    }

    #[test]
    fn unsplit_left_leaf() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .assign_node(Some(99))
            .unwrap()
            .go_left()
            .unwrap();

        let (c, leaf_val, node_data) = c.unsplit_leaf().unwrap();
        assert_eq!(leaf_val, 1);
        assert_eq!(node_data, Some(99));
        // After unsplit, cursor points to the remaining right subtree
        assert!(c.is_leaf());
        assert_eq!(*c.subtree(), Tree::Leaf(2));
    }

    #[test]
    fn unsplit_right_leaf() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .assign_node(Some(50))
            .unwrap()
            .go_right()
            .unwrap();

        let (c, leaf_val, node_data) = c.unsplit_leaf().unwrap();
        assert_eq!(leaf_val, 2);
        assert_eq!(node_data, Some(50));
        assert!(c.is_leaf());
        assert_eq!(*c.subtree(), Tree::Leaf(1));
    }

    // ── split_node_and_insert_left/right ───────────────────────

    #[test]
    fn split_node_and_insert_left_on_leaf_fails() {
        let c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.split_node_and_insert_left(2).is_err());
    }

    #[test]
    fn split_node_and_insert_left_works() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .split_node_and_insert_left(0)
            .unwrap();

        let t = c.tree();
        assert_eq!(t.num_leaves(), 3);
    }

    #[test]
    fn split_node_and_insert_right_on_leaf_fails() {
        let c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.split_node_and_insert_right(2).is_err());
    }

    #[test]
    fn split_node_and_insert_right_works() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .split_node_and_insert_right(3)
            .unwrap();

        let t = c.tree();
        assert_eq!(t.num_leaves(), 3);
    }

    // ── path_to_root ───────────────────────────────────────────

    #[test]
    fn path_to_root_at_top_is_empty() {
        let c = Tree::<i32>::Leaf(1).cursor();
        let path: Vec<_> = c.path_to_root().collect();
        assert!(path.is_empty());
    }

    #[test]
    fn path_to_root_from_left_child() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .assign_node(Some(10))
            .unwrap()
            .go_left()
            .unwrap();

        let path: Vec<_> = c.path_to_root().collect();
        assert_eq!(path.len(), 1);
        assert_eq!(path[0].0, PathBranch::IsLeft);
        assert_eq!(*path[0].1, Some(10));
    }

    // ── postorder_next ─────────────────────────────────────────

    #[test]
    fn postorder_traversal() {
        let t: Tree<i32, i32> = Tree::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();

        // Start at rightmost leaf for postorder
        let mut cursor = t.cursor().go_right().unwrap();
        let mut leaves = vec![*cursor.leaf_mut().unwrap()];

        match cursor.postorder_next() {
            Ok(c) => {
                cursor = c;
                if cursor.is_leaf() {
                    leaves.push(*cursor.leaf_mut().unwrap());
                }
            }
            Err(_) => {}
        }

        assert_eq!(leaves, vec![2, 1]);
    }

    // ── subtree ────────────────────────────────────────────────

    #[test]
    fn subtree_at_leaf() {
        let c = Tree::<i32>::Leaf(42).cursor();
        assert_eq!(*c.subtree(), Tree::Leaf(42));
    }

    #[test]
    fn subtree_at_empty() {
        let c: Cursor<i32, ()> = Cursor::new();
        assert_eq!(*c.subtree(), Tree::Empty);
    }

    // ── tree() reconstructs ────────────────────────────────────

    #[test]
    fn tree_from_deep_cursor_position() {
        let t = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .go_left()
            .unwrap()
            .split_leaf_and_insert_right(3)
            .unwrap()
            .go_left()
            .unwrap()
            .tree();
        // Should reconstruct full tree from deep position
        assert_eq!(t.num_leaves(), 3);
    }

    // ── go_to_nth_leaf ─────────────────────────────────────────

    #[test]
    fn go_to_nth_leaf_zero() {
        let mut c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(10)
            .unwrap()
            .split_leaf_and_insert_right(20)
            .unwrap()
            .go_to_nth_leaf(0)
            .unwrap();
        assert_eq!(*c.leaf_mut().unwrap(), 10);
    }

    #[test]
    fn go_to_nth_leaf_one() {
        let mut c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(10)
            .unwrap()
            .split_leaf_and_insert_right(20)
            .unwrap()
            .go_to_nth_leaf(1)
            .unwrap();
        assert_eq!(*c.leaf_mut().unwrap(), 20);
    }

    #[test]
    fn go_to_nth_leaf_out_of_range_fails() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(10)
            .unwrap()
            .split_leaf_and_insert_right(20)
            .unwrap();
        assert!(c.go_to_nth_leaf(5).is_err());
    }

    // ── Debug impls ────────────────────────────────────────────

    #[test]
    fn tree_debug_output() {
        let t = Tree::<i32>::Leaf(1);
        let debug = format!("{t:?}");
        assert!(debug.contains("Leaf"));
        assert!(debug.contains("1"));
    }

    #[test]
    fn cursor_debug_output() {
        let c = Tree::<i32>::Leaf(1).cursor();
        let debug = format!("{c:?}");
        assert!(debug.contains("Cursor"));
    }

    #[test]
    fn empty_tree_debug() {
        let t: Tree<i32> = Tree::new();
        assert_eq!(format!("{t:?}"), "Empty");
    }

    // ── PartialEq ──────────────────────────────────────────────

    #[test]
    fn empty_trees_are_equal() {
        let a: Tree<i32> = Tree::new();
        let b: Tree<i32> = Tree::new();
        assert_eq!(a, b);
    }

    #[test]
    fn different_leaves_are_not_equal() {
        assert_ne!(Tree::<i32>::Leaf(1), Tree::<i32>::Leaf(2));
    }

    #[test]
    fn leaf_and_empty_not_equal() {
        assert_ne!(Tree::<i32>::Leaf(1), Tree::<i32>::new());
    }

    // ── PathBranch ─────────────────────────────────────────────

    #[test]
    fn path_branch_equality() {
        assert_eq!(PathBranch::IsLeft, PathBranch::IsLeft);
        assert_ne!(PathBranch::IsLeft, PathBranch::IsRight);
    }

    #[test]
    fn path_branch_debug() {
        let debug = format!("{:?}", PathBranch::IsLeft);
        assert!(debug.contains("IsLeft"));
    }

    // ── Additional Tree tests ─────────────────────────────────

    #[test]
    fn node_is_not_empty() {
        let t = Tree::<i32>::Node {
            left: Box::new(Tree::Leaf(1)),
            right: Box::new(Tree::Leaf(2)),
            data: None,
        };
        assert!(!t.is_empty());
    }

    #[test]
    fn num_leaves_deep_tree() {
        let t = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .go_left()
            .unwrap()
            .split_leaf_and_insert_right(3)
            .unwrap()
            .tree();
        assert_eq!(t.num_leaves(), 3);
    }

    #[test]
    fn tree_node_debug_shows_fields() {
        let t = Tree::<i32>::Node {
            left: Box::new(Tree::Leaf(1)),
            right: Box::new(Tree::Leaf(2)),
            data: None,
        };
        let debug = format!("{t:?}");
        assert!(debug.contains("Node"));
        assert!(debug.contains("left"));
        assert!(debug.contains("right"));
    }

    #[test]
    fn same_structure_same_data_equal() {
        let t1 = Tree::<i32, i32>::Node {
            left: Box::new(Tree::Leaf(1)),
            right: Box::new(Tree::Leaf(2)),
            data: Some(10),
        };
        let t2 = Tree::<i32, i32>::Node {
            left: Box::new(Tree::Leaf(1)),
            right: Box::new(Tree::Leaf(2)),
            data: Some(10),
        };
        assert_eq!(t1, t2);
    }

    #[test]
    fn same_structure_different_node_data_not_equal() {
        let t1 = Tree::<i32, i32>::Node {
            left: Box::new(Tree::Leaf(1)),
            right: Box::new(Tree::Leaf(2)),
            data: Some(10),
        };
        let t2 = Tree::<i32, i32>::Node {
            left: Box::new(Tree::Leaf(1)),
            right: Box::new(Tree::Leaf(2)),
            data: Some(20),
        };
        assert_ne!(t1, t2);
    }

    #[test]
    fn node_vs_leaf_not_equal() {
        let t1 = Tree::<i32>::Node {
            left: Box::new(Tree::Leaf(1)),
            right: Box::new(Tree::Leaf(2)),
            data: None,
        };
        let t2 = Tree::<i32>::Leaf(1);
        assert_ne!(t1, t2);
    }

    // ── Additional Cursor mutation tests ──────────────────────

    #[test]
    fn assign_node_on_leaf_fails() {
        let c = Tree::<i32, i32>::Leaf(1).cursor();
        assert!(c.assign_node(Some(5)).is_err());
    }

    #[test]
    fn node_mut_can_mutate() {
        let mut c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .assign_node(Some(10))
            .unwrap();
        *c.node_mut().unwrap() = Some(99);
        assert_eq!(*c.node_mut().unwrap(), Some(99));
    }

    #[test]
    fn split_leaf_and_insert_left_works() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_left(0)
            .unwrap();
        let t = c.tree();
        assert_eq!(t.num_leaves(), 2);
        // Left should be the inserted value
        let mut cursor = t.cursor().go_left().unwrap();
        assert_eq!(*cursor.leaf_mut().unwrap(), 0);
    }

    #[test]
    fn split_leaf_and_insert_right_on_node_fails() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap();
        // Now at a node, not leaf
        assert!(c.split_leaf_and_insert_right(3).is_err());
    }

    #[test]
    fn split_leaf_and_insert_left_on_node_fails() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap();
        assert!(c.split_leaf_and_insert_left(3).is_err());
    }

    // ── Additional path_to_root tests ─────────────────────────

    #[test]
    fn path_to_root_from_right_child() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .assign_node(Some(20))
            .unwrap()
            .go_right()
            .unwrap();

        let path: Vec<_> = c.path_to_root().collect();
        assert_eq!(path.len(), 1);
        assert_eq!(path[0].0, PathBranch::IsRight);
        assert_eq!(*path[0].1, Some(20));
    }

    #[test]
    fn path_to_root_deep_nesting() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .assign_node(Some(10))
            .unwrap()
            .go_left()
            .unwrap()
            .split_leaf_and_insert_right(3)
            .unwrap()
            .assign_node(Some(20))
            .unwrap()
            .go_left()
            .unwrap();

        let path: Vec<_> = c.path_to_root().collect();
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].0, PathBranch::IsLeft);
        assert_eq!(*path[0].1, Some(20));
        assert_eq!(path[1].0, PathBranch::IsLeft);
        assert_eq!(*path[1].1, Some(10));
    }

    // ── Additional traversal tests ────────────────────────────

    #[test]
    fn preorder_next_on_single_leaf_fails() {
        let c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.preorder_next().is_err());
    }

    #[test]
    fn go_to_nth_leaf_single_leaf() {
        let mut c = Tree::<i32>::Leaf(42).cursor().go_to_nth_leaf(0).unwrap();
        assert_eq!(*c.leaf_mut().unwrap(), 42);
    }

    #[test]
    fn go_to_nth_leaf_on_empty_tree_fails() {
        let c: Cursor<i32, ()> = Cursor::new();
        assert!(c.go_to_nth_leaf(0).is_err());
    }

    // ── PathBranch derive traits ──────────────────────────────

    #[test]
    fn path_branch_clone_copy() {
        let a = PathBranch::IsLeft;
        let b = a; // Copy
        let c = a.clone();
        assert_eq!(b, c);
    }

    #[test]
    fn path_branch_is_right_debug() {
        let debug = format!("{:?}", PathBranch::IsRight);
        assert!(debug.contains("IsRight"));
    }

    // ── String leaf type ─────────────────────────────────────

    #[test]
    fn tree_with_string_leaves() {
        let t = Tree::<String>::new()
            .cursor()
            .assign_top("hello".to_string())
            .unwrap()
            .split_leaf_and_insert_right("world".to_string())
            .unwrap()
            .tree();
        assert_eq!(t.num_leaves(), 2);
    }

    // ── postorder_next additional tests ───────────────────────

    #[test]
    fn postorder_next_on_single_leaf_fails() {
        let c = Tree::<i32>::Leaf(1).cursor();
        assert!(c.postorder_next().is_err());
    }

    #[test]
    fn postorder_full_traversal_three_leaves() {
        // Build tree: Node(Node(Leaf(1), Leaf(2)), Leaf(3))
        let t = Tree::<i32, ()>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(3)
            .unwrap()
            .go_left()
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();

        // Start postorder from the rightmost leaf
        let mut cursor = t.cursor().go_right().unwrap();
        let mut leaves = vec![*cursor.leaf_mut().unwrap()];

        loop {
            match cursor.postorder_next() {
                Ok(c) => {
                    cursor = c;
                    if cursor.is_leaf() {
                        leaves.push(*cursor.leaf_mut().unwrap());
                    }
                }
                Err(_) => break,
            }
        }
        // Postorder visits right subtree leaves in reverse
        assert_eq!(leaves, vec![3, 2, 1]);
    }

    // ── Cursor from empty tree ───────────────────────────────

    #[test]
    fn cursor_from_empty_tree_is_empty() {
        let t: Tree<i32> = Tree::new();
        let c = t.cursor();
        assert!(c.is_top());
        assert!(!c.is_leaf());
        assert_eq!(*c.subtree(), Tree::Empty);
    }

    // ── assign_top not at top ────────────────────────────────

    #[test]
    fn assign_top_not_at_top_fails() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .go_left()
            .unwrap();
        // Not at top, so assign_top should fail
        assert!(c.assign_top(99).is_err());
    }

    // ── leaf_mut / node_mut on empty tree ─────────────────────

    #[test]
    fn leaf_mut_on_empty_tree_returns_none() {
        let mut c: Cursor<i32, ()> = Cursor::new();
        assert!(c.leaf_mut().is_none());
    }

    #[test]
    fn node_mut_on_empty_tree_returns_err() {
        let mut c: Cursor<i32, ()> = Cursor::new();
        assert!(c.node_mut().is_err());
    }

    // ── go_to_nth_leaf with many leaves ──────────────────────

    #[test]
    fn go_to_nth_leaf_four_leaves() {
        // Build tree with 4 leaves: 1, 2, 3, 4
        // Must .tree() between splits to reset cursor to root
        fn build_four_leaf_tree() -> Tree<i32, ()> {
            let t = Tree::<i32, ()>::new()
                .cursor()
                .assign_top(1)
                .unwrap()
                .split_leaf_and_insert_right(2)
                .unwrap()
                .tree();
            let t = t
                .cursor()
                .go_to_nth_leaf(1)
                .unwrap()
                .split_leaf_and_insert_right(3)
                .unwrap()
                .tree();
            t.cursor()
                .go_to_nth_leaf(2)
                .unwrap()
                .split_leaf_and_insert_right(4)
                .unwrap()
                .tree()
        }

        assert_eq!(build_four_leaf_tree().num_leaves(), 4);

        for (i, expected) in [1, 2, 3, 4].iter().enumerate() {
            let mut c = build_four_leaf_tree().cursor().go_to_nth_leaf(i).unwrap();
            assert_eq!(*c.leaf_mut().unwrap(), *expected);
        }
    }

    // ── tree() from rightmost position ───────────────────────

    #[test]
    fn tree_from_rightmost_position() {
        let t = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .go_right()
            .unwrap()
            .tree();
        assert_eq!(t.num_leaves(), 2);
    }

    // ── Large tree ───────────────────────────────────────────

    #[test]
    fn build_ten_leaf_tree() {
        let mut t = Tree::<i32, ()>::new()
            .cursor()
            .assign_top(0)
            .unwrap()
            .tree();
        for i in 1..10 {
            t = t
                .cursor()
                .go_to_nth_leaf(i - 1)
                .unwrap()
                .split_leaf_and_insert_right(i as i32)
                .unwrap()
                .tree();
        }
        assert_eq!(t.num_leaves(), 10);
    }

    // ── PartialEq nested trees ───────────────────────────────

    #[test]
    fn nested_trees_equal() {
        let t1 = Tree::<i32, i32>::Node {
            left: Box::new(Tree::Node {
                left: Box::new(Tree::Leaf(1)),
                right: Box::new(Tree::Leaf(2)),
                data: Some(10),
            }),
            right: Box::new(Tree::Leaf(3)),
            data: Some(20),
        };
        let t2 = Tree::<i32, i32>::Node {
            left: Box::new(Tree::Node {
                left: Box::new(Tree::Leaf(1)),
                right: Box::new(Tree::Leaf(2)),
                data: Some(10),
            }),
            right: Box::new(Tree::Leaf(3)),
            data: Some(20),
        };
        assert_eq!(t1, t2);
    }

    #[test]
    fn nested_trees_different_leaf_not_equal() {
        let t1 = Tree::<i32, i32>::Node {
            left: Box::new(Tree::Node {
                left: Box::new(Tree::Leaf(1)),
                right: Box::new(Tree::Leaf(2)),
                data: None,
            }),
            right: Box::new(Tree::Leaf(3)),
            data: None,
        };
        let t2 = Tree::<i32, i32>::Node {
            left: Box::new(Tree::Node {
                left: Box::new(Tree::Leaf(1)),
                right: Box::new(Tree::Leaf(999)),
                data: None,
            }),
            right: Box::new(Tree::Leaf(3)),
            data: None,
        };
        assert_ne!(t1, t2);
    }

    // ── Round-trip cursor split and unsplit ───────────────────

    #[test]
    fn preorder_full_traversal_three_leaves() {
        let t = Tree::<i32, ()>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(3)
            .unwrap()
            .go_left()
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();

        // preorder_next from leftmost leaf should visit all leaves
        let mut cursor = t.cursor().go_to_nth_leaf(0).unwrap();
        let mut leaves = vec![*cursor.leaf_mut().unwrap()];
        loop {
            match cursor.preorder_next() {
                Ok(c) => {
                    cursor = c;
                    if cursor.is_leaf() {
                        leaves.push(*cursor.leaf_mut().unwrap());
                    }
                }
                Err(_) => break,
            }
        }
        assert_eq!(leaves, vec![1, 2, 3]);
    }

    #[test]
    fn build_ten_leaf_tree_verify_values() {
        let mut t = Tree::<i32, ()>::new()
            .cursor()
            .assign_top(0)
            .unwrap()
            .tree();
        for i in 1..10 {
            t = t
                .cursor()
                .go_to_nth_leaf(i - 1)
                .unwrap()
                .split_leaf_and_insert_right(i as i32)
                .unwrap()
                .tree();
        }
        // Verify each leaf value
        for i in 0..10 {
            let mut c = t.cursor().go_to_nth_leaf(i).unwrap();
            assert_eq!(*c.leaf_mut().unwrap(), i as i32);
            t = c.tree();
        }
    }

    #[test]
    fn go_left_then_go_right_sibling_via_up() {
        let t = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();

        // Navigate: root → left → up → right
        let mut c = t
            .cursor()
            .go_left()
            .unwrap()
            .go_up()
            .unwrap()
            .go_right()
            .unwrap();
        assert!(c.is_right());
        assert_eq!(*c.leaf_mut().unwrap(), 2);
    }

    #[test]
    fn assign_node_then_read_back() {
        let mut c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .assign_node(Some(42))
            .unwrap();
        assert_eq!(*c.node_mut().unwrap(), Some(42));
    }

    #[test]
    fn split_then_unsplit_restores_leaf() {
        let c = Tree::<i32, i32>::Leaf(42)
            .cursor()
            .split_leaf_and_insert_right(99)
            .unwrap()
            .go_right()
            .unwrap();

        let (c, removed, _data) = c.unsplit_leaf().unwrap();
        assert_eq!(removed, 99);
        // Should now have a single leaf with value 42
        assert!(c.is_leaf());
        assert_eq!(*c.subtree(), Tree::Leaf(42));
    }

    #[test]
    fn unsplit_left_leaf_then_tree_reconstructs() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .go_left()
            .unwrap();

        let (c, _leaf, _data) = c.unsplit_leaf().unwrap();
        let t = c.tree();
        assert_eq!(t, Tree::Leaf(2));
    }

    #[test]
    fn assign_node_none_clears_data() {
        let mut c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .assign_node(Some(42))
            .unwrap()
            .assign_node(None)
            .unwrap();
        assert_eq!(*c.node_mut().unwrap(), None);
    }

    #[test]
    fn split_leaf_insert_right_preserves_original_on_left() {
        let mut c = Tree::<i32, ()>::Leaf(10)
            .cursor()
            .split_leaf_and_insert_right(20)
            .unwrap()
            .go_left()
            .unwrap();
        assert_eq!(*c.leaf_mut().unwrap(), 10);
    }

    #[test]
    fn split_leaf_insert_left_preserves_original_on_right() {
        let mut c = Tree::<i32, ()>::Leaf(10)
            .cursor()
            .split_leaf_and_insert_left(20)
            .unwrap()
            .go_right()
            .unwrap();
        assert_eq!(*c.leaf_mut().unwrap(), 10);
    }

    #[test]
    fn cursor_new_tree_is_empty() {
        let c: Cursor<i32, ()> = Cursor::new();
        let t = c.tree();
        assert!(t.is_empty());
    }

    #[test]
    fn go_left_go_up_go_left_returns_to_same() {
        let t = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();

        let mut c1 = t.cursor().go_left().unwrap();
        let val1 = *c1.leaf_mut().unwrap();
        let mut c2 = c1.go_up().unwrap().go_left().unwrap();
        let val2 = *c2.leaf_mut().unwrap();
        assert_eq!(val1, val2);
    }

    #[test]
    fn split_node_insert_left_increases_leaves_by_one() {
        let t = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();
        assert_eq!(t.num_leaves(), 2);

        let t = t
            .cursor()
            .split_node_and_insert_left(0)
            .unwrap()
            .tree();
        assert_eq!(t.num_leaves(), 3);
    }

    #[test]
    fn split_node_insert_right_increases_leaves_by_one() {
        let t = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();

        let t = t
            .cursor()
            .split_node_and_insert_right(3)
            .unwrap()
            .tree();
        assert_eq!(t.num_leaves(), 3);
    }

    #[test]
    fn tree_with_unit_node_data() {
        let t = Tree::<i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();
        assert_eq!(t.num_leaves(), 2);
    }

    #[test]
    fn subtree_at_node_shows_children() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap();
        match c.subtree() {
            Tree::Node { left, right, .. } => {
                assert_eq!(**left, Tree::Leaf(1));
                assert_eq!(**right, Tree::Leaf(2));
            }
            _ => panic!("expected Node"),
        }
    }

    #[test]
    fn path_to_root_from_deeply_nested_right() {
        let c = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .assign_node(Some(10))
            .unwrap()
            .go_right()
            .unwrap()
            .split_leaf_and_insert_right(3)
            .unwrap()
            .assign_node(Some(20))
            .unwrap()
            .go_right()
            .unwrap();

        let path: Vec<_> = c.path_to_root().collect();
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].0, PathBranch::IsRight);
        assert_eq!(*path[0].1, Some(20));
        assert_eq!(path[1].0, PathBranch::IsRight);
        assert_eq!(*path[1].1, Some(10));
    }

    #[test]
    fn preorder_visits_all_nodes_and_leaves() {
        let t = Tree::<i32, i32>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .assign_node(Some(100))
            .unwrap()
            .tree();

        let mut cursor = t.cursor();
        let mut count = 0;
        loop {
            count += 1;
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(_) => break,
            }
        }
        // Node(100) + Leaf(1) + Leaf(2) = 3 visits
        assert_eq!(count, 3);
    }

    #[test]
    fn leaf_mut_mutate_then_tree_preserves() {
        let mut c = Tree::<i32>::Leaf(1).cursor();
        *c.leaf_mut().unwrap() = 100;
        let t = c.tree();
        assert_eq!(t, Tree::Leaf(100));
    }

    #[test]
    fn go_to_nth_leaf_two_finds_third_leaf() {
        let t = Tree::<i32, ()>::new()
            .cursor()
            .assign_top(1)
            .unwrap()
            .split_leaf_and_insert_right(2)
            .unwrap()
            .tree();

        let t = t
            .cursor()
            .go_to_nth_leaf(1)
            .unwrap()
            .split_leaf_and_insert_right(3)
            .unwrap()
            .tree();

        let mut c = t.cursor().go_to_nth_leaf(2).unwrap();
        assert_eq!(*c.leaf_mut().unwrap(), 3);
    }

    #[test]
    fn empty_tree_num_leaves_zero() {
        let t: Tree<String> = Tree::Empty;
        assert_eq!(t.num_leaves(), 0);
    }

    #[test]
    fn cursor_go_left_on_empty_fails() {
        let c: Cursor<i32, ()> = Cursor::new();
        assert!(c.go_left().is_err());
    }

    #[test]
    fn cursor_go_right_on_empty_fails() {
        let c: Cursor<i32, ()> = Cursor::new();
        assert!(c.go_right().is_err());
    }

    #[test]
    fn cursor_preorder_next_on_empty_fails() {
        let c: Cursor<i32, ()> = Cursor::new();
        assert!(c.preorder_next().is_err());
    }

    #[test]
    fn cursor_postorder_next_on_empty_fails() {
        let c: Cursor<i32, ()> = Cursor::new();
        assert!(c.postorder_next().is_err());
    }

    #[test]
    fn assign_top_on_empty_then_tree() {
        let t = Tree::<i32>::new()
            .cursor()
            .assign_top(77)
            .unwrap()
            .tree();
        assert_eq!(t, Tree::Leaf(77));
        assert_eq!(t.num_leaves(), 1);
    }
}
