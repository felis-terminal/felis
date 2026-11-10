//! Handing a running session to another process that continues it, as
//! an in-place upgrade does. On Unix the master fd crosses `execve`; on
//! Windows the pseudoconsole's handles are duplicated into a separate
//! successor, which only the bundled `ConPTY` host can rebuild.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{Adopted, Handover};
#[cfg(windows)]
pub use windows::{Adopted, Handover, HandoverValues, Process, available, prepare_adopt};
