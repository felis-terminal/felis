//! felis held to the swash outputs it replaced, for as long as swash stays
//! a test dependency.

mod color;
#[cfg(not(windows))]
mod metadata;
#[cfg(not(windows))]
mod shaping;
