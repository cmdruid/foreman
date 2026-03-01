use std::fs;
use std::process::Command;

mod common;

#[test]
fn e2e_init_project_scaffold() {
    let temp = tempfile::tempdir().expect("temp dir");
    let target = temp.path().join("sample-project");
    let binary = common::binary_path("codex_foreman");

    let status = Command::new(&binary)
        .arg("--init-project")
        .arg(&target)
        .arg("--init-project-overwrite")
        .status()
        .expect("run init project");
    assert!(status.success());

    assert!(target.join("project.toml").exists());
    assert!(target.join("FOREMAN.md").exists());
    assert!(target.join("WORKER.md").exists());
    assert!(target.join("RUNBOOK.md").exists());
    assert!(target.join("HANDOFF.md").exists());
    assert!(!target.join("MANUAL.md").exists());
}

#[test]
fn e2e_init_project_scaffold_with_manual() {
    let temp = tempfile::tempdir().expect("temp dir");
    let target = temp.path().join("sample-project");
    let binary = common::binary_path("codex_foreman");

    let status = Command::new(&binary)
        .arg("--init-project")
        .arg(&target)
        .arg("--init-project-manual")
        .arg("--init-project-overwrite")
        .status()
        .expect("run init project");
    assert!(status.success());

    assert!(target.join("project.toml").exists());
    assert!(target.join("FOREMAN.md").exists());
    assert!(target.join("WORKER.md").exists());
    assert!(target.join("RUNBOOK.md").exists());
    assert!(target.join("HANDOFF.md").exists());
    assert!(target.join("MANUAL.md").exists());
    assert_eq!(
        std::fs::read_to_string(target.join("MANUAL.md"))
            .expect("read manual file")
            .trim_start()
            .lines()
            .next(),
        Some("# codex-foreman Manual")
    );
}

#[test]
fn e2e_init_project_overwrite_guard() {
    let temp = tempfile::tempdir().expect("temp dir");
    let target = temp.path().join("sample-project");
    let binary = common::binary_path("codex_foreman");

    std::fs::create_dir_all(&target).expect("create sample project folder");
    std::fs::write(target.join("FOREMAN.md"), "existing file")
        .expect("write pre-existing foreman file");

    let blocked = Command::new(&binary)
        .arg("--init-project")
        .arg(&target)
        .status()
        .expect("run init project");
    assert!(!blocked.success());

    let status = Command::new(&binary)
        .arg("--init-project")
        .arg(&target)
        .arg("--init-project-overwrite")
        .status()
        .expect("run init project with overwrite");
    assert!(status.success());

    assert_eq!(
        std::fs::read_to_string(target.join("FOREMAN.md"))
            .expect("read foreman file")
            .trim_start()
            .lines()
            .next(),
        Some("# Project Foreman")
    );
}

#[test]
fn e2e_service_config_validation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let binary = common::binary_path("codex_foreman");
    let config_path = temp.path().join("service.toml");
    fs::write(
        &config_path,
        r#"
[callbacks]
default_profile = "openclaw"

[callbacks.profiles.openclaw]
type = "webhook"
url = "https://example.local/webhook"
events = ["turn/completed"]
"#,
    )
    .expect("write service config");

    let status = Command::new(&binary)
        .arg("--service-config")
        .arg(&config_path)
        .arg("--validate-config")
        .status()
        .expect("validate config");
    assert!(status.success());
}

#[test]
fn e2e_service_config_fixtures() {
    let temp = tempfile::tempdir().expect("temp dir");
    let binary = common::binary_path("codex_foreman");

    let valid = temp.path().join("service-valid.toml");
    let invalid_profile = temp.path().join("service-invalid-default-profile.toml");
    let invalid_auth = temp.path().join("service-invalid-auth.toml");

    std::fs::copy(common::fixture_dir("service-valid.toml"), &valid).expect("copy valid fixture");
    std::fs::copy(
        common::fixture_dir("service-invalid-default-profile.toml"),
        &invalid_profile,
    )
    .expect("copy invalid profile fixture");
    std::fs::copy(
        common::fixture_dir("service-invalid-auth.toml"),
        &invalid_auth,
    )
    .expect("copy invalid auth fixture");

    let valid_status = Command::new(&binary)
        .arg("--service-config")
        .arg(&valid)
        .arg("--validate-config")
        .status()
        .expect("validate fixture config");
    assert!(valid_status.success());

    let invalid_profile_status = Command::new(&binary)
        .arg("--service-config")
        .arg(&invalid_profile)
        .arg("--validate-config")
        .status()
        .expect("validate invalid profile config");
    assert!(!invalid_profile_status.success());

    let invalid_auth_status = Command::new(&binary)
        .arg("--service-config")
        .arg(&invalid_auth)
        .arg("--validate-config")
        .status()
        .expect("validate invalid auth config");
    assert!(!invalid_auth_status.success());
}
