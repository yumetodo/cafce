/// エンドポイントURL生成時のエラー
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

/// S3 フレキシブルチェックサム（`x-amz-checksum-sha256`）の使い方
///
/// AWS S3 の拡張機能であり S3 互換サーバの対応状況はまちまちなため、
/// 既定は「使えるなら使う」ベストエフォートとする。
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum S3ChecksumMode {
    /// 使えるなら使う。非対応に起因すると判断できる失敗はチェックサム無しで 1 回だけ再試行する
    Auto,
    /// チェックサムを付けない。`restore` でも `checksum_mode` を指定しない
    Off,
    /// フォールバックしない。`store` の失敗はそのままエラー。
    /// `restore` でサーバがチェックサムを返さなかった場合もエラーにする
    Required,
}

fn default_insecure() -> bool {
    false
}

fn default_force_path_style() -> Option<bool> {
    None
}

fn default_s3_checksum() -> S3ChecksumMode {
    S3ChecksumMode::Auto
}

#[derive(serde::Deserialize)]
pub struct Env {
    /// S3互換サーバーのアドレス
    /// 例: "s3.amazonaws.com", "localhost:9000", "10.200.1.157:9000"
    /// 省略時: SDKデフォルト（AWS S3）
    aws_server_address: Option<String>,

    /// AWSアクセスキー（MinIO用、またはAssumeRoleのソースクレデンシャル）
    /// 省略時: SDK credential provider chainを使用
    aws_access_key: Option<String>,

    /// AWSシークレットキー
    aws_secret_key: Option<String>,

    /// AWSセッショントークン（一時認証用、既にAssumeRole済みの場合など）
    aws_session_token: Option<String>,

    /// AssumeRole用のRole ARN
    /// 指定時: aws_access_key/secret_keyをソースクレデンシャルとしてAssumeRoleを実行
    /// 例: "arn:aws:iam::123456789012:role/my-role"
    aws_role_arn: Option<String>,

    /// AssumeRoleのセッション名
    /// 省略時: "cafce-session"
    aws_role_session_name: Option<String>,

    /// AWSプロファイル名（~/.aws/config のプロファイル）
    /// 例: "my-profile", "assume-role-profile"
    /// プロファイル内でrole_arn設定があれば自動でAssumeRole
    aws_profile: Option<String>,

    /// httpを使用するか（true: http, false: https）
    /// ローカルMinIOではtrueを推奨
    #[serde(default = "default_insecure")]
    aws_insecure: bool,

    /// AWSリージョン（省略時はus-east-1）
    /// MinIOの場合は通常 "us-east-1" を使用
    aws_region: Option<String>,

    /// Path-styleを強制するか
    /// None: 自動判定（amazonaws.comならfalse、それ以外はtrue）
    /// Some(true): Path-style強制
    /// Some(false): Virtual-hosted style強制
    #[serde(default = "default_force_path_style")]
    aws_force_path_style: Option<bool>,

    /// キャッシュを置くS3バケット名（必須）
    aws_bucket: Option<String>,

    /// S3オブジェクトキーの先頭に付ける任意のprefix（末尾スラッシュは正規化）
    s3_prefix: Option<String>,

    /// S3フレキシブルチェックサムの挙動（auto / off / required）
    #[serde(default = "default_s3_checksum")]
    s3_checksum: S3ChecksumMode,
}

/// 資格情報をCIログへ露出させないための手書き`Debug`実装
///
/// アクセスキー・シークレットキー・セッショントークンは固定文字列に置き換える。
/// 値の有無だけは診断のため区別できるよう、`None`は`None`のまま表示する。
impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn redact(value: &Option<String>) -> Option<&'static str> {
            value.as_ref().map(|_| "***")
        }

        f.debug_struct("Env")
            .field("aws_server_address", &self.aws_server_address)
            .field("aws_access_key", &redact(&self.aws_access_key))
            .field("aws_secret_key", &redact(&self.aws_secret_key))
            .field("aws_session_token", &redact(&self.aws_session_token))
            .field("aws_role_arn", &self.aws_role_arn)
            .field("aws_role_session_name", &self.aws_role_session_name)
            .field("aws_profile", &self.aws_profile)
            .field("aws_insecure", &self.aws_insecure)
            .field("aws_region", &self.aws_region)
            .field("aws_force_path_style", &self.aws_force_path_style)
            .field("aws_bucket", &self.aws_bucket)
            .field("s3_prefix", &self.s3_prefix)
            .field("s3_checksum", &self.s3_checksum)
            .finish()
    }
}

fn normalize_s3_prefix(prefix: Option<String>) -> Option<String> {
    prefix.and_then(|p| {
        let trimmed = p.trim_end_matches('/').to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

impl Env {
    pub fn new() -> Result<Self, EnvError> {
        let mut env = envy::prefixed("CAFCE_").from_env::<Env>()?;

        if env.aws_bucket.as_deref().is_none_or(str::is_empty) {
            return Err(EnvError::MissingBucket);
        }

        env.s3_prefix = normalize_s3_prefix(env.s3_prefix);

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

    /// S3フレキシブルチェックサムの挙動を取得する
    pub fn s3_checksum(&self) -> S3ChecksumMode {
        self.s3_checksum
    }

    /// サーバーアドレスからエンドポイントURLを生成する
    ///
    /// - schemeはaws_insecureフラグで決定（true: http, false: https）
    /// - 正規ポート（http:80, https:443）は省略
    /// - aws_server_addressが未指定またはs3.amazonaws.comの場合はNone（SDK既定）
    /// - IPv6アドレス（例: "[::1]:9000"）にも対応
    pub fn build_endpoint(&self) -> Result<Option<url::Url>, EndpointError> {
        let addr = match self.aws_server_address.as_ref() {
            Some(a) if !a.is_empty() => a,
            _ => return Ok(None),
        };

        // "s3.amazonaws.com"の場合はエンドポイント指定不要（SDK既定に任せる）
        if addr == "s3.amazonaws.com" {
            return Ok(None);
        }

        let scheme = if self.aws_insecure { "http" } else { "https" };

        // server_addressが "localhost:9000" や "[::1]:9000" のような形式の場合
        // 仮のURLとして組み立ててパース（url crateがIPv6も正しく処理）
        let url_str = format!("{scheme}://{addr}");
        let url = url::Url::parse(&url_str)?;

        // 正規ポートの場合はポートを省略したURLを返す

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

    /// サーバーアドレスからホスト部分を取得する（Path-style判定用）
    ///
    /// IPv6アドレスの場合もホスト部分を正しく抽出
    fn get_host(&self) -> Option<String> {
        let addr = self.aws_server_address.as_ref()?;
        if addr.is_empty() {
            return None;
        }

        // 仮のURLとしてパースしてホストを取得
        let url_str = format!("http://{addr}");
        url::Url::parse(&url_str)
            .ok()?
            .host_str()
            .map(|s| s.to_string())
    }

    /// Path-styleを使用すべきか判定する
    ///
    /// - 明示的に指定されていればその値を使用
    /// - 未指定の場合は自動判定:
    ///   - aws_server_address未指定またはamazonaws.comを含む -> false (virtual-hosted style)
    ///   - それ以外 -> true (path-style)
    /// - get_host()を使用してIPv6アドレスからも正しくホスト部分を抽出
    pub fn should_use_path_style(&self) -> bool {
        if let Some(force) = self.aws_force_path_style {
            return force;
        }
        match self.get_host() {
            Some(host) => !host.to_ascii_lowercase().contains("amazonaws.com"),
            None => false, // SDKデフォルト（AWS S3）はvirtual-hosted
        }
    }

    /// 使用するリージョンを取得する
    pub fn get_region(&self) -> String {
        self.aws_region
            .clone()
            .unwrap_or_else(|| "us-east-1".to_string())
    }

    /// AWSアクセスキーを取得する
    ///
    /// 未指定の場合はNone（SDK credential provider chainに委ねる）
    pub fn access_key(&self) -> Option<&str> {
        self.aws_access_key.as_deref()
    }

    /// AWSシークレットキーを取得する
    ///
    /// 未指定の場合はNone（SDK credential provider chainに委ねる）
    pub fn secret_key(&self) -> Option<&str> {
        self.aws_secret_key.as_deref()
    }

    /// AWSセッショントークンを取得する
    ///
    /// 未指定の場合はNone
    pub fn session_token(&self) -> Option<&str> {
        self.aws_session_token.as_deref()
    }

    /// AssumeRole用のRole ARNを取得する
    ///
    /// 未指定の場合はNone（AssumeRoleを実行しない）
    pub fn role_arn(&self) -> Option<&str> {
        self.aws_role_arn.as_deref()
    }

    /// AssumeRoleのセッション名を取得する
    ///
    /// 未指定の場合はNone（呼び出し側で"cafce-session"等の既定値を使用する）
    pub fn role_session_name(&self) -> Option<&str> {
        self.aws_role_session_name.as_deref()
    }

    /// AWSプロファイル名を取得する
    ///
    /// 未指定の場合はNone（SDK credential provider chainに委ねる）
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
            s3_checksum: default_s3_checksum(),
        }
    }

    #[doc(hidden)]
    pub fn new_for_test_with_bucket(
        server_address: Option<String>,
        access_key: Option<String>,
        secret_key: Option<String>,
        insecure: bool,
        bucket: String,
        s3_prefix: Option<String>,
        s3_checksum: S3ChecksumMode,
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
            s3_checksum,
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
            s3_checksum: default_s3_checksum(),
        }
    }

    mod redacted_debug_tests {
        use super::*;

        /// 資格情報を全て埋めた`Env`を作る（redact対象が実際に埋まっている状態を作る）
        fn create_env_with_secrets() -> Env {
            Env {
                aws_access_key: Some("AKIAIOSFODNN7EXAMPLE".to_string()),
                aws_secret_key: Some("wJalrXUtnFEMI-K7MDENG-bPxRfiCYEXAMPLEKEY".to_string()),
                aws_session_token: Some("FwoGZXIvYXdzEXAMPLESESSIONTOKEN".to_string()),
                ..create_test_env(Some("localhost:9000"), true, None, None)
            }
        }

        #[test]
        fn test_debug_does_not_contain_access_key() {
            // Arrange
            let env = create_env_with_secrets();

            // Act
            let debug_output = format!("{env:?}");

            // Assert
            assert!(!debug_output.contains("AKIAIOSFODNN7EXAMPLE"));
        }

        #[test]
        fn test_debug_does_not_contain_secret_key() {
            // Arrange
            let env = create_env_with_secrets();

            // Act
            let debug_output = format!("{env:?}");

            // Assert
            assert!(!debug_output.contains("wJalrXUtnFEMI-K7MDENG-bPxRfiCYEXAMPLEKEY"));
        }

        #[test]
        fn test_debug_does_not_contain_session_token() {
            // Arrange
            let env = create_env_with_secrets();

            // Act
            let debug_output = format!("{env:?}");

            // Assert
            assert!(!debug_output.contains("FwoGZXIvYXdzEXAMPLESESSIONTOKEN"));
        }

        #[test]
        fn test_debug_alternate_form_does_not_contain_secrets() {
            // Arrange: `{:#?}`（pretty形式）でも redact が効くことを確認する
            let env = create_env_with_secrets();

            // Act
            let debug_output = format!("{env:#?}");

            // Assert
            assert!(!debug_output.contains("AKIAIOSFODNN7EXAMPLE"));
            assert!(!debug_output.contains("wJalrXUtnFEMI-K7MDENG-bPxRfiCYEXAMPLEKEY"));
            assert!(!debug_output.contains("FwoGZXIvYXdzEXAMPLESESSIONTOKEN"));
        }

        #[test]
        fn test_debug_shows_redaction_placeholder_when_set() {
            // Arrange
            let env = create_env_with_secrets();

            // Act
            let debug_output = format!("{env:?}");

            // Assert: 値の有無は診断のため区別できる
            assert!(debug_output.contains("aws_access_key: Some(\"***\")"));
            assert!(debug_output.contains("aws_secret_key: Some(\"***\")"));
            assert!(debug_output.contains("aws_session_token: Some(\"***\")"));
        }

        #[test]
        fn test_debug_shows_none_for_unset_credentials() {
            // Arrange: 資格情報が未設定の Env
            let env = create_test_env(Some("localhost:9000"), true, None, None);

            // Act
            let debug_output = format!("{env:?}");

            // Assert: 未設定は None のまま表示する
            assert!(debug_output.contains("aws_access_key: None"));
            assert!(debug_output.contains("aws_secret_key: None"));
            assert!(debug_output.contains("aws_session_token: None"));
        }

        #[test]
        fn test_debug_keeps_non_secret_fields_visible() {
            // Arrange
            let env = create_env_with_secrets();

            // Act
            let debug_output = format!("{env:?}");

            // Assert: 診断に必要な非機密フィールドはそのまま見える
            assert!(debug_output.contains("localhost:9000"));
        }
    }

    mod s3_checksum_tests {
        use super::*;

        #[test]
        fn test_default_is_auto() {
            // Arrange
            let env = create_test_env(None, false, None, None);

            // Act
            let mode = env.s3_checksum();

            // Assert
            assert_eq!(mode, S3ChecksumMode::Auto);
        }

        #[test]
        fn test_deserialize_lowercase_values() {
            // Arrange
            let inputs = [
                ("\"auto\"", S3ChecksumMode::Auto),
                ("\"off\"", S3ChecksumMode::Off),
                ("\"required\"", S3ChecksumMode::Required),
            ];

            for (json, expected) in inputs {
                // Act
                let parsed: S3ChecksumMode =
                    parse_checksum_mode(json).expect("既知の値はパースできるはず");

                // Assert
                assert_eq!(parsed, expected, "input={json}");
            }
        }

        #[test]
        fn test_deserialize_unknown_value_is_error() {
            // Arrange
            let json = "\"yes\"";

            // Act
            let parsed = parse_checksum_mode(json);

            // Assert
            assert!(parsed.is_err());
        }

        /// `serde_json`を依存に持たないため、TOMLのvalueとしてデシリアライズする
        fn parse_checksum_mode(quoted: &str) -> Result<S3ChecksumMode, toml::de::Error> {
            let doc = format!("value = {quoted}");
            #[derive(serde::Deserialize)]
            struct Wrapper {
                value: S3ChecksumMode,
            }
            toml::from_str::<Wrapper>(&doc).map(|w| w.value)
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

        #[test]
        fn test_prefix_trailing_slash_normalized() {
            // Arrange
            let input = Some("my-prefix/".to_string());

            // Act
            let result = normalize_s3_prefix(input);

            // Assert
            assert_eq!(result.as_deref(), Some("my-prefix"));
        }

        #[test]
        fn test_prefix_multiple_trailing_slashes_normalized() {
            // Arrange
            let input = Some("my-prefix///".to_string());

            // Act
            let result = normalize_s3_prefix(input);

            // Assert
            assert_eq!(result.as_deref(), Some("my-prefix"));
        }

        #[test]
        fn test_prefix_only_slashes_becomes_none() {
            // Arrange
            let input = Some("/".to_string());

            // Act
            let result = normalize_s3_prefix(input);

            // Assert
            assert_eq!(result, None);
        }

        #[test]
        fn test_prefix_empty_string_becomes_none() {
            // Arrange: envy は空文字の env var を Some("") として渡す
            let input = Some("".to_string());

            // Act
            let result = normalize_s3_prefix(input);

            // Assert
            assert_eq!(result, None);
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
            // 正規ポートは省略される
            assert_eq!(endpoint.unwrap().as_str(), "http://localhost/");
        }

        #[test]
        fn test_build_endpoint_https_default_port() {
            let env = create_test_env(Some("localhost:443"), false, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            assert!(endpoint.is_some());
            // 正規ポートは省略される
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
            // s3.amazonaws.comの場合はNone（SDK既定に任せる）
            assert!(endpoint.is_none());
        }

        #[test]
        fn test_build_endpoint_none() {
            let env = create_test_env(None, false, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            // server_address未指定の場合はNone
            assert!(endpoint.is_none());
        }

        #[test]
        fn test_build_endpoint_empty() {
            let env = create_test_env(Some(""), false, None, None);
            let result = env.build_endpoint();
            assert!(result.is_ok());
            let endpoint = result.unwrap();
            // 空文字の場合はNone
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
            // localhost はamazonaws.comを含まないため、path-style
            assert!(env.should_use_path_style());
        }

        #[test]
        fn test_should_use_path_style_auto_aws() {
            let env = create_test_env(Some("s3.amazonaws.com"), false, None, None);
            // amazonaws.comを含むため、virtual-hosted style
            assert!(!env.should_use_path_style());
        }

        #[test]
        fn test_should_use_path_style_auto_aws_regional() {
            let env = create_test_env(Some("s3.ap-northeast-1.amazonaws.com"), false, None, None);
            // amazonaws.comを含むため、virtual-hosted style
            assert!(!env.should_use_path_style());
        }

        #[test]
        fn test_should_use_path_style_none() {
            let env = create_test_env(None, false, None, None);
            // server_address未指定の場合はfalse（SDKデフォルト）
            assert!(!env.should_use_path_style());
        }

        #[test]
        fn test_should_use_path_style_ipv6() {
            let env = create_test_env(Some("[::1]:9000"), false, None, None);
            // IPv6アドレスはamazonaws.comを含まないため、path-style
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
            // 空文字の場合はそのまま返す（バリデーションは別途実施）
            assert_eq!(env.get_region(), "");
        }
    }
}
