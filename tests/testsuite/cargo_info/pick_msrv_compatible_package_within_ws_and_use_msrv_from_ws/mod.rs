use crate::prelude::*;
use cargo_test_support::{Project, compare::assert_ui, current_dir, file};

use super::init_registry_without_token;

#[cargo_test]
fn case() {
    init_registry_without_token();
    cargo_test_support::registry::Package::new("my-package", "0.1.0").publish();
    cargo_test_support::registry::Package::new("my-package", "0.2.0")
        .rust_version("1.0.0")
        .publish();
    cargo_test_support::registry::Package::new("my-package", "0.2.1")
        .rust_version("1.70.0")
        .publish();

    let project = Project::from_template(current_dir!().join("in"));
    let project_root = project.root();
    let cwd = &project_root.join("crate1");

    snapbox::cmd::Command::cargo_ui()
        .arg("info")
        .arg("my-package")
        .arg("--registry=dummy-registry")
        .current_dir(cwd)
        .assert()
        .success()
        .stdout_eq(file!["stdout.term.svg"])
        .stderr_eq(file!["stderr.term.svg"]);

    assert_ui().subset_matches(current_dir!().join("out"), &project_root);
}
