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
            root: "/remote-root".to_string(),
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
