use e2v_gui::RepositoryRegistry;
use e2v_gui::services::{FsRepositoryRegistryStore, RealRepositoryService, RepositoryService};
use tempfile::tempdir;

#[test]
fn registry_round_trips_pinned_and_recent_entries_in_mru_order() {
    let temp = tempdir().unwrap();
    let store = FsRepositoryRegistryStore::new(temp.path().join("gui-state.json"));

    let mut registry = RepositoryRegistry::default();
    registry.touch_recent("D:/repos/alpha".into(), 300);
    registry.touch_recent("D:/repos/beta".into(), 400);
    registry.toggle_pin("D:/repos/alpha".into());
    store.save(&registry).unwrap();

    let loaded = store.load().unwrap();
    assert_eq!(
        loaded.recent[0].repo_root,
        std::path::PathBuf::from("D:/repos/beta")
    );
    assert_eq!(
        loaded.recent[1].repo_root,
        std::path::PathBuf::from("D:/repos/alpha")
    );
    assert_eq!(
        loaded.pinned,
        vec![std::path::PathBuf::from("D:/repos/alpha")]
    );
}

#[test]
fn real_service_loads_branch_head_and_default_remote_flags() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let service = RealRepositoryService::default();

    let card = service
        .init_repository(repo_root.clone(), "secret".into(), "main".into())
        .unwrap();

    assert_eq!(card.branch_name, "main");
    assert_eq!(card.repo_root, repo_root);
    assert!(!card.remote_configured);
}
