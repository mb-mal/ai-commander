//! v5 #5: kill_tree must take down the WHOLE process tree on Unix — the
//! child runs in its own process group and the group gets TERM→KILL.
//! Windows covers this with `taskkill /T`; here we prove the Unix half.

#![cfg(unix)]

use std::process::{Command, Stdio};

fn alive(pid: i32) -> bool {
    // kill(pid, 0) == 0 → the process still exists
    unsafe { libc::kill(pid, 0) == 0 }
}

fn descendants_of(pid: i32) -> Vec<i32> {
    // pgrep -P works on both linux (procps) and macOS (BSD userland)
    let out = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

#[test]
fn kill_tree_kills_grandchildren_too() {
    // sh spawns two background sleeps and waits — the tree kill_tree must
    // reap completely (agents spawn cargo servers, npm, language servers)
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 300 & sleep 300 & wait"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ai_commander::backend::spawn_prep(&mut cmd); // own process group, like backend children
    let mut child = cmd.spawn().unwrap();
    let pid = child.id() as i32;
    // wait for the sleeps to appear
    let mut kids = Vec::new();
    for _ in 0..50 {
        kids = descendants_of(pid);
        if kids.len() >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(kids.len() >= 2, "the background sleeps must exist");

    ai_commander::backend::kill_tree(&mut child);

    // TERM is graceful; give the reaper a moment, then everyone is gone
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if kids.iter().all(|k| !alive(*k)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    for k in &kids {
        assert!(!alive(*k), "descendant {k} survived kill_tree (v5 #5)");
    }
}
