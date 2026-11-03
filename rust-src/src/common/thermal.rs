//! Thermal state probing via `pmset -g therm`.

use std::process::Command;

/// The pmset invocation that reports thermal conditions affecting CPU speed.
///
/// `pmset -g thermals` does not exist. pmset answers it with
/// `Error: unhandled argument thermals` and exits 1 — on every host, always.
/// The probe used to run exactly that, discarded the exit status, and handed
/// every caller an empty string, which `thermal_state` then reported as
/// "Nominal". So the reading was invented everywhere, including on the
/// throttling machines the quality/context decisions exist to protect, and
/// the detail pane an operator checks to confirm the reading printed nothing.
pub(crate) const THERMAL_PROBE: [&str; 2] = ["-g", "therm"];

/// Raw `pmset -g therm` output. Empty when pmset could not be run or failed —
/// which is a host that cannot answer, not a machine reading Nominal.
pub fn thermal_detail() -> String {
    Command::new("pmset")
        .args(THERMAL_PROBE)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
}

/// One of Nominal | Fair | Serious | Critical — or `None` when the probe could
/// not answer.
///
/// `None` is a real answer and every caller has to handle it. `pmset` is not
/// present on every platform, it exits nonzero where it is unusable, and a
/// broken probe says nothing whatsoever about how hot the machine is. It used
/// to be flattened to "Nominal", and two decision paths read that as
/// headroom: `decide_model_and_context` (`lac_tui.rs`) picks Q4-vs-Q8 and the
/// context window from it, and `thermal_default_context` (`serve_llama.rs`)
/// sets the server's window. Later code collapsed "Serious" into "Nominal"
/// too, hiding the throttle warning that precedes Critical by minutes; an
/// absent reading is the same failure one level up.
pub fn thermal_state() -> Option<String> {
    let out = Command::new("pmset").args(THERMAL_PROBE).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    parse_thermal(&text)
}

/// A reading as a word, for lines that print it. An absent probe is named
/// here rather than defaulted at each call site, so no reason line, banner or
/// prompt can quietly claim a level nobody measured.
pub fn thermal_label(state: &Option<String>) -> &str {
    state.as_deref().unwrap_or("unknown")
}

/// Classify a *successful* `pmset -g therm` capture.
///
/// Split out so the mapping is testable without pmset, and so the few lines
/// that decide what a throttling machine is told can be read in one place.
///
/// The speed limit is the real signal: pmset reports `CPU_Speed_Limit = <n>`
/// only while a thermal condition is limiting the CPU, and omits the line
/// entirely when none is in effect. The bucket edges below are this project's
/// policy, not Apple's — what matters is that a reported limit is read as a
/// limit instead of being discarded, which is exactly what the invalid flag
/// did.
pub fn parse_thermal(stdout: &str) -> Option<String> {
    let out = stdout.trim();
    // pmset succeeded and said nothing at all. Silence is not Nominal.
    if out.is_empty() {
        return None;
    }
    // A named level, where the platform reports one.
    for (needle, level) in [
        ("Critical", "Critical"),
        ("Overheat", "Critical"),
        ("Serious", "Serious"),
        ("Fair", "Fair"),
    ] {
        if out.contains(needle) {
            return Some(level.to_string());
        }
    }
    if let Some(limit) = cpu_speed_limit(out) {
        return Some(
            match limit as u32 {
                0..=74 => "Critical",
                75..=89 => "Serious",
                90..=99 => "Fair",
                _ => "Nominal",
            }
            .to_string(),
        );
    }
    // pmset ran, succeeded, and reported no limit in effect. That is a
    // reading, not an absence: "No thermal warning level has been recorded" is
    // what a machine that is not currently throttling says.
    Some("Nominal".to_string())
}

/// The `CPU_Speed_Limit = n` value from a pmset thermal capture, if present.
pub fn cpu_speed_limit(out: &str) -> Option<f64> {
    let p = out.find("CPU_Speed_Limit")?;
    let tail = &out[p + "CPU_Speed_Limit".len()..];
    let eq = tail.find('=')?;
    tail[eq + 1..]
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

#[cfg(test)]
mod thermal_probe_tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;

    /// The pmset invocation itself. `pmset -g thermals` is not a pmset
    /// argument: it answers `Error: unhandled argument thermals` and exits 1
    /// on every host, always. The probe ran that, threw the exit status away,
    /// and every caller received an empty string — which `thermal_state`
    /// reported as "Nominal", the one value that means "the machine is cool
    /// and you may spend headroom". So the TUI's rules engine chose Q8 and a
    /// 16k window, `serve_llama` defaulted to 16k, and `/loop-studio/status` published
    /// a level, on a machine that was actively throttling. `thermal_detail()`
    /// printed blank, so the operator's own check of the reading showed
    /// nothing.
    #[test]
    fn the_probe_uses_an_argument_pmset_accepts() {
        assert_eq!(
            THERMAL_PROBE,
            ["-g", "therm"],
            "pmset spells it `-g therm`; `-g thermals` exits 1 with a usage error"
        );
        // And the fact, checked against the tool rather than against this
        // file's belief about it. `thermals` must be the one that fails, or
        // this test is asserting the wrong direction.
        let rejected = Command::new("pmset").args(["-g", "thermals"]).output();
        if let Ok(o) = rejected {
            assert!(
                !o.status.success(),
                "pmset accepted -g thermals; if that has changed, re-check \
                 which spelling this project should use"
            );
        }
    }

    /// A failed probe is absent, not Nominal. This is the single assertion the
    /// whole fix turns on: every consumer of the reading (the TUI's
    /// `decide_model_and_context`, `thermal_default_context` in serve_llama,
    /// the loop's preflight gate, the worker's thermal backoff) treats a
    /// level as a licence to spend, so an absent reading must be unable to
    /// look like a licence.
    #[test]
    fn a_probe_that_cannot_answer_yields_no_level() {
        assert_eq!(parse_thermal(""), None, "silence is not Nominal");
        assert_eq!(parse_thermal("   \n\t "), None);
        // A nonzero exit never reaches the parser — `thermal_state` checks the
        // status first — and that is where the old code lost the reading. It
        // captured the usage error, found no level word in it, and reported
        // "Nominal". The level-word scan is kept for platforms that *do* name a
        // level, and the usage text of a rejected invocation is not one.
        let rejected = Command::new("pmset").args(["-g", "thermals"]).output();
        if let Ok(o) = rejected {
            let captured = String::from_utf8_lossy(&o.stdout).into_owned();
            if !o.status.success() {
                // This is precisely the text the old probe fed to a function
                // whose every branch but the last said "Nominal".
                assert!(
                    parse_thermal(&captured)
                        .map(|l| l == "Nominal")
                        .unwrap_or(false),
                    "a rejected invocation has no level word, so the old code \
                     reported it as Nominal; this fixture is what it saw"
                );
            }
        }
    }

    /// What a *successful* pmset capture means, in the shapes it actually
    /// takes. On an idle Mac pmset reports no limit in effect, which is a
    /// reading — not an absence — so that case must be Nominal, or every
    /// healthy machine would print `unknown`.
    #[test]
    fn a_successful_capture_is_classified_not_guessed() {
        // Idle: no limit recorded. This is the real output of the probe.
        let idle = "Note: No thermal warning level has been recorded\n\
                    Note: No performance warning level has been recorded\n\
                    Note: No CPU power status has been recorded\n";
        assert_eq!(parse_thermal(idle).as_deref(), Some("Nominal"));

        // A throttling machine reports a CPU speed limit. The old code could
        // not see this at all: the flag was invalid, so the limit was never
        // read, and the answer was "Nominal" on exactly the machines that need
        // a smaller window.
        assert_eq!(
            parse_thermal("CPU_Speed_Limit = 71\n").as_deref(),
            Some("Critical")
        );
        assert_eq!(
            parse_thermal("CPU_Speed_Limit = 80\n").as_deref(),
            Some("Serious")
        );
        assert_eq!(
            parse_thermal("CPU_Speed_Limit = 95\n").as_deref(),
            Some("Fair")
        );
        assert_eq!(
            parse_thermal("CPU_Speed_Limit = 100\n").as_deref(),
            Some("Nominal")
        );

        // Named levels, where a platform reports one. `Overheat` maps to
        // Critical and `Serious` is *not* collapsed into Nominal — that
        // collapse hid the warning that precedes a throttle by minutes.
        assert_eq!(parse_thermal("Fair").as_deref(), Some("Fair"));
        assert_eq!(parse_thermal("Serious").as_deref(), Some("Serious"));
        assert_eq!(parse_thermal("Critical").as_deref(), Some("Critical"));
        assert_eq!(parse_thermal("Overheat").as_deref(), Some("Critical"));

        // A named level wins over a limit line, whichever order they appear.
        assert_eq!(
            parse_thermal("CPU_Speed_Limit = 100\nSerious\n").as_deref(),
            Some("Serious")
        );
    }

    /// The limit parser, on the values pmset can emit, so the classification
    /// above rests on a number rather than on a substring.
    #[test]
    fn a_speed_limit_is_read_as_a_number_not_a_word() {
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = 71"), Some(71.0));
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit=71"), Some(71.0));
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = 71\n"), Some(71.0));
        assert_eq!(cpu_speed_limit("x CPU_Speed_Limit = 71.5 y"), Some(71.5));
        // Absent, or not a limit at all.
        assert_eq!(
            cpu_speed_limit("No thermal warning level has been recorded"),
            None
        );
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = "), None);
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = fast"), None);
        // A negative or infinite "limit" is not a measurement; it must not
        // become a number the rules engine buckets.
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = -1"), None);
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = inf"), None);
    }

    /// Every level a decision can be made on is a level the router can publish
    /// and a parser can read back. `thermal_label` is the one place an absent
    /// reading is turned into a word, so it is asserted there rather than left
    /// to each call site.
    #[test]
    fn an_absent_reading_is_named_and_never_filled_in() {
        assert_eq!(thermal_label(&None), "unknown");
        for level in ["Nominal", "Fair", "Serious", "Critical"] {
            let reading = Some(level.to_string());
            assert_eq!(thermal_label(&reading), level);
        }
        // "unknown" is not any of the levels, so a `== "Nominal"` style test
        // downstream cannot accidentally pass on it.
        assert_ne!(thermal_label(&None), "Nominal");
    }
}
