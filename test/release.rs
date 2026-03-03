use std::path::{Path, PathBuf};

#[test]
fn release_metadata_tracks_package_version() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest_path = manifest_dir.join("Cargo.toml");

    let manifest = std::fs::read_to_string(&manifest_path).unwrap_or_else(|err| {
        panic!(
            "failed to read Cargo.toml '{}': {err}",
            manifest_path.display()
        )
    });
    let manifest: toml::Value = toml::from_str(&manifest).unwrap_or_else(|err| {
        panic!(
            "failed to parse Cargo.toml '{}': {err}",
            manifest_path.display()
        )
    });
    let version = manifest
        .get("package")
        .and_then(|section| section.get("version"))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("missing package.version in '{}'", manifest_path.display()));

    let expected_release_section = format!("## v{version}");
    let changelog_path = manifest_dir.join("CHANGELOG.md");
    let release_guide_path = manifest_dir.join("RELEASE.md");

    assert!(
        changelog_path.exists(),
        "missing changelog file at '{}'",
        changelog_path.display()
    );
    assert!(
        release_guide_path.exists(),
        "missing release guide file at '{}'",
        release_guide_path.display()
    );

    let changelog = read_text(&changelog_path);
    let release_guide = read_text(&release_guide_path);

    assert!(
        changelog.contains(&expected_release_section),
        "changelog does not contain release section '{expected_release_section}'"
    );
    assert!(
        release_guide.contains("single source of truth"),
        "release guide is missing core release-process section"
    );
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read '{}': {err}", path.display()))
}
