//! How a replaceable Dolphin SDK function is handled. The host crate supplies the policy, so
//! adding a native replacement never rebuilds the recompiled game code.

use crate::Ctx;

#[derive(Clone, Copy)]
pub enum Hle {
    /// Run this native function instead of the original.
    Native(fn(&mut Ctx)),
    /// The original is hardware-independent; run the recompiled code.
    Original,
    /// Run this native function first, then the recompiled original. It may adjust the
    /// arguments or the state the original is about to read.
    Before(fn(&mut Ctx)),
    /// Run this native function; if it returns true it has stood in for the original,
    /// otherwise the recompiled original runs as well.
    Replace(fn(&mut Ctx) -> bool),
    /// Not decided yet: panics by name when called, or logs and returns 0 in survey mode.
    Unimplemented,
}
