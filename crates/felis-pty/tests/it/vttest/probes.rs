use super::*;

async fn at_main_menu() -> VttestSession {
    let session = VttestSession::open();
    session
        .expect_text(
            "VT100 test program",
            Duration::from_secs(3),
            "intro banner never reached the grid",
        )
        .await;
    session.send("\r").await;
    session
        .expect_text(
            "Choose test type",
            Duration::from_secs(3),
            "main menu never drew",
        )
        .await;
    session
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — dumps the screen at menu path VTTEST_PATH=N.M.… (empty: the main menu)"]
async fn dump_menu_path() {
    let path: Vec<String> = std::env::var("VTTEST_PATH")
        .unwrap_or_else(|_| "11.1.2.4".into())
        .split('.')
        .filter(|step| !step.is_empty())
        .map(str::to_string)
        .collect();
    let session = at_main_menu().await;
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
#[ignore = "diagnostic probe — walks menu VTTEST_MENU forward by VTTEST_STEPS RETURNs"]
async fn dump_menu_walk() {
    let menu = std::env::var("VTTEST_MENU").unwrap_or_else(|_| "2".into());
    let steps: u32 = std::env::var("VTTEST_STEPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let session = at_main_menu().await;
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
