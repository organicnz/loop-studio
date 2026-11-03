//! Inter-process directory lock — crash-safe, stale-lock reclaim.

use std::fs;
use std::process::Command;
use std::time::SystemTime;

use super::{home_dir, now_unix};

/// Crash-safe inter-process lock via atomic `create_dir` + owner record.
/// Prevents two `loop-studio worker` daemons (launchd + terminal) draining the
/// same queue twice. Stale locks (dead owner or age) are reclaimed.
pub struct DirLock {
    dir: String,
}

impl DirLock {
    pub fn dir(&self) -> &str {
        &self.dir
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // Only remove a lock that is still ours. A reclaim deletes the
        // directory and the new owner writes its own record into it; the old
        // owner's `remove_dir_all` would then delete the *new* owner's lock, and
        // the queue would be handed to a third worker the moment either of them
        // exited. Comparing the owner record is the only thing that
        // distinguishes "my lock" from "a lock at my path".
        let owner_path = format!("{}/owner", self.dir);
        let still_ours = fs::read_to_string(&owner_path)
            .ok()
            .and_then(|record| {
                record
                    .split_whitespace()
                    .next()
                    .and_then(|pid| pid.parse::<i32>().ok())
            })
            .is_some_and(|pid| pid == std::process::id() as i32);
        if still_ours {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Liveness probe: Some(true) alive, Some(false) dead,
/// None when the probe itself could not run (fail-open unknown).
fn process_alive(pid: i32) -> Option<bool> {
    let out = Command::new("kill").args(["-0", &pid.to_string()]).output().ok()?;
    Some(out.status.success())
}

/// Whether an existing lock directory may be taken over.
///
/// Split out from [`acquire_lock`] because the rule is the whole safety
/// argument and it is worth asserting without a filesystem race. `owner` is
/// the record read from the lock directory, `dir_age_secs` how long ago that
/// directory was last modified, `probe` the pid liveness check.
///
/// The only unconditional reclaim is "the recorded pid is provably gone".
/// Everything else — no record, a record that will not parse, a pid the
/// platform cannot answer for — has to wait out `stale_secs`.
///
/// The previous version read an *unparsable* record as definitively stale
/// while treating a *missing* one conservatively, which is backwards: the
/// acquirer writes the record with `File::create` (O_TRUNC) and then
/// `write_all`, so between those two syscalls the file exists and is empty, and
/// a second worker that arrives in that window parsed `pid = -1`, declared the
/// lock stale and deleted it. Both workers then believed they held
/// `loop-studio-worker` and drained the same Kanban queue, which is the one thing this
/// lock exists to prevent. `kill -9` in that window makes the state permanent.
pub(crate) fn lock_is_reclaimable(
    owner: Option<&str>,
    dir_age_secs: Option<u64>,
    stale_secs: u64,
    probe: impl Fn(i32) -> Option<bool>,
) -> bool {
    let aged_out = dir_age_secs.is_some_and(|age| age >= stale_secs);
    let pid = owner
        .and_then(|record| record.split_whitespace().next())
        .and_then(|pid| pid.parse::<i32>().ok())
        .filter(|pid| *pid > 0);
    match pid {
        Some(pid) => match probe(pid) {
            // The owner is gone. Reclaim now rather than waiting: this is the
            // crash case the lock exists to recover from.
            Some(false) => true,
            // A live owner keeps the lock regardless of age. A forward clock
            // jump must not hand one queue to two workers, and a long-running
            // worker must not lose its lock for being slow.
            Some(true) => false,
            // The pid could not be probed at all, so age is all there is.
            None => aged_out,
        },
        None => aged_out,
    }
}

/// Acquire `~/.lac/locks/<name>`. Returns None when a live owner holds it.
/// `stale_secs` is only a backstop for when the owner pid cannot be probed;
/// a live owner keeps the lock regardless of wall-clock age (a forward
/// clock jump must never hand one queue to two workers).
pub fn acquire_lock(name: &str, stale_secs: u64) -> Option<DirLock> {
    let parent = format!("{}/.lac/locks", home_dir());
    let _ = fs::create_dir_all(&parent);
    let dir = format!("{}/{}", parent, name);
    for _ in 0..2 {
        match fs::create_dir(&dir) {
            Ok(()) => {
                let owner = format!("{} {}", std::process::id(), now_unix());
                // Truncate-then-write is the window `lock_is_reclaimable` is
                // written to survive, but the record is still written before
                // the lock is handed back.
                let _ = fs::write(format!("{}/owner", dir), owner);
                return Some(DirLock { dir });
            }
            Err(_) => {
                let owner_path = format!("{}/owner", dir);
                let owner = fs::read_to_string(&owner_path).ok();
                let dir_age = fs::metadata(&dir)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|modified| now_unix().saturating_sub(modified.as_secs()));
                if lock_is_reclaimable(owner.as_deref(), dir_age, stale_secs, process_alive) {
                    let _ = fs::remove_dir_all(&dir);
                    continue;
                }
                return None;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![deny(dead_code)]
    use super::*;

    #[test]
    fn lock_excludes_second_owner_and_releases_on_drop() {
        let name = format!("test-{}", std::process::id());
        let a = acquire_lock(&name, 60);
        assert!(a.is_some());
        assert!(acquire_lock(&name, 60).is_none());
        drop(a);
        assert!(acquire_lock(&name, 60).is_some());
    }

    /// The reclaim rule, on states that are awkward to produce deliberately
    /// from another process but trivial to state here. The one that matters is
    /// the empty record: the acquirer creates it with `File::create` (O_TRUNC)
    /// and then writes into it, so a second worker arriving in that window used
    /// to read `pid = -1`, call the lock stale, delete it, and take the queue
    /// while the first worker still held it.
    #[test]
    fn a_lock_is_reclaimed_only_on_proof_or_on_age() {
        let alive = |_: i32| Some(true);
        let dead = |_: i32| Some(false);
        let unprobeable = |_: i32| None;

        // Provably gone: reclaimed at once, which is the crash case.
        assert!(lock_is_reclaimable(Some("999999 100"), Some(0), 900, dead));
        // Alive: held, however old the directory is. A forward clock jump must
        // not hand one queue to two workers.
        assert!(!lock_is_reclaimable(Some("4242 100"), Some(100_000), 900, alive));
        // Alive and unprobeable pid: still held.
        assert!(!lock_is_reclaimable(Some("4242 100"), Some(5), 900, unprobeable));
        // Alive-looking but the platform cannot answer, and it is old enough.
        assert!(lock_is_reclaimable(Some("4242 100"), Some(901), 900, unprobeable));
        // The two states that used to disagree. An empty or unparsable record
        // is exactly the truncate window, so it waits out the backstop like a
        // missing record does; neither is a proof of anything.
        for record in [Some(""), Some("   "), Some("not-a-pid 5"), Some("-1 5"), None] {
            assert!(
                !lock_is_reclaimable(record, Some(10), 900, dead),
                "record {record:?} must not be taken as proof"
            );
            assert!(
                lock_is_reclaimable(record, Some(901), 900, dead),
                "record {record:?} ages out like a missing one"
            );
        }
        // No record and no age to judge by: hold.
        assert!(!lock_is_reclaimable(None, None, 900, dead));
    }

    /// A lock that was reclaimed out from under its owner must not be deleted
    /// by that owner's `Drop`. Otherwise the reclaimer's lock disappears the
    /// moment the old worker exits, and a third worker walks in.
    #[test]
    fn a_lock_only_removes_itself() {
        let name = format!("test-drop-owner-{}", std::process::id());
        let lock = acquire_lock(&name, 60).expect("first owner");
        let dir = lock.dir().to_string();
        // Simulate the reclaim: the directory is replaced by someone else's.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("reclaim creates the directory");
        fs::write(format!("{}/owner", dir), format!("{} {}", std::process::id() + 1, now_unix()))
            .expect("new owner record");
        drop(lock);
        assert!(
            std::path::Path::new(&dir).exists(),
            "the new owner's lock must survive the old owner's drop"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
