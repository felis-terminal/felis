//! Render `felis(1)` and visible subcommands into a directory for man-page installation.
//!
//! Hand-rolled recursion avoids `clap_mangen::generate_to` rendering hidden plumbing verbs.

use std::io;
use std::path::Path;

use clap::CommandFactory;

use crate::Cli;

pub(crate) fn generate(out_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    // Clap's implicit `help` subcommand is not `hide = true`, so it
    // would spawn a `felis-help-….1` page per verb. `build()` resolves
    // the display names ("felis-sessions-attach") the file names come
    // from.
    let mut cmd = Cli::command().disable_help_subcommand(true);
    cmd.build();
    emit(&cmd, out_dir)
}

fn emit(cmd: &clap::Command, out_dir: &Path) -> io::Result<()> {
    let name = cmd
        .get_display_name()
        .unwrap_or_else(|| cmd.get_name())
        .to_owned();
    let mut buf = Vec::new();
    clap_mangen::Man::new(cmd.clone()).render(&mut buf)?;
    std::fs::write(out_dir.join(format!("{name}.1")), buf)?;
    for sub in cmd.get_subcommands().filter(|sub| !sub.is_hide_set()) {
        emit(sub, out_dir)?;
    }
    Ok(())
}
