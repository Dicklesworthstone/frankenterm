//! Types for representing Rust types in a more dynamic form
//! that is similar to JSON or Lua values.

#![cfg_attr(not(feature = "std"), no_std)]
// The int! macro emits an Err arm for integer conversions that are Infallible at
// some widths, so that arm is unreachable by construction, not by mistake.
#![allow(unreachable_code)]

extern crate alloc;

mod array;
mod drop;
mod error;
mod fromdynamic;
mod object;
mod todynamic;
mod value;

pub use array::Array;
pub use error::Error;
pub use frankenterm_dynamic_derive::{FromDynamic, ToDynamic};
pub use fromdynamic::{FromDynamic, FromDynamicOptions, UnknownFieldAction};
pub use object::{BorrowedKey, Object, ObjectKeyTrait};
pub use todynamic::{PlaceDynamic, ToDynamic};
pub use value::Value;
