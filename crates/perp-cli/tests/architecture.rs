//! Architecture tests: the layering rules, checked mechanically.
//!
//! The compiler already rejects an inward crate importing an outward one (the
//! dependencies simply are not declared). These tests pin the manifests, forbid
//! third-party crates and build scripts, check that the pure layers never touch
//! the outside world, and that no crate can contain `unsafe`.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::fs;
use std::path::{Path, PathBuf};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn crate_dir(name: &str) -> PathBuf {
    workspace().join("crates").join(name)
}

/// Names listed in `[dependencies]` of a manifest.
fn dependencies(name: &str) -> Vec<String> {
    let text = fs::read_to_string(crate_dir(name).join("Cargo.toml")).unwrap();
    let mut in_deps = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = line == "[dependencies]";
            continue;
        }
        if in_deps && !line.is_empty() && !line.starts_with('#') {
            out.push(line.split('=').next().unwrap().trim().to_string());
        }
    }
    out.sort();
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

fn sources(name: &str, sub: &str) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    rust_files(&crate_dir(name).join(sub), &mut files);
    assert!(!files.is_empty(), "no sources found for {name}/{sub}");
    files
        .into_iter()
        .map(|p| {
            let t = fs::read_to_string(&p).unwrap();
            (p, t)
        })
        .collect()
}

/// Code lines only: comment lines are skipped.
fn code_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
}

#[test]
fn dependency_direction_is_domain_app_infra_cli() {
    assert_eq!(
        dependencies("perp-domain"),
        Vec::<String>::new(),
        "the domain depends on nothing"
    );
    assert_eq!(dependencies("perp-app"), vec!["perp-domain"]);
    assert_eq!(dependencies("perp-infra"), vec!["perp-app", "perp-domain"]);
    assert_eq!(
        dependencies("perp-cli"),
        vec!["perp-app", "perp-domain", "perp-infra"]
    );
}

#[test]
fn no_third_party_crates_build_scripts_or_proc_macros() {
    let lock = fs::read_to_string(workspace().join("Cargo.lock")).unwrap();
    let packages = lock.matches("[[package]]").count();
    assert_eq!(
        packages, 4,
        "Cargo.lock must list exactly the four workspace crates"
    );
    assert!(!lock.contains("source ="), "no registry or git sources");
    for c in ["perp-domain", "perp-app", "perp-infra", "perp-cli"] {
        assert!(
            !crate_dir(c).join("build.rs").exists(),
            "{c} has a build script"
        );
        let manifest = fs::read_to_string(crate_dir(c).join("Cargo.toml")).unwrap();
        assert!(!manifest.contains("[build-dependencies]"), "{c}");
        assert!(!manifest.contains("proc-macro"), "{c}");
        assert!(
            !manifest.contains("[dev-dependencies]"),
            "{c}: tests use std only"
        );
    }
}

#[test]
fn every_crate_forbids_unsafe_code_and_none_uses_it() {
    for c in ["perp-domain", "perp-app", "perp-infra", "perp-cli"] {
        let text = fs::read_to_string(crate_dir(c).join("src").join("lib.rs")).unwrap();
        assert!(
            text.contains("#![forbid(unsafe_code)]"),
            "{c} must forbid unsafe code"
        );
        for sub in ["src", "tests"] {
            let dir = crate_dir(c).join(sub);
            if !dir.exists() {
                continue;
            }
            for (path, text) in sources(c, sub) {
                for (n, line) in code_lines(&text) {
                    let l = line.trim();
                    let bad = l.contains("unsafe {")
                        || l.contains("unsafe fn")
                        || l.contains("unsafe impl")
                        || l.contains("unsafe trait");
                    assert!(
                        !bad || l.contains('"'),
                        "{}:{} uses unsafe",
                        path.display(),
                        n + 1
                    );
                }
            }
        }
    }
    let manifest = fs::read_to_string(workspace().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("unsafe_code = \"forbid\""));
}

#[test]
fn the_domain_layer_is_pure() {
    // No files, sockets, processes, environment, threads, clock or the app/infra crates.
    let banned = [
        "std::fs",
        "std::net",
        "std::process",
        "std::env",
        "std::thread",
        "std::io::stdin",
        "std::io::stdout",
        "SystemTime",
        "Instant",
        "perp_app",
        "perp_infra",
        "perp_cli",
        "println!",
        "eprintln!",
    ];
    for sub in ["src", "tests"] {
        for (path, text) in sources("perp-domain", sub) {
            for (n, line) in code_lines(&text) {
                for b in banned {
                    // tests may print progress with eprintln!
                    if sub == "tests" && b.ends_with("println!") {
                        continue;
                    }
                    assert!(
                        !line.contains(b),
                        "{}:{} uses `{b}` in the domain layer",
                        path.display(),
                        n + 1
                    );
                }
            }
        }
    }
}

#[test]
fn the_application_layer_does_no_io_of_its_own() {
    let banned = [
        "std::fs",
        "std::net",
        "std::process",
        "std::env",
        "SystemTime",
        "Instant::now",
        "perp_infra",
        "perp_cli",
    ];
    for (path, text) in sources("perp-app", "src") {
        for (n, line) in code_lines(&text) {
            for b in banned {
                assert!(
                    !line.contains(b),
                    "{}:{} uses `{b}` in the application layer",
                    path.display(),
                    n + 1
                );
            }
        }
    }
}

#[test]
fn adapters_do_not_reach_into_the_cli() {
    for (path, text) in sources("perp-infra", "src") {
        for (n, line) in code_lines(&text) {
            assert!(!line.contains("perp_cli"), "{}:{}", path.display(), n + 1);
        }
    }
}
