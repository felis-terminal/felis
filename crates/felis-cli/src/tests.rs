use clap::Parser;

use super::*;

mod external;
mod globals;
mod grammar;
mod help;
mod session_line;

fn argv(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}
