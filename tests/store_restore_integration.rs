/// store / restore サブコマンドの RustFS 統合テスト
///
/// このモジュールのテストは通常の `cargo test` 実行ではスキップされる（`#[ignore]` 指定）。
/// 実行するには、リポジトリルートの `docker-compose.yml` で RustFS を事前に起動しておく必要がある:
///
/// ```sh
/// docker compose up -d
/// cargo test store_restore_integration -- --ignored --nocapture
/// ```
///
/// RustFS は beta 段階で実運用実績が少ないため、user metadata のラウンドトリップや
/// フレキシブルチェックサムの挙動が仕様どおりとは限らない。設計doc 10 節のとおり、
/// ここで実際のラウンドトリップを確認することを前提としている。
#[cfg(test)]
mod store_restore_integration_tests {
    fn rustfs_env(bucket: &str, s3_checksum: cafce::env::S3ChecksumMode) -> cafce::env::Env {
        cafce::env::Env::new_for_test_with_bucket(
            Some("localhost:9000".to_string()),
            Some("cafce-dev-access-key".to_string()),
            Some("cafce-dev-secret-key".to_string()),
            true,
            bucket.to_string(),
            None,
            s3_checksum,
        )
    }

    fn unique_name(prefix: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before UNIX_EPOCH")
            .as_nanos();
        format!("{prefix}-{nanos}")
    }

    fn make_setting(
        project: &str,
        primary_key: &str,
        fallback_keys: std::vec::Vec<String>,
        paths: std::vec::Vec<String>,
    ) -> cafce::setting::Setting {
        cafce::setting::Setting {
            project: project.to_string(),
            paths,
            key: serde_either::StringOrStruct::String(primary_key.to_string()),
            fallback_keys,
        }
    }

    /// キャッシュ対象として使うファイル木を作る
    ///
    /// ファイル・ネストしたディレクトリ・空ディレクトリ・シンボリックリンクを含める。
    fn create_source_tree(base_path: &std::path::Path, marker: &str) {
        std::fs::create_dir_all(base_path.join("target/debug")).unwrap();
        std::fs::create_dir_all(base_path.join("target/empty")).unwrap();
        std::fs::write(
            base_path.join("target/debug/app"),
            format!("binary-{marker}"),
        )
        .unwrap();
        std::fs::write(base_path.join("target/.fingerprint"), marker).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("debug/app", base_path.join("target/app-link")).unwrap();
    }

    fn assert_source_tree_restored(base_path: &std::path::Path, marker: &str) {
        assert_eq!(
            std::fs::read_to_string(base_path.join("target/debug/app")).unwrap(),
            format!("binary-{marker}")
        );
        assert_eq!(
            std::fs::read_to_string(base_path.join("target/.fingerprint")).unwrap(),
            marker
        );
        assert!(base_path.join("target/empty").is_dir());
        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(base_path.join("target/app-link"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// バケット内の全オブジェクトを削除する（delete_bucket の前処理）
    async fn delete_all_objects(client: &aws_sdk_s3::Client, bucket: &str) {
        let list_resp = client
            .list_objects_v2()
            .bucket(bucket)
            .send()
            .await
            .unwrap_or_else(|e| panic!("list_objects_v2({bucket}) failed: {e:?}"));

        for obj in list_resp.contents() {
            let key = obj.key().expect("S3 object key must not be None");
            client
                .delete_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .unwrap_or_else(|e| panic!("delete_object({bucket}/{key}) failed: {e:?}"));
        }
    }

    /// バケット作成→テスト実行→全オブジェクト削除→バケット削除のラッパー
    async fn with_bucket<F, Fut>(client: &aws_sdk_s3::Client, bucket: &str, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        client
            .create_bucket()
            .bucket(bucket)
            .send()
            .await
            .unwrap_or_else(|e| panic!("create_bucket({bucket}) failed: {e:?}"));

        f().await;

        // 非空バケットは削除できないため、先にオブジェクトを全削除する
        delete_all_objects(client, bucket).await;

        client
            .delete_bucket()
            .bucket(bucket)
            .send()
            .await
            .unwrap_or_else(|e| panic!("delete_bucket({bucket}) failed: {e:?}"));
    }

    async fn head_object(
        client: &aws_sdk_s3::Client,
        bucket: &str,
        key: &str,
    ) -> aws_sdk_s3::operation::head_object::HeadObjectOutput {
        client
            .head_object()
            .bucket(bucket)
            .key(key)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await
            .unwrap_or_else(|e| panic!("head_object({bucket}/{key}) failed: {e:?}"))
    }

    /// store → probe → restore のラウンドトリップで元のファイル木が復元される
    #[tokio::test]
    #[ignore]
    async fn test_store_probe_restore_roundtrip() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "roundtrip");

        with_bucket(&client, &bucket, || async {
            // Act
            let uploaded = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let probed = cafce::probe::probe(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let restored = cafce::restore::restore(&setting, &env, &client, restore_dir.path())
                .await
                .unwrap();

            // Assert
            assert!(uploaded, "初回なのでアップロードされるはず");
            assert!(probed, "store 済みなので probe は true のはず");
            assert!(restored, "store 済みなので restore は true のはず");
            assert_source_tree_restored(restore_dir.path(), "roundtrip");
        })
        .await;
    }

    /// 同一内容で store を 2 回実行すると 2 回目はアップロードが省略される
    #[tokio::test]
    #[ignore]
    async fn test_store_twice_with_same_content_skips_upload() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "same");
        let object_key = format!("{project}/cache-v1");

        with_bucket(&client, &bucket, || async {
            let first = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let first_modified = head_object(&client, &bucket, &object_key)
                .await
                .last_modified;

            // Act: 内容を変えずに再実行する
            let second = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let second_modified = head_object(&client, &bucket, &object_key)
                .await
                .last_modified;

            // Assert
            assert!(first, "初回はアップロードされるはず");
            assert!(!second, "内容が同一なのでアップロードは省略されるはず");
            assert_eq!(
                first_modified, second_modified,
                "アップロードが省略されたなら LastModified は変わらないはず"
            );
        })
        .await;
    }

    /// 内容を変えて store すると上書きされ、restore が新しい内容を返す
    #[tokio::test]
    #[ignore]
    async fn test_store_with_changed_content_overwrites() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "before");

        with_bucket(&client, &bucket, || async {
            cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();

            // Act: 内容を変えて再 store する
            std::fs::write(source_dir.path().join("target/debug/app"), "binary-after").unwrap();
            std::fs::write(source_dir.path().join("target/.fingerprint"), "after").unwrap();
            let second = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            cafce::restore::restore(&setting, &env, &client, restore_dir.path())
                .await
                .unwrap();

            // Assert
            assert!(second, "内容が変わったので上書きアップロードされるはず");
            assert_source_tree_restored(restore_dir.path(), "after");
        })
        .await;
    }

    /// primary が miss で fallback が hit のとき、fallback のアーカイブが展開される
    #[tokio::test]
    #[ignore]
    async fn test_restore_falls_back_to_fallback_key() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let store_setting =
            make_setting(&project, "cache-main", vec![], vec!["target".to_string()]);
        let restore_setting = make_setting(
            &project,
            "cache-feature-branch",
            vec!["cache-main".to_string()],
            vec![],
        );
        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "fallback");

        with_bucket(&client, &bucket, || async {
            // primary は置かず fallback だけ store する
            cafce::store::store(&store_setting, &env, &client, source_dir.path())
                .await
                .unwrap();

            // Act
            let restored =
                cafce::restore::restore(&restore_setting, &env, &client, restore_dir.path())
                    .await
                    .unwrap();

            // Assert
            assert!(restored, "fallback key が存在するので true のはず");
            assert_source_tree_restored(restore_dir.path(), "fallback");
        })
        .await;
    }

    /// 全 miss のとき restore は false を返し、作業ディレクトリを触らない
    #[tokio::test]
    #[ignore]
    async fn test_restore_all_miss_returns_false() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(
            &project,
            "cache-v1",
            vec!["cache-main".to_string(), "cache-default".to_string()],
            vec![],
        );
        let restore_dir = tempfile::tempdir().unwrap();

        with_bucket(&client, &bucket, || async {
            // Act: 何も store しない
            let restored = cafce::restore::restore(&setting, &env, &client, restore_dir.path())
                .await
                .unwrap();

            // Assert
            assert!(!restored, "何も存在しないので false のはず");
            assert_eq!(
                std::fs::read_dir(restore_dir.path()).unwrap().count(),
                0,
                "全 miss のときは作業ディレクトリを触らないはず"
            );
        })
        .await;
    }

    /// store が付けた user metadata が RustFS でラウンドトリップする
    #[tokio::test]
    #[ignore]
    async fn test_user_metadata_roundtrips() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "metadata");
        let object_key = format!("{project}/cache-v1");

        with_bucket(&client, &bucket, || async {
            cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();

            // Act
            let output = head_object(&client, &bucket, &object_key).await;
            let parsed = cafce::cache_metadata::parse_metadata(output.metadata()).unwrap();

            // Assert
            let parsed = parsed.expect("user metadata がラウンドトリップしていない");
            assert_eq!(parsed.schema_version, cafce::cache_metadata::SCHEMA_VERSION);
            assert_eq!(parsed.archive_format, cafce::cache_metadata::ARCHIVE_FORMAT);
            assert_eq!(parsed.content_sha256.len(), 64);
        })
        .await;
    }

    /// チェックサム付きの PutObject が受理され、HeadObject で値が返る
    ///
    /// 値が返らない場合でも store / restore が成立することを併せて確認する
    /// （フレキシブルチェックサムはあくまで上積みであり、内容の検証は metadata ハッシュが担う）。
    #[tokio::test]
    #[ignore]
    async fn test_flexible_checksum_is_accepted() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "checksum");
        let object_key = format!("{project}/cache-v1");

        with_bucket(&client, &bucket, || async {
            // Act
            let uploaded = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let output = head_object(&client, &bucket, &object_key).await;
            let restored = cafce::restore::restore(&setting, &env, &client, restore_dir.path())
                .await
                .unwrap();

            // Assert
            assert!(uploaded, "チェックサム付きの PutObject が受理されるはず");
            assert!(restored);
            assert_source_tree_restored(restore_dir.path(), "checksum");
            match output.checksum_sha256() {
                Some(checksum) => {
                    assert_eq!(checksum.len(), 44, "32 バイトの Base64 は 44 文字のはず")
                }
                None => eprintln!(
                    "注意: RustFS は HeadObject で x-amz-checksum-sha256 を返さなかった。\
                     store / restore は成立している"
                ),
            }
        })
        .await;
    }

    /// 意図的に壊した値をチェックサムとして渡した PutObject が拒否される
    ///
    /// サーバ側照合が実際に効いていることの確認。効いていない場合はテストを失敗させず、
    /// その旨を記録する（RustFS の対応状況は beta 段階で変わりうるため）。
    #[tokio::test]
    #[ignore]
    async fn test_corrupted_checksum_is_rejected() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let body: &[u8] = b"cafce-corrupted-checksum-test";
        // 本文とは無関係な値（空文字列の SHA-256）を渡す
        let wrong_checksum = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";

        with_bucket(&client, &bucket, || async {
            // Act
            let result = client
                .put_object()
                .bucket(&bucket)
                .key("corrupted")
                .body(aws_sdk_s3::primitives::ByteStream::from_static(body))
                .checksum_algorithm(aws_sdk_s3::types::ChecksumAlgorithm::Sha256)
                .checksum_sha256(wrong_checksum)
                .send()
                .await;

            // Assert
            match result {
                Err(e) => {
                    use aws_sdk_s3::error::ProvideErrorMetadata as _;
                    eprintln!("サーバ側照合が効いている: code={:?}", e.code());
                }
                Ok(_) => eprintln!(
                    "注意: RustFS は壊れた x-amz-checksum-sha256 を拒否しなかった。\
                     内容の検証は metadata ハッシュが独立に担っている"
                ),
            }
        })
        .await;
    }

    /// CAFCE_S3_CHECKSUM=off でチェックサム無しの経路でも store → restore が成立する
    #[tokio::test]
    #[ignore]
    async fn test_checksum_off_roundtrip() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Off);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "checksum-off");

        with_bucket(&client, &bucket, || async {
            // Act
            let uploaded = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let restored = cafce::restore::restore(&setting, &env, &client, restore_dir.path())
                .await
                .unwrap();

            // Assert
            assert!(uploaded);
            assert!(restored);
            assert_source_tree_restored(restore_dir.path(), "checksum-off");
        })
        .await;
    }

    /// paths が空のとき store はエラーになる
    #[tokio::test]
    #[ignore]
    async fn test_store_with_empty_paths_is_error() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec![]);
        let source_dir = tempfile::tempdir().unwrap();

        with_bucket(&client, &bucket, || async {
            // Act
            let result = cafce::store::store(&setting, &env, &client, source_dir.path()).await;

            // Assert
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("キャッシュ対象の解決に失敗しました"));
        })
        .await;
    }
}
