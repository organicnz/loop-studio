//! Log rotation, retention, task transcript management, and event logging.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use super::{home_dir, json_escape, now_unix};

pub fn append_jsonl(path: &str, line: &str) {
    if let Some(parent) = Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        // One write syscall for the record and its newline. `writeln!` is
        // two, and under O_APPEND with a thread per connection two writers
        // can land their bodies adjacent before either newline, welding two
        // JSON objects into one unparseable line.
        let mut record = String::with_capacity(line.len() + 1);
        record.push_str(line);
        record.push('\n');
        let _ = f.write_all(record.as_bytes());
    }
}

/// The directory every launchd plist in this project writes its logs to.
pub fn logs_dir() -> String {
    format!("{}/Library/Logs", home_dir())
}

/// How large a daemon's own log may grow before it is rotated.
pub const LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// How many rotated generations are kept beside the live log.
pub const LOG_KEEP: usize = 3;

/// Rotate `path` once it grows past `max_bytes`, keeping `keep` generations.
///
/// Returns the bytes rotated away, or 0 when nothing needed doing.
pub fn rotate_log_if_large(path: &str, max_bytes: u64, keep: usize) -> u64 {
    let size = match fs::metadata(path) {
        Ok(m) if m.is_file() => m.len(),
        _ => return 0,
    };
    if size <= max_bytes {
        return 0;
    }
    if keep == 0 {
        let _ = fs::remove_file(path);
        return size;
    }
    for i in (1..=keep).rev() {
        let from = format!("{}.{}", path, i);
        if i == keep {
            let _ = fs::remove_file(&from);
        } else {
            let to = format!("{}.{}", path, i + 1);
            if fs::rename(&from, &to).is_err() {
                let _ = fs::remove_file(&from);
            }
        }
    }
    let _ = fs::rename(path, format!("{}.1", path));
    let _ = fs::write(path, b"");
    size
}

/// Keep this daemon's own launchd logs bounded for as long as it runs.
pub fn spawn_log_rotator(names: &[&str], max_bytes: u64, keep: usize, interval: Duration) {
    let owned: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || loop {
        for name in &owned {
            let path = format!("{}/{}", logs_dir(), name);
            let rotated = rotate_log_if_large(&path, max_bytes, keep);
            if rotated > 0 {
                eprintln!(
                    "[loop-studio] rotated {} ({} MiB, keeping {} generations)",
                    path,
                    rotated / (1024 * 1024),
                    keep
                );
            }
        }
        std::thread::sleep(interval);
    });
}

/// How much space this project's logs hold, and how many files they are.
pub fn lac_logs_footprint() -> (u64, usize) {
    let mut bytes = 0;
    let mut files = 0;
    if let Ok(entries) = fs::read_dir(logs_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !is_project_log_name(&name.to_string_lossy()) {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    bytes += meta.len();
                    files += 1;
                }
            }
        }
    }
    (bytes, files)
}

/// Is this file one of this project's logs in `~/Library/Logs`?
pub fn is_project_log_name(name: &str) -> bool {
    name.starts_with("loop-studio-") || name.starts_with("lac-")
}

/// Remove `path`'s rotated generations, leaving the live log in place.
/// Returns (bytes_reclaimed, failures).
pub fn purge_rotated_generations(path: &str) -> (u64, usize) {
    let mut reclaimed = 0;
    let mut failed = 0;
    for i in 1..=64 {
        let generation = format!("{}.{}", path, i);
        match fs::metadata(&generation) {
            Ok(m) if m.is_file() => match fs::remove_file(&generation) {
                Ok(()) => reclaimed += m.len(),
                Err(e) => {
                    failed += 1;
                    eprintln!("could not remove {generation}: {e}");
                }
            },
            _ => break,
        }
    }
    (reclaimed, failed)
}

/// Every name `loop-studio logs <which>` accepts, for the refusal message.
pub fn log_which_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = log_file_names();
    names.push("tasks");
    names
}

/// The launchd log names this project writes, for a daemon to rotate.
pub fn log_names_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "mlx" => &["loop-studio-serve-mlx.log", "loop-studio-serve-mlx.err"],
        "llama" => &["loop-studio-serve-llama.log", "loop-studio-serve-llama.err"],
        "worker" => &["loop-studio-worker.log", "loop-studio-worker.err"],
        _ => &["loop-studio-router.log", "loop-studio-router.err"],
    }
}

/// Every log this project writes, derived from [`log_names_for`].
pub fn log_file_names() -> Vec<&'static str> {
    ["mlx", "llama", "worker", "router"]
        .into_iter()
        .flat_map(log_names_for)
        .copied()
        .collect()
}

/// The per-task agent transcripts, in a directory of their own.
pub fn task_logs_dir() -> String {
    task_logs_dir_in(&home_dir())
}

/// [`task_logs_dir`] for an explicit home.
pub fn task_logs_dir_in(home: &str) -> String {
    format!("{home}/.loop-studio/task-logs")
}

/// Bytes and file count under [`task_logs_dir`].
pub fn task_logs_footprint() -> (u64, usize) {
    task_logs_footprint_in(&task_logs_dir())
}

/// [`task_logs_footprint`] for an explicit directory.
pub fn task_logs_footprint_in(dir: &str) -> (u64, usize) {
    let mut bytes = 0;
    let mut files = 0;
    walk_dir(dir, 0, &mut bytes, &mut files);
    (bytes, files)
}

fn walk_dir(dir: &str, depth: u32, bytes: &mut u64, files: &mut usize) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            walk_dir(&entry.path().to_string_lossy(), depth + 1, bytes, files);
        } else if meta.is_file() {
            *bytes += meta.len();
            *files += 1;
        }
    }
}

/// The oldest `count` transcripts by mtime, for the retention policy.
pub fn oldest_task_logs(count: usize) -> Vec<(String, u64)> {
    oldest_task_logs_in(&task_logs_dir(), count)
}

/// [`oldest_task_logs`] for an explicit directory.
pub fn oldest_task_logs_in(dir: &str, count: usize) -> Vec<(String, u64)> {
    let mut dated: Vec<(std::time::SystemTime, String, u64)> = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        dated.push((
            modified,
            entry.path().to_string_lossy().into_owned(),
            meta.len(),
        ));
    }
    dated.sort_by_key(|entry| entry.0);
    dated
        .into_iter()
        .take(count)
        .map(|(_, path, len)| (path, len))
        .collect()
}

/// How many per-task transcripts to keep, and the ceiling one may reach.
pub const TASK_LOG_KEEP: usize = 40;
pub const TASK_LOG_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Trim the transcript tree to [`TASK_LOG_KEEP`], and cap any file over
/// [`TASK_LOG_MAX_BYTES`]. Returns (files removed, bytes reclaimed).
pub fn trim_task_logs() -> (usize, u64) {
    trim_task_logs_in(&task_logs_dir())
}

/// [`trim_task_logs`] for an explicit directory.
pub fn trim_task_logs_in(dir: &str) -> (usize, u64) {
    let mut removed = 0;
    let mut bytes = 0;
    for (path, len) in oldest_task_logs_in(dir, usize::MAX) {
        if len <= TASK_LOG_MAX_BYTES {
            continue;
        }
        match fs::OpenOptions::new().write(true).open(&path) {
            Ok(f) => {
                if f.set_len(TASK_LOG_MAX_BYTES).is_ok() {
                    bytes += len - TASK_LOG_MAX_BYTES;
                }
            }
            Err(_) => continue,
        }
    }
    let all = oldest_task_logs_in(dir, usize::MAX);
    let doomed = all.len().saturating_sub(TASK_LOG_KEEP);
    for (path, len) in all.into_iter().take(doomed) {
        if fs::remove_file(&path).is_ok() {
            removed += 1;
            bytes += len;
        }
    }
    (removed, bytes)
}

/// The file `loop-studio logs <which>` should show.
pub fn log_file_name(which: &str) -> Option<&'static str> {
    let key = which.trim().to_ascii_lowercase();
    let wanted = match key.as_str() {
        "mlx" | "serve-mlx" => "loop-studio-serve-mlx.log",
        "mlx-err" | "serve-mlx-err" => "loop-studio-serve-mlx.err",
        "llama" | "serve-llama" => "loop-studio-serve-llama.log",
        "llama-err" | "serve-llama-err" => "loop-studio-serve-llama.err",
        "worker" => "loop-studio-worker.log",
        "worker-err" => "loop-studio-worker.err",
        "router" => "loop-studio-router.log",
        "router-err" => "loop-studio-router.err",
        other if log_file_names().contains(&other) => other,
        _ => return None,
    };
    log_file_names().into_iter().find(|n| *n == wanted)
}

/// Worker / daemon event log path.
pub fn event_log_path() -> String {
    format!("{}/.loop-studio/worker-events.jsonl", home_dir())
}

pub fn log_event(kind: &str, msg: &str) {
    let line = format!(
        "{{\"ts\":{},\"event\":\"{}\",\"msg\":\"{}\"}}",
        now_unix(),
        json_escape(kind),
        json_escape(msg)
    );
    append_jsonl(&event_log_path(), &line);
}

#[cfg(test)]
mod log_rotation_tests {
    #![deny(dead_code)]

    use super::super::now_unix;
    use super::*;

    #[test]
    fn a_log_is_rotated_past_the_limit_and_keeps_bounded_generations() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-log-rotate-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir
            .join("loop-studio-router.log")
            .to_string_lossy()
            .into_owned();

        fs::write(&path, b"small").expect("write");
        assert_eq!(rotate_log_if_large(&path, 100, 3), 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), "small");

        fs::write(&path, vec![b'x'; 200]).expect("write");
        assert_eq!(rotate_log_if_large(&path, 100, 3), 200);
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(fs::metadata(format!("{path}.1")).unwrap().len(), 200);

        fs::write(&path, vec![b'y'; 150]).expect("write");
        assert_eq!(rotate_log_if_large(&path, 100, 3), 150);
        assert_eq!(fs::metadata(format!("{path}.1")).unwrap().len(), 150);
        assert_eq!(fs::metadata(format!("{path}.2")).unwrap().len(), 200);

        fs::write(&path, vec![b'z'; 120]).expect("write");
        assert_eq!(rotate_log_if_large(&path, 100, 3), 120);
        assert_eq!(fs::metadata(format!("{path}.1")).unwrap().len(), 120);
        assert_eq!(fs::metadata(format!("{path}.2")).unwrap().len(), 150);
        assert_eq!(fs::metadata(format!("{path}.3")).unwrap().len(), 200);

        fs::write(&path, vec![b'w'; 110]).expect("write");
        assert_eq!(rotate_log_if_large(&path, 100, 3), 110);
        assert!(
            fs::metadata(format!("{path}.4")).is_err(),
            "kept a fourth generation"
        );
        assert_eq!(fs::metadata(format!("{path}.3")).unwrap().len(), 150);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_or_foreign_file_is_never_touched() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-log-rotate-missing-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let absent = dir.join("nothing-here.log").to_string_lossy().into_owned();
        assert_eq!(rotate_log_if_large(&absent, 1, 3), 0);
        assert!(!std::path::Path::new(&absent).exists());

        let as_dir = dir.join("loop-studio-router.log");
        fs::create_dir_all(&as_dir).expect("mkdir");
        assert_eq!(rotate_log_if_large(&as_dir.to_string_lossy(), 1, 3), 0);

        let flat = dir.join("flat.log").to_string_lossy().into_owned();
        fs::write(&flat, vec![b'q'; 50]).expect("write");
        assert_eq!(rotate_log_if_large(&flat, 10, 0), 50);
        assert!(!std::path::Path::new(&flat).exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_usage_log_keeps_exactly_the_generation_its_reader_reads() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-usage-rotate-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir
            .join("router-usage.jsonl")
            .to_string_lossy()
            .into_owned();

        for round in 1..=3u8 {
            fs::write(&path, vec![b'x'; 2_000 * u64::from(round) as usize]).expect("write");
            assert!(
                rotate_log_if_large(&path, 1_000, 1) > 0,
                "round {round} must rotate"
            );
            assert!(
                fs::metadata(format!("{path}.2")).is_err(),
                "kept a generation the reader never reads"
            );
        }
        assert_eq!(fs::metadata(format!("{path}.1")).unwrap().len(), 6_000);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_rule_matches_the_shared_parity_table() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/lock-parity.tsv");
        let table = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} unreadable: {e}", path.display()));
        let mut seen: Vec<&str> = Vec::new();
        for line in table.lines() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(cols.len(), 5, "malformed parity row: {line:?}");
            let (case, owner, age, probe, expect) = (cols[0], cols[1], cols[2], cols[3], cols[4]);
            let owner: Option<&str> = if owner == "-" { None } else { Some(owner) };
            let age: Option<u64> = age.parse().ok();
            let probe = |_pid: i32| match probe {
                "alive" => Some(true),
                "dead" => Some(false),
                "unprobeable" => None,
                other => panic!("unknown probe {other:?} in row {case}"),
            };
            let got = super::super::lock::lock_is_reclaimable(owner, age, 30, probe);
            let want = match expect {
                "reclaim" => true,
                "keep" => false,
                other => panic!("unknown expectation {other:?} in row {case}"),
            };
            assert_eq!(got, want, "parity row {case} disagrees: reclaimable={got}");
            seen.push(case);
        }
        const EXPECTED: [&str; 19] = [
            "no-record-fresh",
            "no-record-at-backstop",
            "no-record-aged",
            "no-record-unknown-age",
            "empty-record-fresh",
            "empty-record-at-backstop",
            "garbage-record-fresh",
            "garbage-record-at-backstop",
            "negative-pid-fresh",
            "negative-pid-aged",
            "live-owner-ancient",
            "live-owner-just-born",
            "dead-owner-fresh",
            "dead-owner-ancient",
            "unprobeable-fresh",
            "unprobeable-just-under",
            "unprobeable-at-backstop",
            "legacy-live-owner",
            "legacy-dead-owner",
        ];
        seen.sort_unstable();
        let mut expected = EXPECTED;
        expected.sort_unstable();
        assert_eq!(
            seen, expected,
            "the parity table is not the agreed set of cases: a row was removed, added or renamed"
        );
    }

    #[test]
    fn every_daemon_rotates_the_logs_its_plist_opens() {
        assert_eq!(
            log_names_for("router"),
            &["loop-studio-router.log", "loop-studio-router.err"]
        );
        assert_eq!(
            log_names_for("mlx"),
            &["loop-studio-serve-mlx.log", "loop-studio-serve-mlx.err"]
        );
        assert_eq!(
            log_names_for("worker"),
            &["loop-studio-worker.log", "loop-studio-worker.err"]
        );
        assert_eq!(
            log_names_for("nonsense"),
            &["loop-studio-router.log", "loop-studio-router.err"]
        );
    }

    #[test]
    fn the_names_are_the_ones_the_plists_open() {
        let launchd = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../launchd");
        let plist = |file: &str| -> String {
            fs::read_to_string(launchd.join(file))
                .unwrap_or_else(|e| panic!("{} unreadable: {e}", launchd.join(file).display()))
        };
        let opened = |xml: &str| -> Vec<String> {
            let mut names = Vec::new();
            for key in ["<key>StandardOutPath</key>", "<key>StandardErrorPath</key>"] {
                let at = xml.find(key).unwrap_or_else(|| panic!("{key} missing"));
                let start = xml[at..]
                    .find("<string>")
                    .map(|i| at + i + "<string>".len())
                    .expect("a string value");
                let end = xml[start..]
                    .find("</string>")
                    .map(|i| start + i)
                    .expect("a closed string");
                names.push(
                    xml[start..end]
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
            names
        };
        for (plist_file, kind) in [
            ("org.loopstudio.router.plist", "router"),
            ("org.loopstudio.serve-mlx.plist", "mlx"),
            ("org.loopstudio.worker.plist", "worker"),
        ] {
            let from_plist = opened(&plist(plist_file));
            let rotating: Vec<String> = log_names_for(kind).iter().map(|s| s.to_string()).collect();
            assert_eq!(
                rotating, from_plist,
                "{plist_file} opens {from_plist:?} but the {kind} daemon rotates {rotating:?}"
            );
        }
    }
}

#[cfg(test)]
mod log_retention_tests {
    use super::super::now_unix;
    use super::*;

    #[test]
    fn purging_reclaims_the_generations_and_leaves_the_live_log() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-log-purge-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir
            .join("loop-studio-router.log")
            .to_string_lossy()
            .into_owned();

        fs::write(&path, b"live").expect("write live");
        for i in 1..=3 {
            fs::write(format!("{path}.{i}"), vec![b'x'; i * 100]).expect("write generation");
        }
        fs::write(dir.join("something-else.log"), b"keep me").expect("write other");

        assert_eq!(purge_rotated_generations(&path), (100 + 200 + 300, 0));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "live",
            "the live log must survive"
        );
        for i in 1..=3 {
            assert!(
                fs::metadata(format!("{path}.{i}")).is_err(),
                "generation {i} survived the purge"
            );
        }
        assert!(fs::metadata(dir.join("something-else.log")).is_ok());

        assert_eq!(purge_rotated_generations(&path), (0, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gap_ends_the_walk() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-log-purge-gap-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir
            .join("loop-studio-router.log")
            .to_string_lossy()
            .into_owned();
        fs::write(format!("{path}.1"), vec![b'x'; 10]).expect("write");
        fs::write(format!("{path}.3"), vec![b'x'; 10]).expect("write");
        assert_eq!(purge_rotated_generations(&path), (10, 0));
        assert!(fs::metadata(&path).is_err(), "no live log to protect");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bytes_the_purge_could_not_reclaim_are_not_counted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "loop-studio-log-purge-denied-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir
            .join("loop-studio-router.log")
            .to_string_lossy()
            .into_owned();
        fs::write(format!("{path}.1"), vec![b'x'; 4096]).expect("write generation");
        let mode = fs::metadata(&dir).expect("dir meta").permissions().mode();
        let root = running_as_root();
        if root {
            eprintln!("skipping: root ignores directory permissions");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("chmod");
        let (reclaimed, failed) = purge_rotated_generations(&path);
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).expect("restore");

        assert_eq!(failed, 1, "a denied unlink must be reported as a failure");
        assert_eq!(
            reclaimed, 0,
            "bytes that were not removed must not be reported as reclaimed"
        );
        assert!(
            fs::metadata(format!("{path}.1")).is_ok(),
            "the file is still there"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        unsafe { geteuid() == 0 }
    }

    #[test]
    fn the_transcript_tree_is_trimmed_oldest_first() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-task-logs-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let logs = task_logs_dir_in(&dir.to_string_lossy());
        fs::create_dir_all(&logs).expect("transcript dir");

        let count = TASK_LOG_KEEP + 10;
        let base =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for i in 0..count {
            let path = std::path::Path::new(&logs).join(format!("task-{i:03}.log"));
            fs::write(&path, vec![b'x'; 16]).expect("write");
            let when = base + std::time::Duration::from_secs(i as u64 * 60);
            let stamp = std::time::UNIX_EPOCH
                + std::time::Duration::from_secs(
                    when.duration_since(std::time::UNIX_EPOCH)
                        .expect("epoch")
                        .as_secs(),
                );
            set_mtime(&path, stamp);
        }

        let (bytes, files) = task_logs_footprint_in(&logs);
        assert_eq!(files, count, "the footprint must see the whole tree");
        assert_eq!(bytes, count as u64 * 16);

        let (removed, reclaimed) = trim_task_logs_in(&logs);
        assert_eq!(removed, 10, "exactly the surplus is removed: {removed}");
        assert_eq!(reclaimed, 10 * 16);

        let (_, after) = task_logs_footprint_in(&logs);
        assert_eq!(
            after, TASK_LOG_KEEP,
            "the tree is bounded afterwards: {after}"
        );

        assert!(
            fs::metadata(std::path::Path::new(&logs).join("task-000.log")).is_err(),
            "the oldest transcript must be the first to go"
        );
        assert!(
            fs::metadata(std::path::Path::new(&logs).join(format!("task-{:03}.log", count - 1)))
                .is_ok(),
            "the newest transcript must survive"
        );

        let (removed_again, _) = trim_task_logs_in(&logs);
        assert_eq!(removed_again, 0, "a settled tree must be left alone");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_runaway_transcript_is_capped() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-task-cap-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let logs = task_logs_dir_in(&dir.to_string_lossy());
        fs::create_dir_all(&logs).expect("transcript dir");
        let path = std::path::Path::new(&logs).join("one.log");
        fs::write(&path, vec![b'x'; 4096]).expect("write");

        let (removed, _reclaimed) = trim_task_logs_in(&logs);
        assert_eq!(removed, 0, "a single transcript is never removed by count");
        assert_eq!(fs::metadata(&path).expect("still there").len(), 4096);

        let ceiling = TASK_LOG_MAX_BYTES;
        fs::write(&path, vec![b'x'; 1]).expect("rewrite");
        {
            let f = fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("open");
            f.set_len(ceiling + 1).expect("grow");
        }
        trim_task_logs_in(&logs);
        let len = fs::metadata(&path).expect("still there").len();
        assert_eq!(
            len, ceiling,
            "an oversized transcript is capped, not deleted"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn set_mtime(path: &std::path::Path, when: std::time::SystemTime) {
        let secs = when
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_secs() as isize;
        let times = [Timespec {
            sec: secs,
            nsec: 0isize,
        }; 2];
        let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).expect("path cstring");
        unsafe extern "C" {
            fn utimensat(
                dirfd: i32,
                path: *const std::ffi::c_char,
                times: *const Timespec,
                flags: i32,
            ) -> i32;
        }
        const AT_FDCWD: i32 = -2;
        let r = unsafe { utimensat(AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(r, 0, "could not set mtime on {path:?}");
    }

    #[cfg(unix)]
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Timespec {
        sec: isize,
        nsec: isize,
    }

    #[test]
    fn the_footprint_counts_our_logs_and_nothing_else() {
        let (bytes, files) = lac_logs_footprint();
        assert!(
            files <= 64,
            "counting something that is not a log file: {files} files, {bytes} bytes"
        );
        let dir = logs_dir();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !is_project_log_name(&name) {
                    continue;
                }
                if let Ok(m) = entry.metadata() {
                    if m.is_file() {
                        assert!(bytes >= m.len(), "undercounted {name}");
                    }
                }
            }
        }
    }
}
