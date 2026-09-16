//! Stamp the edge binary with what built it.
//!
//! A deployed runtime is a bare file on a box; nothing beside it records the
//! source or the compiler. When a bench binary misbehaves months later, "which
//! commit is this?" currently has no answer at all. Embed it so the artifact
//! answers for itself, wherever it ended up and however it got there.
//!
//! Anything that cannot be determined is reported as `unknown` rather than
//! guessed — a wrong provenance stamp is worse than an absent one.
use std::process::Command;

fn probe(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    // A build that cannot see a git tree (a source tarball, or a checkout
    // mounted into a container without its .git) gets `unknown` unless the
    // build system passes the commit in.
    for head in ["../../.git/HEAD", "../../.git"] {
        if std::path::Path::new(head).exists() {
            println!("cargo:rerun-if-changed={head}");
            break;
        }
    }
    println!("cargo:rerun-if-env-changed=IA2_BUILD_COMMIT");

    let commit = std::env::var("IA2_BUILD_COMMIT").ok().unwrap_or_else(|| {
        match probe("git", &["rev-parse", "--short=12", "HEAD"]) {
            Some(c) => {
                // Uncommitted changes mean the commit alone does not identify
                // these bytes. Say so instead of implying reproducibility.
                let dirty = probe("git", &["status", "--porcelain"]).is_some();
                if dirty {
                    format!("{c}-dirty")
                } else {
                    c
                }
            }
            None => "unknown".into(),
        }
    });
    println!("cargo:rustc-env=IA2_BUILD_COMMIT={commit}");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = probe(&rustc, &["-V"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=IA2_BUILD_RUSTC={version}");
}
