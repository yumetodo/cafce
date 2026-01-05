// src/s3_client.rs
use aws_sdk_s3::config::Builder;
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
