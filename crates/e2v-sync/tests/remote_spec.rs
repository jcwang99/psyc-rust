use e2v_store::{WebdavFlavor, WebdavRemoteConfig, WebdavVerifiedCapabilities};
use std::path::PathBuf;

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
