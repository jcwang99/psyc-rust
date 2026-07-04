use e2v_store::{
    RemoteBackend, S3RemoteConfig, WebdavFlavor, WebdavRemoteConfig, WebdavVerifiedCapabilities,
    WriterMode,
};
use std::path::PathBuf;

#[test]
fn store_test_probes_are_not_exposed_as_public_api_methods() {
    let local_source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("e2v-store")
            .join("src")
            .join("local_backend.rs"),
    )
    .unwrap();
    let memory_source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("e2v-store")
            .join("src")
            .join("memory_backend.rs"),
    )
    .unwrap();

    assert!(
        !local_source.contains("pub fn override_physical_modified_time_for_test"),
        "local backend test-only modified-time override should not remain public"
    );
    assert!(
        !memory_source.contains("pub fn override_physical_modified_time_for_test"),
        "memory backend test-only modified-time override should not remain public"
    );
}

#[test]
fn store_root_uses_single_reexport_surface_instead_of_public_module_duplicates() {
    let root_source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("e2v-store")
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();

    for redundant_module in [
        "pub mod capability;",
        "pub mod layout;",
        "pub mod layout_root_store;",
        "pub mod local_backend;",
        "pub mod logical_object_store;",
        "pub mod memory_backend;",
        "pub mod opendal_backend;",
        "pub mod ref_store;",
        "pub mod storage_layout;",
    ] {
        assert!(
            !root_source.contains(redundant_module),
            "e2v-store should expose a single canonical root surface instead of duplicate public module path {redundant_module}"
        );
    }
}

#[test]
fn store_read_paths_do_not_probe_ref_or_layout_root_existence_before_loading() {
    let store_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("e2v-store")
        .join("src");
    let local_source = std::fs::read_to_string(store_src.join("local_backend.rs")).unwrap();
    let memory_source = std::fs::read_to_string(store_src.join("memory_backend.rs")).unwrap();
    let opendal_source = std::fs::read_to_string(store_src.join("opendal_backend.rs")).unwrap();

    for needless_probe in [
        "if !self.exists_physical(&path) {",
        "if !self.exists_physical(\"layout_root.json\") {",
        "if self.exists_physical(\"layout_root.json\") {",
    ] {
        assert!(
            !local_source.contains(needless_probe),
            "local backend should not probe ref/layout existence before loading it: {needless_probe}"
        );
        assert!(
            !memory_source.contains(needless_probe),
            "memory backend should not probe ref/layout existence before loading it: {needless_probe}"
        );
        assert!(
            !opendal_source.contains(needless_probe),
            "opendal backend should not probe ref/layout existence before loading it: {needless_probe}"
        );
    }
}

#[test]
fn store_delete_paths_do_not_probe_existence_before_removal() {
    let store_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("e2v-store")
        .join("src");
    let local_source = std::fs::read_to_string(store_src.join("local_backend.rs")).unwrap();
    let opendal_source = std::fs::read_to_string(store_src.join("opendal_backend.rs")).unwrap();

    for needless_probe in [
        "if !full_path.exists() {",
        "if self.exists_physical(relative_path) {",
    ] {
        assert!(
            !local_source.contains(needless_probe),
            "local backend delete path should not probe existence before removal: {needless_probe}"
        );
        assert!(
            !opendal_source.contains(needless_probe),
            "opendal backend delete path should not probe existence before removal: {needless_probe}"
        );
    }
}

#[test]
fn local_store_range_reads_do_not_materialize_the_full_file_first() {
    let local_source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("e2v-store")
            .join("src")
            .join("local_backend.rs"),
    )
    .unwrap();

    assert!(
        !local_source.contains("let bytes = self.get_object(relative_path)?;"),
        "local backend range reads should stream from the file instead of materializing the full physical object first"
    );
}

#[test]
fn store_ref_and_layout_root_records_do_not_spend_bytes_on_pretty_json_whitespace() {
    let store_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("e2v-store")
        .join("src");

    for file_name in [
        "local_backend.rs",
        "memory_backend.rs",
        "opendal_backend.rs",
    ] {
        let source = std::fs::read_to_string(store_src.join(file_name)).unwrap();
        for pretty_write in [
            "serde_json::to_vec_pretty(&stored)",
            "serde_json::to_vec_pretty(&next)",
        ] {
            assert!(
                !source.contains(pretty_write),
                "store control-plane ref/layout writes should use compact JSON instead of pretty JSON in {file_name}: {pretty_write}"
            );
        }
    }
}

#[test]
fn opendal_store_runtime_initialization_does_not_expect_success() {
    let opendal_source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("e2v-store")
            .join("src")
            .join("opendal_backend.rs"),
    )
    .unwrap();

    assert!(
        !opendal_source.contains(".expect(\"failed to build opendal runtime\")"),
        "opendal backend should surface runtime initialization failures as errors instead of panicking"
    );
}

#[test]
fn parse_remote_spec_decodes_webdav_url_into_remote_config() {
    let spec = e2v_sync::RemoteSpec::parse("webdav+https://alice:secret@example.com/repo").unwrap();

    assert_eq!(
        spec,
        e2v_sync::RemoteSpec::Webdav(WebdavRemoteConfig {
            flavor: WebdavFlavor::Webdav,
            endpoint: "https://example.com".to_string(),
            root: "/repo".to_string(),
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
            token: None,
            disable_create_dir: false,
            verified_capabilities: WebdavVerifiedCapabilities::default(),
        })
    );
}

#[test]
fn parse_remote_spec_decodes_alist_token_url_into_remote_config() {
    let spec = e2v_sync::RemoteSpec::parse("alist+https://token@example.com/remote-root").unwrap();

    assert_eq!(
        spec,
        e2v_sync::RemoteSpec::Webdav(WebdavRemoteConfig {
            flavor: WebdavFlavor::Alist,
            endpoint: "https://example.com".to_string(),
            root: "/dav/remote-root".to_string(),
            username: None,
            password: None,
            token: Some("token".to_string()),
            disable_create_dir: false,
            verified_capabilities: WebdavVerifiedCapabilities::default(),
        })
    );
}

#[test]
fn parse_remote_spec_preserves_explicit_alist_dav_prefix() {
    let spec =
        e2v_sync::RemoteSpec::parse("alist+https://token@example.com/dav/remote-root").unwrap();

    assert_eq!(
        spec,
        e2v_sync::RemoteSpec::Webdav(WebdavRemoteConfig {
            flavor: WebdavFlavor::Alist,
            endpoint: "https://example.com".to_string(),
            root: "/dav/remote-root".to_string(),
            username: None,
            password: None,
            token: Some("token".to_string()),
            disable_create_dir: false,
            verified_capabilities: WebdavVerifiedCapabilities::default(),
        })
    );
}

#[test]
fn parse_remote_spec_rejects_unsupported_scheme() {
    let error = e2v_sync::RemoteSpec::parse("ftp://example.com/repo").unwrap_err();

    assert!(error.to_string().contains("unsupported"));
}

#[test]
fn parse_remote_spec_decodes_file_url_into_local_remote() {
    #[cfg(windows)]
    let (raw, expected) = (
        "file:///C:/tmp/e2v-remote",
        PathBuf::from(r"C:\tmp\e2v-remote"),
    );
    #[cfg(not(windows))]
    let (raw, expected) = ("file:///tmp/e2v-remote", PathBuf::from("/tmp/e2v-remote"));

    let spec = e2v_sync::RemoteSpec::parse(raw).unwrap();

    assert_eq!(spec, e2v_sync::RemoteSpec::LocalFolder(expected));
}

#[test]
fn parse_remote_spec_decodes_s3_url_into_remote_config() {
    let spec = e2v_sync::RemoteSpec::parse(
        "s3+https://alice:secret@s3.example.com/example-bucket/sync-root?region=us-east-1",
    )
    .unwrap();

    assert_eq!(
        spec,
        e2v_sync::RemoteSpec::S3(S3RemoteConfig {
            endpoint: "https://s3.example.com".to_string(),
            bucket: "example-bucket".to_string(),
            root: "/sync-root".to_string(),
            region: Some("us-east-1".to_string()),
            access_key_id: Some("alice".to_string()),
            secret_access_key: Some("secret".to_string()),
            session_token: None,
            disable_config_load: true,
        })
    );
}

#[test]
fn s3_remote_spec_can_construct_a_remote_backend_boundary() {
    let spec = e2v_sync::RemoteSpec::parse(
        "s3+https://alice:secret@s3.example.com/example-bucket/sync-root?region=us-east-1",
    )
    .unwrap();

    let kind = spec
        .with_backend(|remote| match remote {
            e2v_sync::RemoteBackendRef::S3(_) => Ok("s3"),
            e2v_sync::RemoteBackendRef::LocalFolder(_) => Ok("local"),
            e2v_sync::RemoteBackendRef::Webdav(_) => Ok("webdav"),
        })
        .unwrap();

    assert_eq!(kind, "s3");
}

#[test]
fn s3_remote_spec_exposes_safe_single_writer_push_capability() {
    let spec = e2v_sync::RemoteSpec::parse(
        "s3+https://alice:secret@s3.example.com/example-bucket/sync-root?region=us-east-1",
    )
    .unwrap();

    let writer_mode = spec
        .with_backend(|remote| match remote {
            e2v_sync::RemoteBackendRef::S3(remote) => Ok(remote.capability().push_write_mode()),
            _ => unreachable!("expected s3 remote backend"),
        })
        .unwrap();

    assert_eq!(writer_mode, WriterMode::SingleWriter);
}

#[test]
fn s3_remote_spec_constructs_backend_with_safe_single_writer_capabilities_without_layout_root_cas()
{
    let spec = e2v_sync::RemoteSpec::parse(
        "s3+https://alice:secret@s3.example.com/example-bucket/sync-root?region=us-east-1",
    )
    .unwrap();

    let capability = spec
        .with_backend(|remote| match remote {
            e2v_sync::RemoteBackendRef::S3(remote) => Ok(remote.capability().clone()),
            _ => unreachable!("expected s3 remote backend"),
        })
        .unwrap();

    assert!(capability.supports_remote_lock_or_lease);
    assert!(capability.supports_transaction_markers);
    assert!(capability.supports_reliable_remote_time);
    assert!(capability.supports_object_generation_or_etag);
    assert!(!capability.supports_layout_root_cas);
}
