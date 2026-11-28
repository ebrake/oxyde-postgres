//! Decode database cells and write columnar MessagePack results.
//!
//! `encoder` defines the `CellEncoder` trait and generic columnar encoding.

// msgpack container lengths are u32 by format (counts never approach it);
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

pub mod cell;
pub mod encoder;
pub mod postgres;
mod postgres_types;

pub use cell::{ArrayItem, ArrayValues, Cell, PgNumeric, PgTemporal};
