use serde::Deserialize;
use url::Url;

/// エンドポイントURL生成時のエラー
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("無効なサーバーアドレス: {0}")]
    InvalidAddress(#[from] url::ParseError),
    #[error("ポート番号の設定に失敗しました")]
    PortSetFailed,
}

fn default_insecure() -> bool {
    false
}

fn default_force_path_style() -> Option<bool> {
    None
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
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
}

impl Env {
    pub fn new() -> Result<Self, envy::Error> {
        envy::prefixed("CAFCE_").from_env::<Env>()
    }

    /// サーバーアドレスからエンドポイントURLを生成する
    ///
    /// - schemeはaws_insecureフラグで決定（true: http, false: https）
    /// - 正規ポート（http:80, https:443）は省略
    /// - aws_server_addressが未指定またはs3.amazonaws.comの場合はNone（SDK既定）
    /// - IPv6アドレス（例: "[::1]:9000"）にも対応
    pub fn build_endpoint(&self) -> Result<Option<Url>, EndpointError> {
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
        let url = Url::parse(&url_str)?;

        // 正規ポートの場合はポートを省略したURLを返す
        let is_default_port = match (url.scheme(), url.port()) {
            ("http", Some(80)) => true,
            ("https", Some(443)) => true,
            _ => false,
        };

        if is_default_port {
            let mut normalized = url.clone();
            normalized.set_port(None).map_err(|_| EndpointError::PortSetFailed)?;
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
        Url::parse(&url_str).ok()?.host_str().map(|s| s.to_string())
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
            Some(host) => {
                let host_lower = host.to_ascii_lowercase();
                !host_lower.contains("amazonaws.com")
            }
            None => false, // SDKデフォルト（AWS S3）はvirtual-hosted
        }
    }

    /// 使用するリージョンを取得する
    pub fn get_region(&self) -> String {
        self.aws_region.clone().unwrap_or_else(|| "us-east-1".to_string())
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
                aws_server_address: None,
                aws_access_key: None,
                aws_secret_key: None,
                aws_session_token: None,
                aws_role_arn: None,
                aws_role_session_name: None,
                aws_profile: None,
                aws_insecure: false,
                aws_region: Some("".to_string()),
                aws_force_path_style: None,
            };
            // 空文字の場合はそのまま返す（バリデーションは別途実施）
            assert_eq!(env.get_region(), "");
        }
    }
}
