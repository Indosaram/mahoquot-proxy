#[cfg(test)]
mod tests {
    use mahoquot_gateway::plugins::store::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parse_store_registry() {
        let registry_json = r#"{
            "schema_version": 1,
            "plugins": [
                {
                    "id": "sample-plugin",
                    "name": "Sample Plugin",
                    "description": "A test plugin",
                    "author": "author",
                    "version": "1.0.0",
                    "install": {
                        "type": "direct",
                        "artifacts": [
                            {
                                "goos": "darwin",
                                "goarch": "arm64",
                                "url": "https://example.invalid/sample.dylib",
                                "sha256": "abcdef"
                            }
                        ]
                    }
                }
            ]
        }"#;

        let store = PluginStore::new(std::path::PathBuf::from("/tmp/plugins"), None);
        let parsed = store
            .parse_registry(registry_json.as_bytes())
            .expect("parse registry");
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.plugins.len(), 1);
        assert_eq!(parsed.plugins[0].id, "sample-plugin");
        assert_eq!(parsed.plugins[0].version, "1.0.0");
    }

    #[test]
    fn install_artifact_direct_with_checksum_and_delete() {
        let temp = TempDir::new("plugin-store-test");
        let store = PluginStore::new(temp.path().to_path_buf(), None);

        let content = b"binary-artifact-content-for-test";
        let mut hasher = Sha256::new();
        hasher.update(content);
        let sha256 = format!("{:x}", hasher.finalize());

        let installed_path = store
            .install_artifact_direct("test-artifact", "0.1.0", content, &sha256)
            .expect("install direct");

        assert!(installed_path.exists());
        let read_back = fs::read(&installed_path).expect("read installed file");
        assert_eq!(read_back, content);

        let wrong_sha = "0000000000000000000000000000000000000000000000000000000000000000";
        let err = store
            .install_artifact_direct("test-artifact-2", "0.1.0", content, wrong_sha)
            .unwrap_err();
        assert_eq!(err.code, "checksum_mismatch");

        let deleted = store.delete_plugin("test-artifact").expect("delete plugin");
        assert!(deleted);
        assert!(!installed_path.exists());
    }

    #[test]
    fn home_plugin_sync_with_local_fake() {
        let temp = TempDir::new("home-plugin-sync-test");
        let store = PluginStore::new(temp.path().to_path_buf(), None);

        let fake_payload = b"home-sync-artifact-payload";
        let mut hasher = Sha256::new();
        hasher.update(fake_payload);
        let sha256 = format!("{:x}", hasher.finalize());

        let (goos, goarch) = PluginStore::current_platform();

        let items = vec![mahoquot_gateway::plugins::store::PluginSyncItem {
            manifest: mahoquot_gateway::plugins::store::StoreManifest {
                schema_version: 1,
                id: "home-plugin-1".to_string(),
                name: "Home Plugin 1".to_string(),
                description: "desc".to_string(),
                author: "author".to_string(),
                version: "1.2.3".to_string(),
                release_tag: "v1.2.3".to_string(),
                repository: "".to_string(),
                logo: "".to_string(),
                homepage: "".to_string(),
                license: "".to_string(),
                tags: vec![],
                source_id: "official".to_string(),
                source_name: "Official".to_string(),
                source_url: "".to_string(),
                install: mahoquot_gateway::plugins::store::StoreInstallPlan {
                    install_type: "direct".to_string(),
                    artifacts: vec![mahoquot_gateway::plugins::store::StoreArtifact {
                        goos,
                        goarch,
                        url: "local://fake/home-plugin-1".to_string(),
                        sha256,
                        size: fake_payload.len() as i64,
                    }],
                },
            },
            auth: json!({}),
        }];

        let fake_fetcher = |url: &str| -> Result<Vec<u8>, String> {
            if url == "local://fake/home-plugin-1" {
                Ok(fake_payload.to_vec())
            } else {
                Err("not found in local fake".to_string())
            }
        };

        let report = store.execute_home_sync("node-node-1", &items, fake_fetcher);
        assert!(report.ok);
        assert_eq!(report.status, "success");
        assert_eq!(report.plugins.len(), 1);
        assert_eq!(report.plugins[0].id, "home-plugin-1");
        assert_eq!(report.plugins[0].install_status, "installed");

        let expected_path = std::path::PathBuf::from(&report.plugins[0].path);
        assert!(expected_path.exists());
    }
}
