// src/s3_client.rs
use aws_sdk_s3::config::{Builder, Credentials, Region};
use crate::env::Env;

/// S3設定ビルダーに環境変数に基づく設定（エンドポイント、Path-style）を適用する
///
/// この関数は純粋なロジックであり、ネットワーク通信を行わない。
/// 認証情報の設定は呼び出し元（build_s3_client）が担当する。
///
/// # Arguments
/// * `builder` - ベースのS3設定ビルダー（認証情報設定済み）
/// * `env` - 環境変数設定
///
/// # Returns
/// * `Ok(Builder)` - 設定適用後のビルダー
/// * `Err` - エンドポイントURL生成に失敗した場合
pub fn apply_s3_config(
    mut builder: Builder,
    env: &Env,
) -> Result<Builder, crate::env::EndpointError> {
    // エンドポイント設定（MinIO等の場合）
    if let Some(endpoint) = env.build_endpoint()? {
        builder = builder.endpoint_url(endpoint.to_string());
    }

    // Path-style設定
    builder = builder.force_path_style(env.should_use_path_style());

    Ok(builder)
}

/// 環境変数設定に基づいてS3クライアントを構築する
///
/// - リージョンはenv.get_region()を使用してconfig loaderに設定する
/// - aws_access_key/aws_secret_keyが両方とも設定されている場合のみ、
///   静的クレデンシャルをcredentials_providerとして設定する
///   （どちらか一方でも未設定の場合はSDKデフォルトのcredential provider chainに委ねる）
/// - エンドポイント・Path-style設定はapply_s3_config()に委譲する
///
/// AssumeRole（aws_role_arn）、AWS Profile（aws_profile）、
/// SDK provider chainの明示的なロード処理は今回のスコープ外（未実装）。
pub async fn build_s3_client(env: &Env) -> Result<aws_sdk_s3::Client, crate::env::EndpointError> {
    let shared_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(Region::new(env.get_region()))
        .load()
        .await;

    let mut builder = Builder::from(&shared_config);

    if let (Some(access_key), Some(secret_key)) = (env.access_key(), env.secret_key()) {
        let credentials = Credentials::new(
            access_key,
            secret_key,
            env.session_token().map(String::from),
            None,
            "cafce-static",
        );
        builder = builder.credentials_provider(credentials);
    }

    builder = apply_s3_config(builder, env)?;

    Ok(aws_sdk_s3::Client::from_conf(builder.build()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Env;

    fn create_test_env(
        server_address: Option<&str>,
        insecure: bool,
        force_path_style: Option<bool>,
    ) -> Env {
        Env::new_for_test(
            server_address.map(String::from),
            None,
            None,
            None,
            None,
            None,
            None,
            insecure,
            None,
            force_path_style,
        )
    }

    #[test]
    fn test_apply_s3_config_minio() {
        let env = create_test_env(Some("localhost:9000"), true, None);
        let builder = aws_sdk_s3::config::Builder::new();

        let result = apply_s3_config(builder, &env);
        assert!(result.is_ok());
        // 設定が適用されたことを確認（ビルダーの戻り値がOkであればOK）
    }

    #[test]
    fn test_apply_s3_config_aws_s3() {
        let env = create_test_env(None, false, None);
        let builder = aws_sdk_s3::config::Builder::new();

        let result = apply_s3_config(builder, &env);
        assert!(result.is_ok());
        // AWS S3の場合はエンドポイント未設定、virtual-hosted style
    }

    #[test]
    fn test_apply_s3_config_path_style_forced() {
        let env = create_test_env(Some("s3.amazonaws.com"), false, Some(true));
        let builder = aws_sdk_s3::config::Builder::new();

        let result = apply_s3_config(builder, &env);
        assert!(result.is_ok());
        // Path-styleを明示的に強制した場合
    }

    #[test]
    fn test_apply_s3_config_virtual_hosted_forced() {
        let env = create_test_env(Some("localhost:9000"), true, Some(false));
        let builder = aws_sdk_s3::config::Builder::new();

        let result = apply_s3_config(builder, &env);
        assert!(result.is_ok());
        // Virtual-hosted styleを明示的に強制した場合
    }

    #[test]
    fn test_apply_s3_config_ipv6_endpoint() {
        let env = create_test_env(Some("[::1]:9000"), true, None);
        let builder = aws_sdk_s3::config::Builder::new();

        let result = apply_s3_config(builder, &env);
        assert!(result.is_ok());
        // IPv6アドレスのエンドポイント設定
    }

    #[test]
    fn test_apply_s3_config_ip_address_endpoint() {
        let env = create_test_env(Some("10.200.1.157:9000"), false, None);
        let builder = aws_sdk_s3::config::Builder::new();

        let result = apply_s3_config(builder, &env);
        assert!(result.is_ok());
        // IPアドレスのエンドポイント設定
    }
}

/// RustFS（ローカルS3互換サーバー）に対する疎通確認テスト。
///
/// このモジュールのテストは通常の`cargo test`実行ではスキップされる
/// （`#[ignore]`指定）。実行するには、リポジトリルートの`docker-compose.yml`で
/// RustFSを事前に起動しておく必要がある:
///
/// ```sh
/// docker compose up -d
/// cargo test rustfs_integration -- --ignored --nocapture
/// ```
///
/// `docker compose down`（またはコンテナ停止）した状態で実行すると、
/// エンドポイントへの接続に失敗してテストが失敗する。
#[cfg(test)]
mod rustfs_integration_tests {
    use super::*;
    use crate::env::Env;
    use aws_sdk_s3::primitives::ByteStream;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// テスト間で衝突しないよう、現在時刻（ナノ秒）でバケット名をユニーク化する。
    fn unique_bucket_name() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before UNIX_EPOCH")
            .as_nanos();
        format!("cafce-integration-test-{nanos}")
    }

    fn rustfs_test_env() -> Env {
        Env::new_for_test(
            Some("localhost:9000".to_string()),
            Some("cafce-dev-access-key".to_string()),
            Some("cafce-dev-secret-key".to_string()),
            None,
            None,
            None,
            None,
            true,
            None,
            None,
        )
    }

    /// docker-compose.ymlで起動したRustFSに対して、
    /// バケット作成→オブジェクトアップロード→一覧確認→取得→削除→バケット削除
    /// の一連の流れが正しく行えることを確認する。
    ///
    /// 実行方法:
    ///   docker compose up -d
    ///   cargo test rustfs_integration_smoke_test -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn rustfs_integration_smoke_test() {
        let env = rustfs_test_env();
        let client = build_s3_client(&env)
            .await
            .expect("failed to build S3 client for RustFS");

        let bucket = unique_bucket_name();
        let key = "cafce-test.txt";
        let content: &[u8] = b"hello from cafce";

        // 1. create_bucket
        client
            .create_bucket()
            .bucket(&bucket)
            .send()
            .await
            .unwrap_or_else(|e| panic!("create_bucket({bucket}) failed: {e:?}"));

        // 2. put_object
        client
            .put_object()
            .bucket(&bucket)
            .key(key)
            .body(ByteStream::from_static(content))
            .send()
            .await
            .unwrap_or_else(|e| panic!("put_object({bucket}/{key}) failed: {e:?}"));

        // 3. list_objects_v2でキーが含まれることを確認
        let list_resp = client
            .list_objects_v2()
            .bucket(&bucket)
            .send()
            .await
            .unwrap_or_else(|e| panic!("list_objects_v2({bucket}) failed: {e:?}"));
        let listed_keys: Vec<&str> = list_resp
            .contents()
            .iter()
            .filter_map(|obj| obj.key())
            .collect();
        assert!(
            listed_keys.contains(&key),
            "expected key {key:?} to be listed in bucket {bucket}, got {listed_keys:?}"
        );

        // 4. get_objectで取得し、中身が一致することを確認
        let get_resp = client
            .get_object()
            .bucket(&bucket)
            .key(key)
            .send()
            .await
            .unwrap_or_else(|e| panic!("get_object({bucket}/{key}) failed: {e:?}"));
        let body = get_resp
            .body
            .collect()
            .await
            .expect("failed to collect get_object body")
            .into_bytes();
        assert_eq!(body.as_ref(), content, "downloaded content does not match uploaded content");

        // 5. delete_object
        client
            .delete_object()
            .bucket(&bucket)
            .key(key)
            .send()
            .await
            .unwrap_or_else(|e| panic!("delete_object({bucket}/{key}) failed: {e:?}"));

        // 6. delete_bucket
        client
            .delete_bucket()
            .bucket(&bucket)
            .send()
            .await
            .unwrap_or_else(|e| panic!("delete_bucket({bucket}) failed: {e:?}"));
    }
}
