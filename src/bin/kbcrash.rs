//! Crash-test helper for the kanban event store (spawned and killed by
//! tests/kanban_crash.rs). Opens the store, commits one full cycle, prints
//! READY, then appends events in a tight loop until the parent kills the
//! process — no graceful shutdown.

use std::io::Write;

fn main() {
    let path = std::env::args().nth(1).expect("usage: kbcrash <db-path>");
    let mut board = ai_commander::kanban::store::BoardStore::open_redb(std::path::Path::new(&path))
        .expect("open store");

    // one durable cycle before signalling readiness, so the parent only kills
    // during steady-state writes (the create phase itself is not the contract)
    let t = board.add_task("warmup");
    let _ = board.transition(&t.id, ai_commander::kanban::TaskAction::Promote);
    println!("READY");
    let _ = std::io::stdout().flush();

    let mut i = 0u64;
    loop {
        i += 1;
        let t = board.add_task(&format!("crash probe {i}"));
        let _ = board.transition(&t.id, ai_commander::kanban::TaskAction::Promote);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
