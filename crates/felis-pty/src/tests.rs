use std::time::Duration;

use super::*;

mod child_lifecycle;
mod park_and_adopt;
mod sink_backpressure;
mod spawn_and_io;

fn small_size() -> Size {
    Size {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// From `ComSpec` with the canonical location as a fallback, so the
/// spawn resolves under a stripped-down environment whose `PATH`
/// lacks `System32`.
#[cfg(windows)]
fn comspec() -> std::ffi::OsString {
    std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into())
}

#[cfg(unix)]
fn shell_echo(marker: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", &format!("printf '{marker}\\n'; exit 0")]);
    cmd
}

#[cfg(windows)]
fn shell_echo(marker: &str) -> Command {
    let mut cmd = Command::new(comspec());
    cmd.args(["/c", &format!("echo {marker}")]);
    cmd
}

fn collector() -> (Arc<Mutex<Vec<u8>>>, ByteSink) {
    let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink_side = Arc::clone(&collected);
    let sink: ByteSink = Box::new(move |bytes: &[u8]| {
        sink_side
            .lock()
            .expect("sink collector lock")
            .extend_from_slice(bytes);
    });
    (collected, sink)
}

fn collected_text(collected: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&collected.lock().expect("collector lock")).into_owned()
}

async fn sink_until(collected: &Mutex<Vec<u8>>, needle: &str) -> String {
    for _ in 0..200 {
        let text = collected_text(collected);
        if text.contains(needle) {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    collected_text(collected)
}

async fn wait_for_exit(child: &ChildHandle) -> bool {
    child
        .wait_timeout(Duration::from_secs(5))
        .await
        .expect("wait")
        .is_some()
}
