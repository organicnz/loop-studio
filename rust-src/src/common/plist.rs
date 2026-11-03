//! launchd plist retargeting and key-value mutation.

/// Retarget a launchd plist template at THIS machine: baked-in prior-home
/// paths become `$HOME`, `ProgramArguments[0]` becomes `want_bin`, the
/// working directory becomes this checkout, and log paths re-root into
/// this user's `~/Library/Logs` (filenames preserved). Keyed on plist
/// structure, not on one exact string, so templates stay portable
/// across users and checkouts.
pub fn retarget_launchd_plist(content: &str, home: &str, root: &str, want_bin: &str) -> String {
    let mut out = content.replace("/Users/organic", home);
    // Replace the first <string> entry (ProgramArguments[0]).
    if let Some(arr) = out.find("<array>") {
        if let Some(s0) = out[arr..].find("<string>") {
            let abs = arr + s0 + "<string>".len();
            if let Some(e0) = out[abs..].find("</string>") {
                out.replace_range(abs..abs + e0, want_bin);
            }
        }
    }
    // Pin WorkingDirectory to this checkout.
    if let Some(k) = out.find("<key>WorkingDirectory</key>") {
        if let Some(s0) = out[k..].find("<string>") {
            let abs = k + s0 + "<string>".len();
            if let Some(e0) = out[abs..].find("</string>") {
                out.replace_range(abs..abs + e0, root);
            }
        }
    }
    // Re-root log files into this user's Logs dir,
    // preserving each plist's filename.
    for key in ["<key>StandardOutPath</key>", "<key>StandardErrorPath</key>"] {
        if let Some(k) = out.find(key) {
            if let Some(s0) = out[k..].find("<string>") {
                let abs = k + s0 + "<string>".len();
                if let Some(e0) = out[abs..].find("</string>") {
                    let cur = out[abs..abs + e0].to_string();
                    let base = cur.rsplit('/').next().unwrap_or("loop-studio.log");
                    out.replace_range(abs..abs + e0, &format!("{}/Library/Logs/{}", home, base));
                }
            }
        }
    }
    out
}

/// Set the `<string>` value that follows `<key>KEY</key>` in a plist, whatever
/// the indentation. Returns `None` when the key is not there, so a caller can
/// tell "retargeted" from "silently did nothing".
///
/// This exists because the router port was being rewritten with a literal
/// `"<key>LOOP_STUDIO_ROUTER_PORT</key>\n    <string>8000</string>"`, and the shipped
/// template indents its `<string>` by eight spaces. The replace never matched,
/// `LOOP_STUDIO_ROUTER_PORT=9000 loop-studio daemon install` printed `[ok] Placed:`, and the
/// launchd router bound 8000 while every `loop-studio` command addressed 9000 — so the
/// whole CLI reported the gateway down. A plist is XML with no significant
/// whitespace; matching on it is how that stayed invisible.
pub fn set_plist_string(content: &str, key: &str, value: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let k = content.find(&needle)?;
    let after = k + needle.len();
    // The value must belong to *this* key, so the search stops at the next
    // one. A key whose value is not a string (`<true/>`, a nested `<dict>`) is
    // reported rather than answered with whatever `<string>` comes later in the
    // file.
    let end = content[after..]
        .find("<key>")
        .map(|i| after + i)
        .unwrap_or(content.len());
    let region = &content[after..end];
    let s0 = region.find("<string>")? + after;
    let open = s0 + "<string>".len();
    let close = content[open..end].find("</string>")? + open;
    let mut out = content.to_string();
    out.replace_range(open..close, value);
    Some(out)
}

#[cfg(test)]
mod tests {
    #![deny(dead_code)]
    use super::*;

    #[test]
    fn plist_retarget_ports_across_macs() {
        let tpl = r#"<?xml version="1.0"?>
<dict>
    <key>ProgramArguments</key>
    <array>
        <string>/Users/organic/dev/work/AI/loop-studio/rust-src/target/release/serve-mlx</string>
        <string>mlx-community/Qwen3.8-27B-4bit</string>
    </array>
    <key>WorkingDirectory</key>
    <string>/Users/organic/dev/work/AI/loop-studio</string>
    <key>StandardOutPath</key>
    <string>/Users/organic/Library/Logs/loop-studio-serve-mlx.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/organic/Library/Logs/loop-studio-serve-mlx.err</string>
</dict>"#;
        let out = retarget_launchd_plist(tpl, "/Users/alice", "/ws/loop-studio", "/Users/alice/.local/bin/loop-studio-serve-mlx");
        assert!(out.contains("/Users/alice/.local/bin/loop-studio-serve-mlx"), "binary retargeted");
        assert!(out.contains("<string>/ws/loop-studio</string>"), "workdir pinned");
        assert!(out.contains("/Users/alice/Library/Logs/loop-studio-serve-mlx.log"), "stdout re-rooted");
        assert!(out.contains("/Users/alice/Library/Logs/loop-studio-serve-mlx.err"), "stderr re-rooted");
        assert!(!out.contains("/Users/organic"), "no prior-home leakage");
        // Second model arg untouched (only ProgramArguments[0] replaced).
        assert!(out.contains("mlx-community/Qwen3.8-27B-4bit"), "model arg preserved");
    }

    #[test]
    fn a_plist_value_is_matched_by_key_not_by_its_whitespace() {
        // The shape of launchd/org.loopstudio.router.plist: the <string> under a
        // nested <dict> is indented by eight, not four.
        let router = "<key>EnvironmentVariables</key>\n    <dict>\n        <key>LOOP_STUDIO_ROUTER_PORT</key>\n        <string>8000</string>\n        <!-- note -->\n    </dict>";
        let out = set_plist_string(router, "LOOP_STUDIO_ROUTER_PORT", "9000")
            .expect("the key is present, so this must retarget");
        assert!(out.contains("<key>LOOP_STUDIO_ROUTER_PORT</key>"), "{out}");
        assert!(out.contains("<string>9000</string>"), "{out}");
        assert!(!out.contains("8000"), "{out}");
        // The literal-whitespace replace this replaced, for the record.
        assert!(router.replace(
            "<key>LOOP_STUDIO_ROUTER_PORT</key>\n    <string>8000</string>",
            "<key>LOOP_STUDIO_ROUTER_PORT</key>\n    <string>9000</string>"
        ) == router, "the old needle never matched the shipped template");
        // A key that is not there is reported, not silently ignored.
        assert!(set_plist_string(router, "LOOP_STUDIO_MISSING_KEY", "1").is_none());
        // A key with no string value is reported too, rather than rewriting
        // whatever `<string>` happens to come later in the file.
        let no_value = "<key>KeepAlive</key>\n    <true/>\n    <key>Other</key>\n    <string>keep</string>";
        assert!(set_plist_string(no_value, "KeepAlive", "false").is_none());
    }
}
