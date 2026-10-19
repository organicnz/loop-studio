//! bootstrap v2.6 — idempotent machine setup (Rust).
//!
//! v2.6: never creates directories under /Volumes unless the volume is
//! really mounted (the old code shadowed the future mount with an
//! internal-disk directory of the same name); never overwrites
//! user-edited loop files; actually installs the zprofile hooks the
//! docs always claimed existed; verifies Time Machine exclusions.

mod common;

use std::env;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

fn get_project_root() -> String {
    common::project_root()
}

fn tm_excluded(path: &str) -> bool {
    Command::new("tmutil")
        .args(["isexcluded", path])
        .output()
        .ok()
        .map(|o| {
            let t = String::from_utf8_lossy(&o.stdout);
            t.contains("[Excluded]")
        })
        .unwrap_or(false)
}

const ZPROFILE_MARKER: &str = "# >>> Loop Studio env (managed by loop-studio bootstrap) >>>";
const ZPROFILE_END: &str = "# <<< Loop Studio env <<<";

/// The managed block, and nothing else. Written between the two markers so a
/// later run can replace exactly this region and never touch a line the user
/// added.
fn managed_block(model_base: &str) -> String {
    format!(
        "{ZPROFILE_MARKER}\nexport HF_HOME=\"{model_base}/hf\"\nexport HF_HUB_CACHE=\"{model_base}/hf/hub\"\nexport PATH=\"$HOME/.local/bin:$PATH\"\n{ZPROFILE_END}\n"
    )
}

/// What happened to the managed block. `Failed` is separate from the rest
/// because it is the one case that used to be reported as a warning while the
/// run went on to print "all green".
#[derive(Debug, PartialEq, Eq)]
enum ZprofileOutcome {
    /// Already correct — nothing written.
    Unchanged,
    /// No block present; one was appended.
    Installed,
    /// A block was present but recorded a different model library; it was
    /// replaced in place.
    Reconciled,
    Failed(String),
}

/// Reconcile the managed block in `zp` against `model_base`.
///
/// The one-shot marker check this replaces was a substring test: present meant
/// "nothing to do", forever. So the first bootstrap run decided where
/// `HF_HOME` points and no later run could ever change it — including the run
/// where `/Volumes/AIModels` finished mounting, or stopped being mounted. The
/// file went on claiming an internal-disk path while every other step in the
/// same run used the external volume, and the two halves of the model library
/// silently diverged. Mount state is exactly the thing that changes; a
/// one-shot marker cannot be the right tool for it.
fn ensure_zprofile_in(zp: &str, model_base: &str) -> ZprofileOutcome {
    let current = fs::read_to_string(zp).unwrap_or_default();
    let desired = managed_block(model_base);
    let start = current.find(ZPROFILE_MARKER);
    let end = current.find(ZPROFILE_END);
    let updated = match (start, end) {
        (Some(start), Some(end)) if end > start => {
            // Take the whole end line with it, so the replacement cannot
            // inherit a stray fragment of the old block.
            let after = current[end..]
                .find('\n')
                .map(|nl| end + nl + 1)
                .unwrap_or(current.len());
            if current[start..after] == desired {
                return ZprofileOutcome::Unchanged;
            }
            let mut out = String::with_capacity(current.len() + desired.len());
            out.push_str(&current[..start]);
            out.push_str(&desired);
            out.push_str(&current[after..]);
            out
        }
        // No complete managed block. If a start marker exists with no end
        // marker the file was hand-edited into a broken state; appending a
        // fresh block leaves the orphan visible rather than silently consuming
        // text the user may rely on.
        _ => {
            let mut out = current;
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
            out.push_str(&desired);
            out
        }
    };
    match common::atomic_write(zp, &updated) {
        Ok(()) if start.is_some() && end.is_some() => ZprofileOutcome::Reconciled,
        Ok(()) => ZprofileOutcome::Installed,
        Err(e) => ZprofileOutcome::Failed(e.to_string()),
    }
}

fn ensure_zprofile(model_base: &str) -> bool {
    let zp = format!("{}/.zprofile", common::home_dir());
    match ensure_zprofile_in(&zp, model_base) {
        ZprofileOutcome::Unchanged => {
            eprintln!("   zprofile hooks already correct");
            true
        }
        ZprofileOutcome::Installed => {
            eprintln!("   zprofile hooks installed in {}", zp);
            true
        }
        ZprofileOutcome::Reconciled => {
            eprintln!("   zprofile hooks updated to {}", model_base);
            true
        }
        ZprofileOutcome::Failed(e) => {
            eprintln!("   zprofile install failed: {}", e);
            eprintln!(
                "   HF_HOME/HF_HUB_CACHE are NOT set for new shells; every model download will \
                 use the wrong cache until this succeeds"
            );
            false
        }
    }
}

/// Copy one template file, reporting what actually happened. `fs::copy` on a
/// directory always fails with EISDIR, and the old loop tried every entry in
/// `templates/loops` including subdirectories, then printed "installed loop"
/// for a file that was never created.
fn install_template(src: &Path, dest: &str) -> Result<bool, String> {
    let meta = match fs::metadata(src) {
        Ok(m) => m,
        Err(e) => return Err(format!("{}: {e}", src.display())),
    };
    if !meta.is_file() {
        // Not an error: a nested directory in the template tree is a layout
        // choice, not a broken install.
        return Ok(false);
    }
    match fs::copy(src, dest) {
        Ok(_) => Ok(true),
        Err(e) => Err(format!("{} -> {dest}: {e}", src.display())),
    }
}

fn main() {
    common::ignore_sigpipe();
    eprintln!("=== Loop Studio bootstrap v2.6 (Rust, idempotent) ===");
    // Every path below is `home_dir() + something`, and `common::home_dir()`
    // now refuses rather than falling back to "." — a relative path is not a
    // home directory, it is "wherever this process was launched", and
    // bootstrap would otherwise create ./.zprofile and ./.ollama in whatever
    // directory it was invoked from and report success. One rule, from the
    // shared helper, so the two cannot drift: it used to accept a merely
    // non-empty $HOME here while the helper required an absolute one.
    let home = match common::home_dir_from(env::var("HOME").ok().as_deref()) {
        Ok(home) => home,
        Err(_) => {
            eprintln!("$HOME is not set to an absolute path, so there is no home directory to set up.");
            eprintln!("Run this from a login shell, or set HOME first.");
            std::process::exit(78);
        }
    };
    let root = get_project_root();
    // Two counters, because "you may not need this" and "I tried and it did
    // not work" are different things and were being reported the same way.
    let mut warns = 0;
    let mut failures = 0;

    // 1. Homebrew
    eprintln!("1. Ensuring Homebrew...");
    let brew_path = common::which("brew").unwrap_or_default();
    if brew_path.is_empty() {
        eprintln!("   brew NOT FOUND — install from https://brew.sh then rerun");
        warns += 1;
    } else {
        eprintln!("   brew: {}", brew_path);
    }

    // 2. Brew bundle (failures are real errors, not silence).
    let brewfile_path = format!("{}/Brewfile", root);
    if Path::new(&brewfile_path).exists() && !brew_path.is_empty() {
        eprintln!("2. Running brew bundle ({})...", brewfile_path);
        match Command::new("brew")
            .args(["bundle", "install", &format!("--file={}", brewfile_path)])
            .output()
        {
            Ok(o) if o.status.success() => eprintln!("   brew bundle: ok"),
            Ok(o) => {
                eprintln!("   brew bundle exit: {} (see output above)", o.status);
                warns += 1;
            }
            Err(e) => {
                eprintln!("   brew bundle failed to run: {}", e);
                warns += 1;
            }
        }
    } else {
        eprintln!("2. Brewfile step skipped (no Brewfile or no brew).");
    }

    // 3. Rust via rustup
    eprintln!("3. Ensuring Rust via rustup...");
    if common::which("rustc").is_some() {
        if let Ok(o) = Command::new("rustc").arg("-V").output() {
            eprintln!("   rustc: {}", String::from_utf8_lossy(&o.stdout).trim());
        }
    } else {
        eprintln!("   rustc missing — run: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y");
        warns += 1;
    }

    // 4. uv / python
    eprintln!("4. Ensuring uv/python...");
    match Command::new("uv").arg("--version").output() {
        Ok(o) if o.status.success() => {
            eprintln!("   uv: {}", String::from_utf8_lossy(&o.stdout).trim())
        }
        _ => {
            eprintln!("   uv missing — run: curl -LsSf https://astral.sh/uv/install.sh | sh");
            warns += 1;
        }
    }

    // 5. mise
    eprintln!("5. Ensuring mise...");
    match Command::new("mise").arg("--version").output() {
        Ok(o) if o.status.success() => {
            eprintln!("   mise: {}", String::from_utf8_lossy(&o.stdout).trim())
        }
        _ => {
            eprintln!("   mise missing — run: curl https://mise.run | sh");
            warns += 1;
        }
    }

    // 6. Model library dirs — mount-gated, never shadow /Volumes.
    eprintln!("6. Creating model library dirs...");
    let external = common::volume_mounted("/Volumes/AIModels");
    let model_base = if external {
        "/Volumes/AIModels".to_string()
    } else {
        eprintln!("   /Volumes/AIModels not mounted — using internal fallback");
        common::model_base()
    };
    for d in [
        model_base.clone(),
        format!("{}/hf", model_base),
        format!("{}/ollama", model_base),
        format!("{}/lmstudio", model_base),
        format!("{}/indexes", model_base),
    ] {
        match fs::create_dir_all(&d) {
            Ok(()) => eprintln!("   verified: {}", d),
            Err(e) => {
                eprintln!("   FAILED {}: {}", d, e);
                warns += 1;
            }
        }
    }

    // 7. Ollama symlink (migrate-aware, never destroy data).
    eprintln!("7. Setting up ollama symlink...");
    let ollama_parent = format!("{}/.ollama", home);
    // Checked: step 7 writes a symlink inside this directory, so a failure
    // here is the reason that step fails, and the operator should be told
    // which one to look at.
    if let Err(e) = fs::create_dir_all(&ollama_parent) {
        eprintln!("   FAILED {}: {}", ollama_parent, e);
        failures += 1;
    }
    let target = format!("{}/ollama", model_base);
    let link = format!("{}/models", ollama_parent);
    match fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            let ok = fs::read_link(&link).ok().map(|t| t.exists()).unwrap_or(false);
            if ok {
                eprintln!("   symlink healthy: {} -> {}", link, target);
            } else {
                let _ = fs::remove_file(&link);
                match symlink(&target, &link) {
                    Ok(()) => eprintln!("   relinked dangling symlink -> {}", target),
                    Err(e) => {
                        eprintln!("   relink failed: {}", e);
                        warns += 1;
                    }
                }
            }
        }
        Ok(_) => {
            eprintln!("   {} is a real directory with user data — NOT replacing.", link);
            eprintln!("   To migrate: move its contents to {} then rerun.", target);
            warns += 1;
        }
        Err(_) => match symlink(&target, &link) {
            Ok(()) => eprintln!("   symlink: {} -> {}", link, target),
            Err(e) => {
                eprintln!("   symlink failed: {}", e);
                warns += 1;
            }
        },
    }

    // 8. Kanban templates — never overwrite user edits.
    eprintln!("8. Initializing Kanban task & loop templates...");
    let loops_dir = format!("{}/todo/loop-studio-loops", home);
    let old_loops = format!("{}/todo/lac-loops", home);
    if fs::metadata(&old_loops).is_ok() && fs::metadata(&loops_dir).is_err() {
        let _ = fs::create_dir_all(&loops_dir);
        if let Ok(entries) = fs::read_dir(&old_loops) {
            for entry in entries.flatten() {
                let dest = format!("{}/{}", loops_dir, entry.file_name().to_string_lossy());
                let _ = fs::copy(entry.path(), &dest);
            }
        }
        eprintln!("   migrated loops from {}", old_loops);
    }
    if let Err(e) = fs::create_dir_all(&loops_dir) {
        eprintln!("   FAILED {}: {}", loops_dir, e);
        failures += 1;
    }
    let templates_loops = format!("{}/templates/loops", root);
    match fs::read_dir(&templates_loops) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let dest = format!("{}/{}", loops_dir, entry.file_name().to_string_lossy());
                if fs::metadata(&dest).is_ok() {
                    eprintln!("   keeping existing: {}", dest);
                    continue;
                }
                // The copy's result used to be discarded while "installed
                // loop" printed regardless, so a failed copy — including the
                // guaranteed EISDIR from a nested template directory — was
                // reported as a success.
                match install_template(&entry.path(), &dest) {
                    Ok(true) => eprintln!("   installed loop: {}", dest),
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("   FAILED loop {}: {}", dest, e);
                        failures += 1;
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("   FAILED to read {}: {}", templates_loops, e);
            failures += 1;
        }
    }
    let task_template = format!("{}/templates/tasks/loop-studio-tasks.yaml", root);
    let task_dest = format!("{}/todo/loop-studio-tasks.yaml", home);
    let old_task = format!("{}/todo/lac-tasks.yaml", home);
    if fs::metadata(&old_task).is_ok() && fs::metadata(&task_dest).is_err() {
        if let Some(parent) = Path::new(&task_dest).parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::copy(&old_task, &task_dest).is_ok() {
            eprintln!("   migrated task queue from {}", old_task);
        }
    }
    if let Some(parent) = Path::new(&task_dest).parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("   FAILED {}: {}", parent.display(), e);
            failures += 1;
        }
    }
    // "Keeping existing" is a claim about a file, so it has to be true. The
    // `else` also caught the case where the destination is *absent* and the
    // template is *absent* — a sparse clone, or a copy without `templates/` —
    // and printed "keeping existing task queue: ~/todo/loop-studio-tasks.yaml" for a
    // file that does not exist, then counted nothing. `loop-studio loop init` reports
    // the same state correctly as a failure, so the two writers of this one
    // fact disagreed.
    match (fs::metadata(&task_dest), Path::new(&task_template).exists()) {
        (Ok(_), _) => eprintln!("   keeping existing task queue: {}", task_dest),
        (Err(_), true) => match install_template(Path::new(&task_template), &task_dest) {
            Ok(true) => eprintln!("   installed task queue: {}", task_dest),
            Ok(false) => {
                eprintln!("   FAILED task template is not a file: {}", task_template);
                failures += 1;
            }
            Err(e) => {
                eprintln!("   FAILED task queue: {}", e);
                failures += 1;
            }
        },
        (Err(_), false) => {
            eprintln!(
                "   FAILED no task queue at {} and no template at {} to install from",
                task_dest, task_template
            );
            failures += 1;
        }
    }

    // 9. Shell env hooks (the docs always promised these).
    eprintln!("9. Ensuring shell env hooks...");
    if !ensure_zprofile(&model_base) {
        failures += 1;
    }

    // 10. Time Machine exclusion, verified.
    eprintln!("10. Applying Time Machine exclusions...");
    let _ = Command::new("tmutil").args(["addexclusion", &model_base]).output();
    if tm_excluded(&model_base) {
        eprintln!("   verified excluded: {}", model_base);
    } else {
        eprintln!("   WARNING: tmutil exclusion not confirmed for {}", model_base);
        warns += 1;
    }

    match bootstrap_verdict(failures, warns) {
        BootstrapVerdict::Green => eprintln!("=== Loop Studio bootstrap complete: all green ==="),
        BootstrapVerdict::Warnings => {
            eprintln!("=== Loop Studio bootstrap complete with {} warning(s) (see above) ===", warns);
            std::process::exit(2);
        }
        BootstrapVerdict::Failed => {
            eprintln!(
                "=== Loop Studio bootstrap finished with {} failure(s) and {} warning(s) — see above ===",
                failures, warns
            );
            std::process::exit(1);
        }
    }
}

/// The exit status a bootstrap run reports, decided once.
///
/// Split out because "warn-only is still a nonzero exit" is a rule about what
/// a *caller* can conclude, and it is a rule that decays: it held for the
/// `failures` arm, whose comment said so, and quietly did not hold for the
/// `warns` arm nine steps above it.
fn bootstrap_verdict(failures: u32, warns: u32) -> BootstrapVerdict {
    match (failures, warns) {
        (0, 0) => BootstrapVerdict::Green,
        (0, _) => BootstrapVerdict::Warnings,
        _ => BootstrapVerdict::Failed,
    }
}

#[derive(Debug, PartialEq)]
enum BootstrapVerdict {
    Green,
    Warnings,
    Failed,
}

#[cfg(test)]
mod tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;

    /// A run that only warned is not a configured machine.
    ///
    /// Nine of bootstrap's ten steps can only ever increment `warns` — Homebrew,
    /// rustc, uv, mise, the model dirs, the ollama symlink, the Time Machine
    /// exclusion — and the `failures == 0` arm fell off the end of `main` and
    /// exited 0. So on a fresh Mac with no `brew`, `loop-studio bootstrap` printed
    /// "complete with 7 warning(s)" and returned success, and `make bootstrap`
    /// could not see it. The file's own comment, three lines below that arm,
    /// said a zero would "carry a half-configured machine forward as a
    /// configured one" — which is why the rule held for `failures` and decayed
    /// for `warns`.
    ///
    /// Distinct nonzero codes so a caller can tell "not configured" from
    /// "broken" without parsing stdout, which is the whole point of having an
    /// exit code.
    #[test]
    fn a_warning_only_bootstrap_is_not_a_configured_machine() {
        assert_eq!(bootstrap_verdict(0, 0), BootstrapVerdict::Green);
        assert_ne!(bootstrap_verdict(0, 1), BootstrapVerdict::Green);
        assert_ne!(bootstrap_verdict(0, 9), BootstrapVerdict::Green);
        assert_eq!(bootstrap_verdict(0, 7), BootstrapVerdict::Warnings);
        // A failure outranks a warning, and is a different code.
        assert_eq!(bootstrap_verdict(1, 0), BootstrapVerdict::Failed);
        assert_eq!(bootstrap_verdict(3, 2), BootstrapVerdict::Failed);
    }

    fn temp_zprofile(tag: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "loop-studio-test-zprofile-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        path.to_string_lossy().into_owned()
    }

    /// The bug this pins: the marker check was one-shot, so the first run
    /// decided where HF_HOME pointed and no later run could change it —
    /// including the run where /Volumes/AIModels finished mounting. Bootstrap
    /// then wrote an internal-disk path into the shell while every other step
    /// in the same run used the external volume.
    #[test]
    fn zprofile_block_is_reconciled_when_the_model_library_moves() {
        let zp = temp_zprofile("reconcile");

        // First run: no block at all.
        assert_eq!(
            ensure_zprofile_in(&zp, "/Volumes/AIModels"),
            ZprofileOutcome::Installed
        );
        let after_first = fs::read_to_string(&zp).expect("read");
        assert!(after_first.contains("HF_HOME=\"/Volumes/AIModels/hf\""), "{after_first}");

        // Second run, same target: nothing to do. Idempotence is the whole
        // point of a marker, and it has to survive the reconcile path.
        assert_eq!(
            ensure_zprofile_in(&zp, "/Volumes/AIModels"),
            ZprofileOutcome::Unchanged
        );
        assert_eq!(fs::read_to_string(&zp).expect("read"), after_first);

        // The volume was not mounted on the first run. Now it is. The block
        // is present but records the wrong library, so this is a reconcile —
        // which is exactly the transition the one-shot marker skipped.
        assert_eq!(
            ensure_zprofile_in(&zp, "/home/u/.lac/models"),
            ZprofileOutcome::Reconciled
        );
        let internal = fs::read_to_string(&zp).expect("read");
        assert!(internal.contains("HF_HOME=\"/home/u/.lac/models/hf\""), "{internal}");

        // ...and the reverse transition, which is worse: an HF_HOME pointing
        // into a volume that is no longer mounted.
        assert_eq!(
            ensure_zprofile_in(&zp, "/Volumes/AIModels"),
            ZprofileOutcome::Reconciled
        );
        let back = fs::read_to_string(&zp).expect("read");
        assert!(back.contains("HF_HOME=\"/Volumes/AIModels/hf\""), "{back}");
        assert!(!back.contains("/home/u/.lac/models"), "stale path survived: {back}");
        // Exactly one managed block, never a second appended copy.
        assert_eq!(back.matches(ZPROFILE_MARKER).count(), 1, "{back}");
        assert_eq!(back.matches(ZPROFILE_END).count(), 1, "{back}");

        let _ = fs::remove_file(&zp);
    }

    #[test]
    fn zprofile_reconcile_leaves_the_users_own_lines_alone() {
        let zp = temp_zprofile("preserve");
        fs::write(
            &zp,
            "# my shell\nexport EDITOR=vim\nalias ll='ls -l'\n",
        )
        .expect("seed");

        assert_eq!(
            ensure_zprofile_in(&zp, "/Volumes/AIModels"),
            ZprofileOutcome::Installed
        );
        let after = fs::read_to_string(&zp).expect("read");
        for line in ["# my shell", "export EDITOR=vim", "alias ll='ls -l'"] {
            assert!(after.contains(line), "dropped {line}: {after}");
        }

        // Now change the target. Only the managed region may change.
        assert_eq!(
            ensure_zprofile_in(&zp, "/home/u/.lac/models"),
            ZprofileOutcome::Reconciled
        );
        let after = fs::read_to_string(&zp).expect("read");
        for line in ["# my shell", "export EDITOR=vim", "alias ll='ls -l'"] {
            assert!(after.contains(line), "reconcile dropped {line}: {after}");
        }
        assert!(after.contains("HF_HOME=\"/home/u/.lac/models/hf\""), "{after}");
        assert!(!after.contains("/Volumes/AIModels"), "stale path survived: {after}");

        // A file with no trailing newline still gets a valid block.
        let zp2 = temp_zprofile("nonewline");
        fs::write(&zp2, "export FOO=1").expect("seed");
        assert_eq!(
            ensure_zprofile_in(&zp2, "/Volumes/AIModels"),
            ZprofileOutcome::Installed
        );
        let after = fs::read_to_string(&zp2).expect("read");
        assert!(after.starts_with("export FOO=1\n"), "{after}");
        assert!(after.contains(ZPROFILE_MARKER), "{after}");

        // An orphaned start marker is left visible rather than swallowed.
        let zp3 = temp_zprofile("orphan");
        fs::write(&zp3, format!("prelude\n{ZPROFILE_MARKER}\nhand-edited junk\n")).expect("seed");
        assert_eq!(
            ensure_zprofile_in(&zp3, "/Volumes/AIModels"),
            ZprofileOutcome::Installed
        );
        let after = fs::read_to_string(&zp3).expect("read");
        assert!(after.contains("hand-edited junk"), "orphan was consumed: {after}");
        assert!(after.contains("HF_HOME=\"/Volumes/AIModels/hf\""), "{after}");

        for path in [zp, zp2, zp3] {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn a_failed_zprofile_write_is_reported_not_assumed() {
        // A directory where the file should be: every write fails, and this
        // is the case that used to print a warning and then let the run
        // continue to "all green".
        let dir = std::env::temp_dir().join(format!("loop-studio-test-zpdir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        let blocked = dir.join(".zprofile");
        fs::create_dir_all(&blocked).expect("mkdir over the file path");

        match ensure_zprofile_in(&blocked.to_string_lossy(), "/Volumes/AIModels") {
            ZprofileOutcome::Failed(_) => {}
            other => panic!("expected Failed, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn template_install_reports_what_actually_happened() {
        let base = std::env::temp_dir().join(format!("loop-studio-test-tmpl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("nested")).expect("mkdir");
        let file = base.join("loop.yaml");
        fs::write(&file, "id: x\n").expect("write");
        let dest = base.join("out.yaml");
        let dest = dest.to_string_lossy().into_owned();

        // A real file is copied and says so.
        assert_eq!(install_template(&file, &dest), Ok(true));
        assert_eq!(fs::read_to_string(&dest).expect("read"), "id: x\n");

        // A directory is skipped, not attempted. `fs::copy` on a directory
        // always fails with EISDIR, and the old loop printed "installed loop"
        // for it anyway.
        assert_eq!(install_template(&base.join("nested"), &dest), Ok(false));
        // A missing source is an error the caller can count.
        assert!(install_template(&base.join("absent"), &dest).is_err());

        let _ = fs::remove_dir_all(&base);
    }
}
