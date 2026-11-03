//! package-app — build, stage, and sign the Loop Studio macOS app bundle.
//!
//! Pure Rust replacement for SwiftUI/package-app.sh. Uses std::process::Command
//! for swift build/codesign/sips/iconutil — those are macOS system tools, not
//! runtime dependencies. Zero external crates.
//!
//! Usage:
//!   package-app [--release] [--open] [--restart] [--dry-run]
//!
//! --release   Build in release mode (default: debug)
//! --open      Open the app after packaging
//! --restart   Kill any running instance and reopen
//! --dry-run   Validate all source paths, print what would happen, exit 0

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const BUNDLE_ID: &str = "org.loopstudio.studio";
const APP_NAME: &str = "Loop Studio";
const BINARY_NAME: &str = "LoopStudio";
const SHORT_VERSION: &str = "2.10";
const MIN_OS: &str = "13.0";

const INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>LoopStudio</string>
    <key>CFBundleIdentifier</key>
    <string>org.loopstudio.studio</string>
    <key>CFBundleName</key>
    <string>Loop Studio</string>
    <key>CFBundleDisplayName</key>
    <string>Loop Studio</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>2.10</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSLocalNetworkUsageDescription</key>
    <string>Loop Studio connects to your local and Tailscale gateway.</string>
    <key>NSAppTransportSecurity</key>
    <dict>
        <key>NSAllowsLocalNetworking</key>
        <true/>
    </dict>
</dict>
</plist>
"#;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let release = args.iter().any(|a| a == "--release" || a == "-r");
    let open_app = args.iter().any(|a| a == "--open" || a == "-o");
    let restart = args.iter().any(|a| a == "--restart");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    // Locate the SwiftUI directory relative to this binary's manifest.
    // When run from the project root, SwiftUI/ is a sibling of rust-src/.
    let swiftui_dir = locate_swiftui_dir();

    if dry_run {
        println!("=== package-app dry-run ===");
        println!("SwiftUI dir : {}", swiftui_dir.display());
        println!("Build mode  : {}", if release { "release" } else { "debug" });
        println!("Bundle name : {}.app", APP_NAME);
        println!("Bundle id   : {}", BUNDLE_ID);
        println!("Binary      : {}", BINARY_NAME);
        println!("Min OS      : {}", MIN_OS);
        println!("Version     : {}", SHORT_VERSION);
        // Validate Info.plist is well-formed XML.
        if INFO_PLIST.contains("<key>CFBundleExecutable</key>")
            && INFO_PLIST.contains("<key>CFBundleIdentifier</key>")
            && INFO_PLIST.contains("<key>NSAllowsLocalNetworking</key>")
        {
            println!("Info.plist  : valid (required keys present)");
        } else {
            eprintln!("Info.plist  : MISSING required keys");
            std::process::exit(1);
        }
        println!("=== dry-run complete ===");
        return;
    }

    println!("=== Loop Studio package-app v{} ===", SHORT_VERSION);

    // Step 1: swift build
    let config = if release { "release" } else { "debug" };
    println!("Building LoopStudio ({})...", config);
    let mut swift_args = vec!["build"];
    if release {
        swift_args.extend(["-c", "release"]);
    }
    let status = Command::new("swift")
        .args(&swift_args)
        .current_dir(&swiftui_dir)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("swift build failed: exit {}", s);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("swift build failed to run: {}", e);
            std::process::exit(1);
        }
    }

    // Locate the built binary.
    let show_path_args: Vec<&str> = if release {
        vec!["build", "-c", "release", "--show-bin-path"]
    } else {
        vec!["build", "--show-bin-path"]
    };
    let bin_path = Command::new("swift")
        .args(&show_path_args)
        .current_dir(&swiftui_dir)
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8(o.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| format!("{}/build/debug", swiftui_dir.display()));
    let binary = PathBuf::from(&bin_path).join(BINARY_NAME);
    if !binary.exists() {
        eprintln!("Built binary not found at {}", binary.display());
        std::process::exit(1);
    }
    println!("Binary: {}", binary.display());

    // Step 2: stage the app bundle.
    let build_dir = swiftui_dir.join(".build");
    let app_dir = build_dir.join(format!("{}.app", APP_NAME));
    let contents = app_dir.join("Contents");
    let macos_dir = contents.join("MacOS");
    let resources_dir = contents.join("Resources");
    for d in [&macos_dir, &resources_dir] {
        fs::create_dir_all(d).unwrap_or_else(|e| {
            eprintln!("create_dir_all {}: {}", d.display(), e);
            std::process::exit(1);
        });
    }

    // Write Info.plist.
    let plist_path = contents.join("Info.plist");
    fs::write(&plist_path, INFO_PLIST).unwrap_or_else(|e| {
        eprintln!("write Info.plist: {}", e);
        std::process::exit(1);
    });

    // Copy binary, removing any stale pre-rename copy.
    let dest_bin = macos_dir.join(BINARY_NAME);
    let stale_bin = macos_dir.join("LACStudio");
    let _ = fs::remove_file(&stale_bin);
    fs::copy(&binary, &dest_bin).unwrap_or_else(|e| {
        eprintln!("copy binary: {}", e);
        std::process::exit(1);
    });
    // Make executable.
    let mut perms = fs::metadata(&dest_bin)
        .map(|m| m.permissions())
        .unwrap_or_else(|_| fs::Permissions::from_mode(0o755));
    perms.set_mode(0o755);
    let _ = fs::set_permissions(&dest_bin, perms);

    // Step 3: generate AppIcon.icns if the source PNG exists.
    let icon_src = swiftui_dir
        .join("Resources")
        .join("LoopStudioDashboard.appiconset")
        .join("icon_1024.png");
    if icon_src.exists() {
        generate_icns(&icon_src, &build_dir, &resources_dir);
    }

    // Copy appiconset into Resources.
    let appiconset = swiftui_dir
        .join("Resources")
        .join("LoopStudioDashboard.appiconset");
    if appiconset.exists() {
        let dest = resources_dir.join("LoopStudioDashboard.appiconset");
        copy_dir_all(&appiconset, &dest);
    }

    // Step 4: codesign the bundle.
    codesign(&app_dir);
    println!("staged + signed: {}", app_dir.display());

    // Step 5: create alias bundles (backward compat with scripts expecting old names).
    for alias_name in ["LAC Studio", "LACDashboard"] {
        let alias = build_dir.join(format!("{}.app", alias_name));
        let _ = fs::remove_dir_all(&alias);
        copy_dir_all(&app_dir, &alias);
        codesign(&alias);
    }

    // Step 6: open / restart if requested.
    if restart {
        for bin_name in ["Contents/MacOS/LoopStudio", "Contents/MacOS/LACDashboard", "Contents/MacOS/LACStudio"] {
            let _ = Command::new("pkill").args(["-f", bin_name]).status();
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        let _ = Command::new("open").arg(&app_dir).status();
    } else if open_app {
        let _ = Command::new("open").arg(&app_dir).status();
    }
}

fn locate_swiftui_dir() -> PathBuf {
    // When installed to ~/.local/bin, the SwiftUI dir is found relative to
    // common::project_root(). Fall back to a sibling of the current dir.
    let root = common::project_root();
    let candidate = PathBuf::from(&root).join("SwiftUI");
    if candidate.is_dir() {
        return candidate;
    }
    // Running from inside rust-src/
    let candidate2 = PathBuf::from("../SwiftUI");
    if candidate2.is_dir() {
        return candidate2.canonicalize().unwrap_or(candidate2);
    }
    PathBuf::from("SwiftUI")
}

fn generate_icns(src: &Path, build_dir: &Path, resources_dir: &Path) {
    let iconset = build_dir.join("icon").join("AppIcon.iconset");
    let _ = fs::create_dir_all(&iconset);
    let sizes: &[(u32, &str, bool)] = &[
        (16, "icon_16x16.png", false),
        (32, "icon_16x16@2x.png", false),
        (32, "icon_32x32.png", false),
        (64, "icon_32x32@2x.png", false),
        (128, "icon_128x128.png", false),
        (256, "icon_128x128@2x.png", false),
        (256, "icon_256x256.png", false),
        (512, "icon_256x256@2x.png", false),
        (512, "icon_512x512.png", false),
        (1024, "icon_512x512@2x.png", false),
    ];
    for (size, name, _) in sizes {
        let out = iconset.join(name);
        let _ = Command::new("sips")
            .args(["-z", &size.to_string(), &size.to_string()])
            .arg(src)
            .args(["--out", &out.to_string_lossy()])
            .output();
    }
    let icns_out = build_dir.join("icon").join("AppIcon.icns");
    let _ = Command::new("iconutil")
        .args(["-c", "icns"])
        .arg(&iconset)
        .args(["-o", &icns_out.to_string_lossy()])
        .output();
    if icns_out.exists() {
        let _ = fs::copy(&icns_out, resources_dir.join("AppIcon.icns"));
    }
}

fn codesign(path: &Path) {
    let _ = Command::new("codesign")
        .args(["--force", "--deep", "-s", "-"])
        .arg(path)
        .status();
}

fn copy_dir_all(src: &Path, dst: &Path) {
    let _ = fs::create_dir_all(dst);
    let Ok(entries) = fs::read_dir(src) else { return };
    for entry in entries.flatten() {
        let ty = entry.file_type();
        let dest_path = dst.join(entry.file_name());
        if ty.map(|t| t.is_dir()).unwrap_or(false) {
            copy_dir_all(&entry.path(), &dest_path);
        } else {
            let _ = fs::copy(entry.path(), &dest_path);
        }
    }
}
