//! Stamp the edge binary with what built it.
//!
//! A deployed runtime is a bare file on a box; nothing beside it records the
//! source or the compiler. When a bench binary misbehaves months later, "which
//! commit is this?" currently has no answer at all. Embed it so the artifact
//! answers for itself, wherever it ended up and however it got there.
//!
//! Anything that cannot be determined is reported as `unknown` rather than
//! guessed — a wrong provenance stamp is worse than an absent one.
//!
//! Residual, by construction: a build script re-runs only when something it
//! watches changes, so an edit that is never staged or committed leaves the
//! stamp reading the last commit this script saw. Read a locally built
//! stamp as "the commit this crate was last configured at". The deploy path
//! does not rely on that — it passes `IA2_BUILD_COMMIT` explicitly, and a
//! changed value always re-runs this script.
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
    //
    // Ask git where HEAD actually lives rather than assuming `../../.git` is
    // a directory. In a **worktree** it is a 74-byte pointer file whose
    // contents never change, so watching it froze the stamp at whatever the
    // first build saw: this binary reported a commit 20 revisions old, and
    // `-dirty` on a clean tree. A stamp that lies is worse than no stamp,
    // which is the whole reason this file exists. `--git-path` resolves to
    // the per-worktree HEAD (and to `.git/HEAD` in an ordinary clone), which
    // does change on every commit and checkout. Watching the index too means
    // staging refreshes the dirty flag.
    for path in ["HEAD", "index"] {
        if let Some(p) = probe("git", &["rev-parse", "--git-path", path]) {
            if std::path::Path::new(&p).exists() {
                println!("cargo:rerun-if-changed={p}");
            }
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
