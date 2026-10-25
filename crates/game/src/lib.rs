//! The recompiled game. Everything under `gen/` is written by `recomp` and is not checked in:
//!
//!   cargo run -p recomp --release -- disc/files/sb04gc_nm.elf config/hle.txt crates/game/src/gen

#![allow(non_snake_case, unused_variables, unused_mut, unused_assignments, unreachable_code, unused_parens, clippy::all)]

mod gen;
pub use gen::*;
