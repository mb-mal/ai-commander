//! Crash-consistency test: spawn the kbcrash helper, kill it mid-write
//! (SIGKILL / TerminateProcess — no graceful shutdown), reopen the store and
//! verify the invariants: reopen succeeds, snapshot covers exactly the log,
//! sequences are hole-free, and the projection folded from the full log
//! equals the stored snapshot.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use ai_commander::kanban::Kanban;
use ai_commander::kanban::store::{BoardStore, EventStore, RedbStore, SCHEMA_VERSION, fold};

fn tmpdb(tag: u32) -> PathBuf {
    let d = std::env::temp_dir().join(format!("aic-crash-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("kanban.redb")
}

#[test]
fn kill_mid_write_keeps_store_consistent() {
    for round in 1..=3u32 {
        let db = tmpdb(round);
        let mut child = Command::new(env!("CARGO_BIN_EXE_kbcrash"))
            .arg(&db)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn kbcrash");
        // wait for the READY barrier: one durable cycle is committed, so the
        // kill below lands strictly during steady-state writes
        {
            use std::io::{BufRead, BufReader};
            let out = child.stdout.take().expect("piped stdout");
            let mut line = String::new();
            BufReader::new(out)
                .read_line(&mut line)
                .expect("read READY");
            assert!(line.trim() == "READY", "helper signalled {line:?}");
        }
        // let it write for a bit, then kill hard mid-loop
        std::thread::sleep(Duration::from_millis(40 * u64::from(round)));
        child.kill().expect("kill child");
        let _ = child.wait();

        // 1) reopen must succeed after the crash
        let board = BoardStore::open_redb(&db).expect("reopen after crash");
        drop(board);

        // 2) log and snapshot invariants
        let store = RedbStore::open(&db).unwrap();
        let last = store.last_seq().unwrap();
        let snap_seq = store.snapshot_seq().unwrap();
        assert_eq!(
            snap_seq, last,
            "round {round}: snapshot must cover the whole log (single-txn append)"
        );
        assert!(last > 0, "round {round}: child had time to write");

        let all = store.read_since(0, usize::MAX).unwrap();
        assert_eq!(all.len() as u64, last, "round {round}: no holes in the log");
        for (i, ev) in all.iter().enumerate() {
            assert_eq!(
                ev.seq,
                i as u64 + 1,
                "round {round}: strictly consecutive seqs"
            );
        }

        // 3) fold-from-zero equals the stored snapshot
        let folded = fold(Kanban::default(), &all);
        let snap = store.load_snapshot().unwrap().expect("snapshot present");
        assert_eq!(folded, snap, "round {round}: projection == fold(log)");

        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        drop(store);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }
}
