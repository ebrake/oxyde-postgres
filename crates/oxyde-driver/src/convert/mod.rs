//! Type conversion: database rows → msgpack bytes.
//!
//! `encoder` defines the `CellEncoder` trait and generic columnar encoding.
//! All encoding writes directly to `Vec<u8>` msgpack buffers.

// msgpack container lengths are u32 by format (counts never approach it);
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

pub mod encoder;
pub mod postgres;
