//! Tests for the client-side half of a pipe.

use super::*;

/// Serializes the tests that stage region files: under `cargo test`
/// (the Windows gate) they share one pid-named staging directory, and
/// a sibling's file makes the last-removal-prunes-the-directory
/// contract unobservable.
static STAGING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn staging_lock() -> std::sync::MutexGuard<'static, ()> {
    STAGING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn dims() -> GridDims {
    GridDims {
        rows: 24,
        cols: 80,
        pixel_w: 0,
        pixel_h: 0,
    }
}

fn position() -> RegionPosition {
    RegionPosition {
        top_line: 42,
        cursor_line: 99,
        cursor_column: 7,
    }
}

#[test]
fn position_env_names_all_three_variables() {
    assert_eq!(
        position_env(Some(position())),
        vec![
            ("FELIS_INPUT_LINE_NUMBER".to_owned(), "42".to_owned()),
            ("FELIS_CURSOR_LINE".to_owned(), "99".to_owned()),
            ("FELIS_CURSOR_COLUMN".to_owned(), "7".to_owned()),
        ]
    );
    assert_eq!(position_env(None), Vec::<(String, String)>::new());
}

#[test]
fn the_builtin_pager_opens_at_the_position_line() {
    assert_eq!(
        builtin_pager(Some(position())),
        vec!["less".to_owned(), "-R".to_owned(), "+42".to_owned()]
    );
    assert_eq!(
        builtin_pager(None),
        vec!["less".to_owned(), "-R".to_owned()]
    );
}

#[test]
fn a_local_window_names_the_origin_session_and_no_host() {
    let origin = Origin {
        session_id: 0xfeed,
        host: None,
        osc7: None,
    };
    let env = child_env(None, &origin);

    assert!(env.contains(&(
        ORIGIN_SESSION_ENV.to_owned(),
        format!("{:032x}", 0xfeed_u128)
    )));
    assert!(!env.iter().any(|(k, _)| k == HOST_ENV));
    assert!(!env.iter().any(|(k, _)| k == CWD_ENV));
}

#[test]
fn a_remote_window_carries_the_destination_and_the_verbatim_report() {
    let origin = Origin {
        session_id: 1,
        host: Some("user@devbox".to_owned()),
        osc7: Some("file://devbox/home/you/src".to_owned()),
    };
    let env = child_env(Some(position()), &origin);

    assert!(env.contains(&(HOST_ENV.to_owned(), "user@devbox".to_owned())));
    assert!(env.contains(&(CWD_ENV.to_owned(), "file://devbox/home/you/src".to_owned())));
    assert!(env.contains(&("FELIS_INPUT_LINE_NUMBER".to_owned(), "42".to_owned())));
}

#[test]
fn a_piped_region_lands_on_disk_and_trails_the_argv() {
    let _staging = staging_lock();
    let region = b"line one\nline two\n";
    let argv = vec!["less".to_owned(), "-R".to_owned()];
    let spawn = prepare_spawn(Some(region), &argv, dims(), None, &Origin::default()).unwrap();
    let temp = spawn
        .region
        .as_ref()
        .expect("a piped region needs a file")
        .path()
        .to_path_buf();

    assert_eq!(std::fs::read(&temp).unwrap(), region);
    assert_eq!(spawn.args.command, "less");
    assert_eq!(spawn.args.args.first().map(String::as_str), Some("-R"));
    assert_eq!(
        spawn.args.args.last().map(String::as_str),
        Some(temp.to_string_lossy().as_ref())
    );
    assert_eq!(
        spawn
            .args
            .dims
            .expect("a pipe inherits the window's grid")
            .rows,
        24
    );

    drop(spawn);
    assert!(!temp.exists(), "the guard unlinks what it staged");
}

#[test]
fn removing_the_last_region_removes_the_staging_directory() {
    let _staging = staging_lock();
    let first = write_region_file(b"a\n", None).unwrap();
    let second = write_region_file(b"b\n", None).unwrap();
    let dir = first.parent().unwrap().to_path_buf();

    drop(StagedRegion::adopt(first));
    assert!(dir.exists(), "a staged sibling must keep the directory");
    drop(StagedRegion::adopt(second));
    assert!(!dir.exists(), "the last removal must take the directory");
}

#[test]
fn removing_a_file_outside_the_staging_dir_spares_its_parent() {
    let _staging = staging_lock();
    let dir = std::env::temp_dir().join(format!("felis-pipe-test-user-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = write_region_file(b"x\n", Some(dir.join("named.txt"))).unwrap();

    drop(StagedRegion::adopt(path));
    assert!(dir.exists(), "a foreign parent directory must survive");
    std::fs::remove_dir(&dir).unwrap();
}

#[test]
fn a_user_argv_is_passed_through_untouched() {
    let _staging = staging_lock();
    let argv = vec!["nvim".to_owned()];
    let spawn = prepare_spawn(
        Some(b"x\n"),
        &argv,
        dims(),
        Some(position()),
        &Origin::default(),
    )
    .unwrap();

    assert_eq!(spawn.args.command, "nvim");
    assert_eq!(spawn.args.args.len(), 1, "only the region path is added");
}

#[test]
fn an_empty_argv_runs_the_default_pager_on_the_region() {
    let _staging = staging_lock();
    let spawn = prepare_spawn(
        Some(b"x\n"),
        &[],
        dims(),
        Some(position()),
        &Origin::default(),
    )
    .unwrap();
    let temp = spawn
        .region
        .as_ref()
        .unwrap()
        .path()
        .to_str()
        .unwrap()
        .to_owned();

    let mut want = default_pager(Some(position()));
    want.push(temp);
    let mut got = vec![spawn.args.command.clone()];
    got.extend(spawn.args.args.iter().cloned());
    assert_eq!(got, want);
}

#[test]
fn the_run_action_gets_no_region_file() {
    let argv = vec!["felis-session-picker".to_owned()];
    let spawn = prepare_spawn(None, &argv, dims(), None, &Origin::default()).unwrap();

    assert!(spawn.region.is_none());
    assert_eq!(spawn.args.command, "felis-session-picker");
    assert!(
        spawn.args.args.is_empty(),
        "no region means no path to append"
    );
}

#[test]
fn local_cwd_from_osc7_accepts_this_machine_and_decodes_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_string_lossy().into_owned();
    // A Windows shell reports `file:///C:/src` for `C:\\src` (RFC 8089).
    let url_path = url_path_of(dir.path());

    assert_eq!(local_cwd_from_osc7(&format!("file://{url_path}")), path);
    assert_eq!(
        local_cwd_from_osc7(&format!("file://localhost{url_path}")),
        path
    );

    let spaced = dir.path().join("a b");
    std::fs::create_dir(&spaced).unwrap();
    assert_eq!(
        local_cwd_from_osc7(&format!("file://localhost{url_path}/a%20b")),
        spaced.to_string_lossy(),
    );

    let host = nodename().expect("this machine has a node name");
    assert_eq!(
        local_cwd_from_osc7(&format!("file://{host}{url_path}")),
        path
    );
}

fn url_path_of(dir: &Path) -> String {
    let text = dir.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        text
    } else {
        format!("/{text}")
    }
}

#[test]
fn local_cwd_from_osc7_rejects_foreign_and_unusable_reports() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_string_lossy().into_owned();

    assert_eq!(local_cwd_from_osc7(""), "");
    assert_eq!(local_cwd_from_osc7(&path), "");
    assert_eq!(
        local_cwd_from_osc7(&format!("file://some-other-host{path}")),
        "",
    );
    assert_eq!(
        local_cwd_from_osc7(&format!("file://localhost{path}/missing")),
        "",
    );
    assert_eq!(
        local_cwd_from_osc7(&format!("file://localhost{path}/%zz")),
        ""
    );
}

#[test]
fn a_foreign_report_still_reaches_the_child_verbatim() {
    let origin = Origin {
        session_id: 7,
        host: Some("devbox".to_owned()),
        osc7: Some("file://devbox/srv/app".to_owned()),
    };
    let spawn = prepare_spawn(None, &["true".to_owned()], dims(), None, &origin).unwrap();

    assert_eq!(spawn.args.cwd, "", "a foreign path is not a local cwd");
    assert!(
        spawn
            .args
            .env
            .contains(&(CWD_ENV.to_owned(), "file://devbox/srv/app".to_owned()))
    );
}

#[cfg(unix)]
#[test]
fn only_a_gone_run_s_staging_directory_is_reclaimable() {
    let ours = std::process::id();
    assert!(is_reclaimable(&format!("{TEMP_DIR_PREFIX}{ours}"), ours));
    assert!(is_reclaimable(
        &format!("{TEMP_DIR_PREFIX}4000000000"),
        ours
    ));
    assert!(!is_reclaimable(&format!("{TEMP_DIR_PREFIX}1"), ours));
    assert!(!is_reclaimable(TEMP_DIR_PREFIX, ours));
    assert!(!is_reclaimable(&format!("{TEMP_DIR_PREFIX}abc"), ours));
    assert!(!is_reclaimable("tmux-501", ours));
}

#[cfg(unix)]
#[test]
fn the_startup_sweep_reclaims_a_dead_run_and_spares_a_live_one() {
    // The sweep reclaims this process's own staging directory too, so a
    // sibling staging a region mid-run would lose it under itself.
    let _staging = staging_lock();
    let root = std::env::temp_dir();
    let dead = root.join(format!("{TEMP_DIR_PREFIX}4000000001"));
    let live = root.join(format!("{TEMP_DIR_PREFIX}1"));
    std::fs::create_dir_all(&dead).unwrap();
    std::fs::write(dead.join("region-x.txt"), b"leaked").unwrap();
    std::fs::create_dir_all(&live).unwrap();

    sweep_temp_dir();

    let live_survived = live.exists();
    std::fs::remove_dir_all(&live).ok();
    assert!(!dead.exists(), "a dead run's staging must be reclaimed");
    assert!(live_survived, "a live run's staging must be left alone");
}

fn exported(command: &std::process::Command, key: &str) -> Option<String> {
    command
        .get_envs()
        .find(|(name, _)| *name == key)
        .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
}

fn removed(command: &std::process::Command, key: &str) -> bool {
    command
        .get_envs()
        .any(|(name, value)| name == key && value.is_none())
}

#[test]
fn a_background_command_removes_every_origin_variable_the_origin_lacks() {
    let origin = Origin {
        session_id: 0xabc,
        host: None,
        osc7: None,
    };
    let command = background_command(&["felis".to_owned()], &origin).unwrap();
    assert_eq!(
        exported(&command, ORIGIN_SESSION_ENV),
        Some(SessionHex(0xabc).to_string())
    );
    for key in ORIGIN_ENV_KEYS
        .into_iter()
        .filter(|k| *k != ORIGIN_SESSION_ENV)
    {
        assert!(removed(&command, key), "{key} must be removed");
    }
}

#[test]
fn a_background_command_exports_the_origin_it_has() {
    let origin = Origin {
        session_id: 1,
        host: Some("user@devbox".to_owned()),
        osc7: Some("file://devbox/srv/app".to_owned()),
    };
    let command = background_command(&["felis".to_owned()], &origin).unwrap();
    assert_eq!(exported(&command, HOST_ENV).as_deref(), Some("user@devbox"));
    assert_eq!(
        exported(&command, CWD_ENV).as_deref(),
        Some("file://devbox/srv/app")
    );
}

#[test]
fn a_background_command_runs_in_the_local_osc7_directory_else_home() {
    let dir = tempfile::tempdir().unwrap();
    let here = Origin {
        session_id: 1,
        host: None,
        osc7: Some(format!("file://localhost{}", url_path_of(dir.path()))),
    };
    let command = background_command(&["felis".to_owned()], &here).unwrap();
    assert_eq!(command.get_current_dir(), Some(dir.path()));

    let foreign = Origin {
        osc7: Some("file://devbox/srv/app".to_owned()),
        ..here
    };
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_owned());
    let command = background_command(&["felis".to_owned()], &foreign).unwrap();
    assert_eq!(command.get_current_dir(), home.as_deref());
}

#[test]
fn a_background_command_needs_a_program() {
    assert!(background_command(&[], &Origin::default()).is_none());
}

#[cfg(unix)]
#[test]
fn a_background_spawn_returns_before_the_command_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("done");
    let origin = Origin {
        session_id: 7,
        host: None,
        osc7: Some(format!("file://localhost{}", url_path_of(dir.path()))),
    };
    let argv = [
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "sleep 0.3; printf %s \"$FELIS_ORIGIN_SESSION_ID\" > done".to_owned(),
    ];
    spawn_background(&argv, &origin).unwrap();
    assert!(!marker.exists(), "the spawn must not wait for the command");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the command never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        SessionHex(7).to_string()
    );
}
