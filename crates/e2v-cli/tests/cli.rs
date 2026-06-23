use std::fs;

use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
use tempfile::tempdir;

fn init_repo(repo_root: &std::path::Path) {
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: repo_root.to_path_buf(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
}

#[test]
fn branch_commands_create_list_and_checkout() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "base").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "base".to_string(),
        })
        .unwrap();

    let create_output = e2v_cli::run_cli_for_test([
        "e2v",
        "branch",
        "--repo",
        repo_root.to_str().unwrap(),
        "create",
        "feature",
    ])
    .unwrap();
    assert!(create_output.contains("feature"));

    let list_output = e2v_cli::run_cli_for_test([
        "e2v",
        "branch",
        "--repo",
        repo_root.to_str().unwrap(),
        "list",
    ])
    .unwrap();
    assert!(list_output.contains("* main"));
    assert!(list_output.contains("feature"));

    let checkout_output = e2v_cli::run_cli_for_test([
        "e2v",
        "branch",
        "--repo",
        repo_root.to_str().unwrap(),
        "checkout",
        "feature",
    ])
    .unwrap();
    assert!(checkout_output.contains("feature"));

    let list_after = e2v_cli::run_cli_for_test([
        "e2v",
        "branch",
        "--repo",
        repo_root.to_str().unwrap(),
        "list",
    ])
    .unwrap();
    assert!(list_after.contains("* feature"));
}

#[test]
fn search_command_prints_filename_matches() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("alpha-notes.txt"), "alpha").unwrap();
    fs::write(repo_root.join("beta.txt"), "beta").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "search",
        "notes",
        "--repo",
        repo_root.to_str().unwrap(),
    ])
    .unwrap();

    assert!(output.contains("alpha-notes.txt"));
    assert!(!output.contains("beta.txt"));
}
