//! Export the real deploy generator for an offline systemd experiment.

use super::*;
use sha2::{Digest, Sha256};

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
#[ignore = "explicit offline export: scripts/r11-systemd/fixture.py prepare"]
fn export_production_deploy_template() {
    let output = std::path::PathBuf::from(
        std::env::var_os("IA2_R11_EXPORT_DIR").expect("IA2_R11_EXPORT_DIR is required"),
    );
    assert!(output.is_absolute(), "export directory must be absolute");
    std::fs::create_dir(&output).expect("export directory must not already exist");
    let script = remote_deploy_script(
        "__R11_INSTALL_ROOT__",
        "r11-project",
        Some("r11-runtime"),
        None,
    );
    assert_eq!(script.matches("__R11_INSTALL_ROOT__").count(), 1);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = std::fs::read(root.join("crates/server/src/edges.rs")).unwrap();
    let service = std::fs::read(root.join("infra/ia2.service")).unwrap();
    let git = |args: &[&str]| {
        let result = std::process::Command::new("git")
            .current_dir(&root)
            .args(args)
            .output()
            .expect("git for fixture provenance");
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap().trim().to_string()
    };
    let metadata = serde_json::json!({
        "schema": 1,
        "source_commit": git(&["rev-parse", "HEAD"]),
        "source_dirty": !git(&["status", "--porcelain"]).is_empty(),
        "production_unit_alias": EDGE_UNIT,
        "install_placeholder": "__R11_INSTALL_ROOT__",
        "edges_rs_sha256": digest(&source),
        "service_sha256": digest(&service),
        "deploy_template_sha256": digest(script.as_bytes()),
        "evidence": "OFFLINE_EXPORT_ONLY",
        "r11_status": "V_PENDING_REAL_SYSTEMD",
    });
    std::fs::write(output.join("deploy.sh.in"), script).unwrap();
    std::fs::write(output.join("production.service"), service).unwrap();
    std::fs::write(
        output.join("source.json"),
        serde_json::to_string_pretty(&metadata).unwrap() + "\n",
    )
    .unwrap();
}
