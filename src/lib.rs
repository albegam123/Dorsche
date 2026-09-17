//! Dorsche's stable Rust-facing wireless audio boundary.
//!
//! The Bluetooth protocol state machines remain in the pristine Floss import.
//! This crate owns session policy and the metadata-preserving local media plane.

pub mod bridge;
pub mod topshim;
pub mod wireless;
