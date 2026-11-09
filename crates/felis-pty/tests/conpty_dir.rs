#![cfg(windows)]

use felis_pty::handover::{HandoverValues, prepare_adopt};

fn adopt_error() -> String {
    match prepare_adopt(&HandoverValues::default()) {
        Ok(_) => panic!("adopted null handles"),
        Err(err) => err.to_string(),
    }
}

/// Built without `cfg(test)`, so no directory is preset; `prepare_adopt`
/// always loads the bundled host, whatever this process's integrity.
#[test]
fn an_adoption_before_the_conpty_dir_is_set_does_not_stick() {
    let unset = adopt_error();
    assert!(unset.contains("no ConPTY directory set"), "{unset}");

    // Fixed rather than a tempdir: the loaded host stays held until exit,
    // so a per-run directory could never be removed.
    let dir = std::env::temp_dir().join("felis-pty-conpty-dir-test");
    felis_pty::set_conpty_dir(dir.clone()).expect("set the directory");
    let set = adopt_error();
    assert!(set.contains("a null handle value"), "{set}");
    assert!(
        dir.join(conpty_bin_vendored::VERSION)
            .join("conpty.dll")
            .is_file()
    );

    assert_eq!(felis_pty::set_conpty_dir(dir.clone()), Ok(()));
    let other = std::env::temp_dir().join("elsewhere");
    assert_eq!(felis_pty::set_conpty_dir(other), Err(dir));
}
