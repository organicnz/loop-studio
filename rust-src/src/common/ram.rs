//! RAM probing — total, free, thresholds, and serve tiers.

use std::process::Command;

/// OS page size via `sysctl hw.pagesize`; Apple Silicon fallback 16384.
/// Never hardcode: Rosetta/Intel pages are 4096 and silently skew RAM math.
pub fn page_size() -> u64 {
    if let Ok(out) = Command::new("sysctl")
        .args(["-n", "hw.pagesize"])
        .output()
    {
        if let Ok(n) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
            if n > 0 {
                return n;
            }
        }
    }
    16384
}

/// Free unified memory in GiB.
///
/// Counts free + inactive + speculative pages. Speculative pages are
/// reclaimable on memory pressure; ignoring them (as before) understated
/// free RAM by several GiB and caused false OOM alarms.
pub fn free_ram_gib() -> Option<f64> {
    let out = Command::new("vm_stat").output().ok()?;
    let txt = String::from_utf8_lossy(&out.stdout);
    let ps = page_size() as f64;
    let mut pages: f64 = 0.0;
    for line in txt.lines() {
        let l = line.trim();
        if l.starts_with("Pages free:")
            || l.starts_with("Pages inactive:")
            || l.starts_with("Pages speculative:")
        {
            let n: String = l.chars().filter(|c| c.is_ascii_digit()).collect();
            pages += n.parse::<f64>().unwrap_or(0.0);
        }
    }
    if pages <= 0.0 {
        return None;
    }
    Some(pages * ps / 1073741824.0)
}

/// Total physical RAM in GiB via `sysctl hw.memsize`.
pub fn total_ram_gib() -> Option<f64> {
    let out = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    let n: f64 = String::from_utf8(out.stdout).ok()?.trim().parse().ok()?;
    if n <= 0.0 {
        return None;
    }
    Some(n / 1073741824.0)
}

/// RAM-pressure thresholds scaled by total physical RAM, so an 8GB
/// MacBook Air and a 96GB Studio get sane gates from the same code.
/// Returns (red_gib, yellow_gib): free < red is RED, < yellow is YELLOW.
pub fn mem_thresholds_gib() -> (f64, f64) {
    let total = total_ram_gib().unwrap_or(16.0);
    let red = (total * 0.04).clamp(1.5, 4.0);
    let yellow = (total * 0.12).clamp(3.0, 12.0);
    (red, yellow)
}

/// Smart-serve tiers scaled by total RAM: (high_gib, medium_gib).
/// Above high → quality lane; above medium → balanced; below → conserve.
/// On 96GB this is ~(38, 14), matching the old 40/16 constants.
pub fn serve_ram_tiers_gib() -> (f64, f64) {
    let total = total_ram_gib().unwrap_or(16.0);
    (total * 0.4, total * 0.15)
}

#[cfg(test)]
mod tests {
    #![deny(dead_code)]
    use super::*;

    #[test]
    fn page_size_plausible() {
        let ps = page_size();
        assert!(ps == 4096 || ps == 16384 || ps == 65536, "ps={}", ps);
    }

    #[test]
    fn mem_thresholds_ordered_and_clamped() {
        let (red, yellow) = mem_thresholds_gib();
        assert!(red > 0.0 && yellow > red, "red={} yellow={}", red, yellow);
        assert!((1.5..=4.0).contains(&red), "red={}", red);
        assert!((3.0..=12.0).contains(&yellow), "yellow={}", yellow);
    }

    #[test]
    fn serve_tiers_ordered() {
        let (high, medium) = serve_ram_tiers_gib();
        assert!(high > medium && medium > 0.0, "high={} medium={}", high, medium);
    }
}
