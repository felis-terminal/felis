use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — dumps the four-level submenu screen VTTEST_PATH=N.M.K.L"]
async fn dump_four_level_screen() {
    let session = VttestSession::open();
    let path: Vec<String> = std::env::var("VTTEST_PATH")
        .unwrap_or_else(|_| "11.1.2.4".into())
        .split('.')
        .map(str::to_string)
        .collect();
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Choose test type", Duration::from_secs(3))
            .await
    );
    for step in &path {
        session.send(&format!("{step}\r")).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Path {} screen ===\n{dump}", path.join("."));
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — dumps the sub-sub-menu screen VTTEST_OUTER.VTTEST_INNER.VTTEST_LEAF"]
async fn dump_subsubmenu_screen() {
    let session = VttestSession::open();
    let outer = std::env::var("VTTEST_OUTER").unwrap_or_else(|_| "11".into());
    let inner = std::env::var("VTTEST_INNER").unwrap_or_else(|_| "5".into());
    let leaf = std::env::var("VTTEST_LEAF").unwrap_or_else(|_| "7".into());
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Choose test type", Duration::from_secs(3))
            .await
    );
    session.send(&format!("{outer}\r")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    session.send(&format!("{inner}\r")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    session.send(&format!("{leaf}\r")).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Submenu {outer}.{inner}.{leaf} screen ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — dumps the sub-menu screen VTTEST_OUTER.VTTEST_INNER"]
async fn dump_submenu_screen() {
    let session = VttestSession::open();
    let outer = std::env::var("VTTEST_OUTER").unwrap_or_else(|_| "11".into());
    let inner = std::env::var("VTTEST_INNER").unwrap_or_else(|_| "1".into());
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Choose test type", Duration::from_secs(3))
            .await
    );
    session.send(&format!("{outer}\r")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    session.send(&format!("{inner}\r")).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Submenu {outer}.{inner} screen ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — walks menu VTTEST_MENU forward by VTTEST_STEPS RETURNs"]
async fn dump_menu_walk() {
    let session = VttestSession::open();
    let menu = std::env::var("VTTEST_MENU").unwrap_or_else(|_| "2".into());
    let steps: u32 = std::env::var("VTTEST_STEPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Choose test type", Duration::from_secs(3))
            .await
    );
    session.send(&format!("{menu}\r")).await;
    for i in 0..steps {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let dump = session.with_grid(dump_grid);
        eprintln!("=== Menu {menu} screen step {i} ===\n{dump}");
        session.send("\r").await;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Menu {menu} screen final ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — prints the main menu to stderr"]
async fn dump_main_menu() {
    let session = VttestSession::open();
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Main menu ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — prints menu VTTEST_MENU's screen to stderr"]
async fn dump_menu_screen() {
    let session = VttestSession::open();
    let menu = std::env::var("VTTEST_MENU").unwrap_or_else(|_| "1".into());
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Choose test type", Duration::from_secs(3))
            .await
    );
    session.send(&format!("{menu}\r")).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Menu {menu} screen ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — prints the reports submenu to stderr"]
async fn dump_terminal_reports_submenu() {
    let session = VttestSession::open();
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Test of terminal reports", Duration::from_secs(3))
            .await
    );
    session.send("6\r").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Terminal Reports submenu ===\n{dump}");
    session.shutdown().await;
}
