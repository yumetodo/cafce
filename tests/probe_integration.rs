/// probe サブコマンドの RustFS 統合テスト
///
/// このモジュールのテストは通常の `cargo test` 実行ではスキップされる（`#[ignore]` 指定）。
/// 実行するには、リポジトリルートの `docker-compose.yml` で RustFS を事前に起動しておく必要がある:
///
/// ```sh
/// docker compose up -d
/// cargo test probe_integration -- --ignored --nocapture
/// ```
#[cfg(test)]
mod probe_integration_tests {
    fn rustfs_env(bucket: &str) -> cafce::env::Env {
        cafce::env::Env::new_for_test_with_bucket(cafce::env::TestEnvParams {
            server_address: Some("localhost:9000".to_string()),
            access_key: Some("cafce-dev-access-key".to_string()),
            secret_key: Some("cafce-dev-secret-key".to_string()),
            insecure: true,
            region: None,
            bucket: bucket.to_string(),
            s3_prefix: None,
            s3_checksum: cafce::env::S3ChecksumMode::Auto,
        })
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
        fallback_keys: Vec<String>,
    ) -> cafce::setting::Setting {
        cafce::setting::Setting {
            project: project.to_string(),
            paths: vec![],
            key: serde_either::StringOrStruct::String(primary_key.to_string()),
            fallback_keys,
        }
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

    async fn put_object(client: &aws_sdk_s3::Client, bucket: &str, key: &str) {
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from_static(
                b"cafce-probe-test",
            ))
            .send()
            .await
            .unwrap_or_else(|e| panic!("put_object({bucket}/{key}) failed: {e:?}"));
    }

    /// primary key を put_object した場合 → probe は true を返す
    #[tokio::test]
    #[ignore]
    async fn test_probe_primary_hit() {
        // Arrange
        let bucket = unique_name("cafce-probe-test");
        let project = unique_name("proj");
        let primary_key = "cache-v1";
        let env = rustfs_env(&bucket);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, primary_key, vec![]);
        let object_key = format!("{project}/{primary_key}");

        with_bucket(&client, &bucket, || async {
            put_object(&client, &bucket, &object_key).await;

            // Act
            let result =
                cafce::probe::probe(&setting, &env, &client, std::path::Path::new(".")).await;

            // Assert
            assert!(result.unwrap(), "primary key が存在するので true のはず");
        })
        .await;
    }

    /// primary は無く fallback_keys の 1 つを put_object した場合 → probe は true を返す
    #[tokio::test]
    #[ignore]
    async fn test_probe_fallback_hit() {
        // Arrange
        let bucket = unique_name("cafce-probe-test");
        let project = unique_name("proj");
        let primary_key = "cache-feature-branch";
        let fallback_key = "cache-main";
        let env = rustfs_env(&bucket);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, primary_key, vec![fallback_key.to_string()]);
        let fallback_object_key = format!("{project}/{fallback_key}");

        with_bucket(&client, &bucket, || async {
            // primary は置かず fallback だけ置く
            put_object(&client, &bucket, &fallback_object_key).await;

            // Act
            let result =
                cafce::probe::probe(&setting, &env, &client, std::path::Path::new(".")).await;

            // Assert
            assert!(result.unwrap(), "fallback key が存在するので true のはず");
        })
        .await;
    }

    /// 何も put_object しない場合 → probe は false を返す
    #[tokio::test]
    #[ignore]
    async fn test_probe_all_miss() {
        // Arrange
        let bucket = unique_name("cafce-probe-test");
        let project = unique_name("proj");
        let primary_key = "cache-v1";
        let env = rustfs_env(&bucket);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(
            &project,
            primary_key,
            vec!["cache-main".to_string(), "cache-default".to_string()],
        );

        with_bucket(&client, &bucket, || async {
            // Act: 何も置かない
            let result =
                cafce::probe::probe(&setting, &env, &client, std::path::Path::new(".")).await;

            // Assert
            assert!(!result.unwrap(), "何も存在しないので false のはず");
        })
        .await;
    }
}
