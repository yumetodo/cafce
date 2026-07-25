use serde::Deserialize;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("無効なサーバーアドレス: {0}")]
    InvalidAddress(#[from] url::ParseError),
    #[error("ポート番号の設定に失敗しました")]
    PortSetFailed,
}

#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    #[error("環境変数の読み込みに失敗しました: {0}")]
    Envy(#[from] envy::Error),
    #[error(
        "CAFCE_AWS_BUCKET が設定されていません。キャッシュを置く S3 バケット名を指定してください"
    )]
    MissingBucket,
}

fn default_insecure() -> bool {
    false
}

fn default_force_path_style() -> Option<bool> {
    None
}

#[derive(Deserialize, Debug)]
pub struct Env {
    /// S3互換サーバーのアドレス（例: "s3.amazonaws.com", "localhost:9000"）
    aws_server_address: Option<String>,

    /// AWSアクセスキー（MinIO用、またはAssumeRoleのソースクレデンシャル）
    aws_access_key: Option<String>,

    /// AWSシークレットキー
    aws_secret_key: Option<String>,

    /// AWSセッショントークン（一時認証用）
    aws_session_token: Option<String>,

    /// AssumeRole用のRole ARN
    aws_role_arn: Option<String>,

    /// AssumeRoleのセッション名
    aws_role_session_name: Option<String>,

    /// AWSプロファイル名（~/.aws/config のプロファイル）
    aws_profile: Option<String>,

    /// httpを使用するか（true: http, false: https）
    #[serde(default = "default_insecure")]
    aws_insecure: bool,

    /// AWSリージョン（省略時はus-east-1）
    aws_region: Option<String>,

    /// Path-styleを強制するか
    #[serde(default = "default_force_path_style")]
    aws_force_path_style: Option<bool>,

    /// キャッシュを置くS3バケット名（必須）
    aws_bucket: Option<String>,

    /// S3オブジェクトキーの先頭に付ける任意のprefix（末尾スラッシュは正規化）
    s3_prefix: Option<String>,
}

impl Env {
    pub fn new() -> Result<Self, EnvError> {
        let mut env = envy::prefixed("CAFCE_").from_env::<Env>()?;

        if env.aws_bucket.as_deref().is_none_or(str::is_empty) {
            return Err(EnvError::MissingBucket);
        }

        // 末尾スラッシュを正規化し、スラッシュのみなら None に
        if let Some(prefix) = env.s3_prefix.as_mut() {
            let trimmed = prefix.trim_end_matches('/').to_string();
            env.s3_prefix = if trimmed.is_empty() { None } else { Some(trimmed) };
        }

        Ok(env)
    }

    pub fn bucket(&self) -> &str {
        self.aws_bucket
            .as_deref()
            .expect("bucket is validated at construction")
    }

    pub fn s3_prefix(&self) -> Option<&str> {
        self.s3_prefix.as_deref()
    }

    pub fn build_endpoint(&self) -> Result<Option<Url>, EndpointError> {
        let addr = match self.aws_server_address.as_ref() {
            Some(a) if !a.is_empty() => a,
            _ => return Ok(None),
        };

        if addr == "s3.amazonaws.com" {
            return Ok(None);
        }

        let scheme = if self.aws_insecure { "http" } else { "https" };
        let url_str = format!("{scheme}://{addr}");
        let url = Url::parse(&url_str)?;

        let is_default_port = matches!(
            (url.scheme(), url.port()),
            ("http", Some(80)) | ("https", Some(443))
        );

        if is_default_port {
            let mut normalized = url.clone();
            normalized
                .set_port(None)
                .map_err(|_| EndpointError::PortSetFailed)?;
            Ok(Some(normalized))
        } else {
            Ok(Some(url))
        }
    }

    fn get_host(&self) -> Option<String> {
        let addr = self.aws_server_address.as_ref()?;
        if addr.is_empty() {
            return None;
        }
        let url_str = format!("http://{addr}");
        Url::parse(&url_str).ok()?.host_str().map(|s| s.to_string())
    }

    pub fn should_use_path_style(&self) -> bool {
        if let Some(force) = self.aws_force_path_style {
            return force;
        }
        match self.get_host() {
            Some(host) => !host.to_ascii_lowercase().contains("amazonaws.com"),
            None => false,
        }
    }

    pub fn get_region(&self) -> String {
        self.aws_region
            .clone()
            .unwrap_or_else(|| "us-east-1".to_string())
    }

    pub fn access_key(&self) -> Option<&str> {
        self.aws_access_key.as_deref()
    }

    pub fn secret_key(&self) -> Option<&str> {
        self.aws_secret_key.as_deref()
    }

    pub fn session_token(&self) -> Option<&str> {
        self.aws_session_token.as_deref()
    }

    pub fn role_arn(&self) -> Option<&str> {
        self.aws_role_arn.as_deref()
    }

    pub fn role_session_name(&self) -> Option<&str> {
        self.aws_role_session_name.as_deref()
    }

    pub fn profile(&self) -> Option<&str> {
        self.aws_profile.as_deref()
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_for_test(
        server_address: Option<String>,
        access_key: Option<String>,
        secret_key: Option<String>,
        session_token: Option<String>,
        role_arn: Option<String>,
        role_session_name: Option<String>,
        profile: Option<String>,
        insecure: bool,
        region: Option<String>,
        force_path_style: Option<bool>,
    ) -> Self {
        Self {
            aws_server_address: server_address,
            aws_access_key: access_key,
            aws_secret_key: secret_key,
            aws_session_token: session_token,
            aws_role_arn: role_arn,
            aws_role_session_name: role_session_name,
            aws_profile: profile,
            aws_insecure: insecure,
            aws_region: region,
            aws_force_path_style: force_path_style,
            aws_bucket: None,
            s3_prefix: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_with_bucket(
        server_address: Option<String>,
        access_key: Option<String>,
        secret_key: Option<String>,
        insecure: bool,
        bucket: String,
        s3_prefix: Option<String>,
    ) -> Self {
        Self {
            aws_server_address: server_address,
            aws_access_key: access_key,
            aws_secret_key: secret_key,
            aws_session_token: None,
            aws_role_arn: None,
            aws_role_session_name: None,
            aws_profile: None,
            aws_insecure: insecure,
            aws_region: None,
            aws_force_path_style: None,
            aws_bucket: Some(bucket),
            s3_prefix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_env(
        server_address: Option<&str>,
        insecure: bool,
        force_path_style: Option<bool>,
        region: Option<&str>,
    ) -> Env {
        Env {
            aws_server_address: server_address.map(String::from),
            aws_access_key: None,
            aws_secret_key: None,
            aws_session_token: None,
            aws_role_arn: None,
            aws_role_session_name: None,
            aws_profile: None,
            aws_insecure: insecure,
            aws_region: region.map(String::from),
            aws_force_path_style: force_path_style,
            aws_bucket: None,
            s3_prefix: None,
        }
    }

    mod bucket_prefix_tests {
        use super::*;

        #[test]
        fn test_bucket_accessor() {
            // Arrange
            let env = Env {
                aws_bucket: Some("my-bucket".to_string()),
                s3_prefix: None,
                ..create_test_env(None, false, None, None)
            };

            // Act
            let bucket = env.bucket();

            // Assert
            assert_eq!(bucket, "my-bucket");
        }

        #[test]
        fn test_s3_prefix_some() {
            // Arrange
            let env = Env {
                aws_bucket: Some("my-bucket".to_string()),
                s3_prefix: Some("my-prefix".to_string()),
                ..create_test_env(None, false, None, None)
            };

            // Act
            let prefix = env.s3_prefix();

            // Assert
            assert_eq!(prefix, Some("my-prefix"));
        }

        #[test]
        fn test_s3_prefix_none() {
            // Arrange
            let env = Env {
                aws_bucket: Some("my-bucket".to_string()),
                s3_prefix: None,
                ..create_test_env(None, false, None, None)
            };

            // Act
            let prefix = env.s3_prefix();

            // Assert
            assert_eq!(prefix, None);
        }
    }

    mod build_endpoint_tests {
        use super::*;

        #[test]
        fn test_build_endpoint_localhost_http() {
            let env = create_test_env(Some("localhost:9000"), true, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            assert!(endpoint.is_some());
            assert_eq!(endpoint.unwrap().as_str(), "http://localhost:9000/");
        }

        #[test]
        fn test_build_endpoint_localhost_https() {
            let env = create_test_env(Some("localhost:9000"), false, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            assert!(endpoint.is_some());
            assert_eq!(endpoint.unwrap().as_str(), "https://localhost:9000/");
        }

        #[test]
        fn test_build_endpoint_http_default_port() {
            let env = create_test_env(Some("localhost:80"), true, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            assert!(endpoint.is_some());
            assert_eq!(endpoint.unwrap().as_str(), "http://localhost/");
        }

        #[test]
        fn test_build_endpoint_https_default_port() {
            let env = create_test_env(Some("localhost:443"), false, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            assert!(endpoint.is_some());
            assert_eq!(endpoint.unwrap().as_str(), "https://localhost/");
        }

        #[test]
        fn test_build_endpoint_ipv6() {
            let env = create_test_env(Some("[::1]:9000"), true, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            assert!(endpoint.is_some());
            assert_eq!(endpoint.unwrap().as_str(), "http://[::1]:9000/");
        }

        #[test]
        fn test_build_endpoint_aws_s3() {
            let env = create_test_env(Some("s3.amazonaws.com"), false, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            assert!(endpoint.is_none());
        }

        #[test]
        fn test_build_endpoint_none() {
            let env = create_test_env(None, false, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            assert!(endpoint.is_none());
        }

        #[test]
        fn test_build_endpoint_empty() {
            let env = create_test_env(Some(""), false, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            assert!(endpoint.is_none());
        }

        #[test]
        fn test_build_endpoint_ip_address() {
            let env = create_test_env(Some("10.200.1.157:9000"), true, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            assert!(endpoint.is_some());
            assert_eq!(endpoint.unwrap().as_str(), "http://10.200.1.157:9000/");
        }
    }

    mod should_use_path_style_tests {
        use super::*;

        #[test]
        fn test_should_use_path_style_explicit_true() {
            let env = create_test_env(Some("localhost:9000"), false, Some(true), None);
            assert!(env.should_use_path_style());
        }

        #[test]
        fn test_should_use_path_style_explicit_false() {
            let env = create_test_env(Some("localhost:9000"), false, Some(false), None);
            assert!(!env.should_use_path_style());
        }

        #[test]
        fn test_should_use_path_style_auto_minio() {
            let env = create_test_env(Some("localhost:9000"), false, None, None);
            assert!(env.should_use_path_style());
        }

        #[test]
        fn test_should_use_path_style_auto_aws() {
            let env = create_test_env(Some("s3.amazonaws.com"), false, None, None);
            assert!(!env.should_use_path_style());
        }

        #[test]
        fn test_should_use_path_style_auto_aws_regional() {
            let env = create_test_env(Some("s3.ap-northeast-1.amazonaws.com"), false, None, None);
            assert!(!env.should_use_path_style());
        }

        #[test]
        fn test_should_use_path_style_none() {
            let env = create_test_env(None, false, None, None);
            assert!(!env.should_use_path_style());
        }

        #[test]
        fn test_should_use_path_style_ipv6() {
            let env = create_test_env(Some("[::1]:9000"), false, None, None);
            assert!(env.should_use_path_style());
        }
    }

    mod get_region_tests {
        use super::*;

        #[test]
        fn test_get_region_specified() {
            let env = create_test_env(None, false, None, Some("ap-northeast-1"));
            assert_eq!(env.get_region(), "ap-northeast-1");
        }

        #[test]
        fn test_get_region_default() {
            let env = create_test_env(None, false, None, None);
            assert_eq!(env.get_region(), "us-east-1");
        }

        #[test]
        fn test_get_region_empty() {
            let env = Env {
                aws_region: Some("".to_string()),
                ..create_test_env(None, false, None, None)
            };
            assert_eq!(env.get_region(), "");
        }
    }
}
