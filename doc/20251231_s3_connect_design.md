# AWS S3 / MinIO 接続設計ドキュメント

## 1. Overview (概要)

本ドキュメントは、cafceプロジェクトにおけるAWS S3およびS3互換サービス（特にローカルのMinIO）への接続方法を設計したものである。単一のコードベースで、本番環境のAWS S3とローカル開発環境のMinIOの両方に対応する接続機能を実現する。

AWS SDK for Rust (`aws-sdk-s3`) を使用し、以下の認証方式に対応する：
- 静的クレデンシャル（MinIO、AWS直接接続）
- AWS Profile（`~/.aws/config` のプロファイル、AssumeRole設定含む）
- 環境変数によるAssumeRole（Role ARNを明示指定）
- SDK credential provider chain（環境変数、IMDS等からの自動検出）
- `aws login` コマンド（AWS Management Console認証情報を使用）
- `aws sso login` コマンド（IAM Identity Center SSO認証）

## 2. Context (背景)

- cafceはS3互換ストレージからファイルを取得するためのツールである
- 本番環境ではAWS S3に接続するが、ローカル開発・テスト環境ではMinIOを使用したい
- AWS SDK for Rustの公式サンプルはhttps接続前提であり、ローカルMinIO（http）への接続には追加の設定が必要
- S3のURL形式（path-style / virtual-hosted style）の違いにも対応が必要
  - AWS S3: 2020年9月30日以降に作成されたバケットはvirtual-hosted styleのみ
  - MinIO: デフォルトはpath-style
- 本番環境ではAssumeRoleやAWS Profileを使用した認証が求められることがある

## 3. Scope (範囲)

### 変更対象ファイル

| ファイル | 役割 |
|---------|------|
| `src/env.rs` | 環境変数の構造体を拡張し、S3接続設定・認証設定を追加 |

### 新規追加ファイル

| ファイル | 役割 |
|---------|------|
| `src/s3_client.rs` | S3クライアントのビルダー関数を実装 |

### 追加が必要な依存関係

| crate | 用途 |
|-------|------|
| `aws-sdk-sts` | AssumeRole実行用 |
| `url` | エンドポイントURL生成・パース（IPv6対応、入力検証） |

### 変更対象外

| ファイル | 理由 |
|---------|------|
| `src/setting.rs` | S3接続設定は環境変数で管理するため、TOMLファイルの設定には含めない |

## 4. Goal (目標)

- **AWS S3とMinIOの両方に接続可能な単一のコードベース**を実現する
- **環境変数のみで接続先・認証方式を切り替え可能**にする（コード変更不要）
- **セキュアなデフォルト設定**を維持する（https優先、明示的なオプトインでhttpを許可）
- **AWS Profile、AssumeRole、SDK credential provider chain**に対応する

## 5. Non-Goal (目標外)

- MinIOサーバーのセットアップ・運用方法の詳細
- Presigned URLの生成機能（将来的な拡張として検討）
- Dualstack / Accelerateなどの AWS固有機能のサポート
- minio crateの使用（AWS SDKに一本化）
- MFA（多要素認証）を必要とするAssumeRole

## 6. Solution / Technical Architecture (解決策 / 技術アーキテクチャ)

### 6.1 環境変数スキーマ

現在の `src/env.rs` を以下のように拡張する:

```rust
use serde::Deserialize;

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
}
```

### 6.2 認証方式の決定フロー

```mermaid
flowchart TD
    START[認証方式の決定] --> CHECK_ROLE{CAFCE_AWS_ROLE_ARN<br/>が指定されている?}

    CHECK_ROLE -->|Yes| CHECK_CRED_ROLE{CAFCE_AWS_ACCESS_KEY<br/>が指定されている?}
    CHECK_CRED_ROLE -->|Yes| ASSUME_ROLE[静的キーでAssumeRole実行<br/>一時クレデンシャル取得]
    CHECK_CRED_ROLE -->|No| ERROR[エラー: AssumeRoleには<br/>ソースクレデンシャルが必要]

    CHECK_ROLE -->|No| CHECK_STATIC{CAFCE_AWS_ACCESS_KEY<br/>が指定されている?}
    CHECK_STATIC -->|Yes| STATIC[静的クレデンシャル使用<br/>MinIO向け / AWS直接接続]
    CHECK_STATIC -->|No| CHECK_PROFILE{CAFCE_AWS_PROFILE<br/>が指定されている?}

    CHECK_PROFILE -->|Yes| PROFILE[指定プロファイルをロード<br/>AssumeRole設定も自動適用]
    CHECK_PROFILE -->|No| SDK_CHAIN[SDK credential provider chain<br/>環境変数→config→IMDS等]
```

### 6.3 エンドポイント生成ロジック

`url` crateを使用してエンドポイントURLを生成する。これにより以下のメリットがある：
- **IPv6アドレス対応**: `[::1]:9000` のような形式を正しくパース
- **入力検証**: 不正なURLを早期に検出
- **型安全なポート操作**: ポートの取得・設定が型安全

```rust
use url::Url;

/// エンドポイントURL生成時のエラー
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("無効なサーバーアドレス: {0}")]
    InvalidAddress(#[from] url::ParseError),
    #[error("ポート番号の設定に失敗しました")]
    PortSetFailed,
}

impl Env {
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
}
```

### 6.4 S3クライアント生成

設計上、S3クライアント生成ロジックは以下の2つの関数に分離する：

1. **`apply_s3_config()`**: S3設定ビルダーに環境変数に基づく設定を適用する純粋関数
2. **`build_s3_client()`**: SDK設定のロード、認証、S3クライアント生成を統括するasync関数

この分離により、`apply_s3_config()` はネットワーク通信なしでユニットテスト可能となる。

```rust
// src/s3_client.rs
use aws_sdk_s3::config::{Builder, Credentials, Region};
use aws_sdk_s3::Client;
use aws_config::BehaviorVersion;
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

/// AssumeRoleを実行して一時クレデンシャルを取得する
async fn assume_role(
    access_key: &str,
    secret_key: &str,
    role_arn: &str,
    session_name: Option<&str>,
    region: &str,
) -> Result<Credentials, aws_sdk_sts::Error> {
    // STSクライアントを静的クレデンシャルで作成
    let sts_config = aws_config::defaults(BehaviorVersion::latest())
        .credentials_provider(Credentials::new(
            access_key.to_string(),
            secret_key.to_string(),
            None,
            None,
            "cafce-source",
        ))
        .region(Region::new(region.to_string()))
        .load()
        .await;

    let sts_client = aws_sdk_sts::Client::new(&sts_config);

    // AssumeRole実行
    let response = sts_client
        .assume_role()
        .role_arn(role_arn)
        .role_session_name(session_name.unwrap_or("cafce-session"))
        .send()
        .await?;

    let creds = response.credentials.expect("AssumeRole should return credentials");
    Ok(Credentials::new(
        creds.access_key_id,
        creds.secret_access_key,
        Some(creds.session_token),
        Some(creds.expiration.into()),
        "cafce-assumed-role",
    ))
}

/// 環境変数に基づいてS3クライアントを構築する
///
/// この関数は以下の処理を行う：
/// 1. AWS SDK設定のロード（ネットワーク通信の可能性あり）
/// 2. 認証情報の設定（AssumeRole実行時はネットワーク通信あり）
/// 3. apply_s3_config()によるエンドポイント・Path-style設定
/// 4. S3クライアントの生成
pub async fn build_s3_client(env: &Env) -> Result<Client, Box<dyn std::error::Error>> {
    // ベースのconfig loaderを作成
    let mut config_loader = aws_config::defaults(BehaviorVersion::latest());

    // リージョン設定
    config_loader = config_loader.region(Region::new(env.get_region()));

    // プロファイル指定（AssumeRole設定も含めて自動でロード）
    if let Some(profile) = &env.aws_profile {
        config_loader = config_loader.profile_name(profile);
    }

    // 共有設定をロード
    let shared_config = config_loader.load().await;

    let mut builder = aws_sdk_s3::config::Builder::from(&shared_config);

    // 認証方式の決定
    if let Some(role_arn) = &env.aws_role_arn {
        // AssumeRole: aws_access_key/secret_keyをソースとして使用
        let access_key = env.aws_access_key.as_ref()
            .ok_or("CAFCE_AWS_ROLE_ARN が指定されている場合、CAFCE_AWS_ACCESS_KEY は必須です")?;
        let secret_key = env.aws_secret_key.as_ref()
            .ok_or("CAFCE_AWS_ROLE_ARN が指定されている場合、CAFCE_AWS_SECRET_KEY は必須です")?;

        let assumed_creds = assume_role(
            access_key,
            secret_key,
            role_arn,
            env.aws_role_session_name.as_deref(),
            &env.get_region(),
        ).await?;

        builder = builder.credentials_provider(assumed_creds);
    } else if let (Some(access_key), Some(secret_key)) = (&env.aws_access_key, &env.aws_secret_key) {
        // 静的クレデンシャル（MinIO向け / AWS直接接続）
        builder = builder.credentials_provider(
            Credentials::new(
                access_key.clone(),
                secret_key.clone(),
                env.aws_session_token.clone(),
                None,
                "cafce-static",
            )
        );
    }
    // それ以外はSDK credential provider chainに任せる（Profile指定済み、または自動検出）

    // エンドポイント・Path-style設定を適用
    let builder = apply_s3_config(builder, env)?;

    Ok(Client::from_conf(builder.build()))
}
```

### 6.5 システムコンテキスト図

```mermaid
flowchart TB
    subgraph cafce["cafce アプリケーション"]
        ENV["Env (環境変数)"]
        AUTH["認証方式判定"]
        CLIENT["S3 Client Builder"]
        SDK["AWS SDK for Rust"]
    end

    subgraph credentials["認証方式"]
        STATIC["静的クレデンシャル"]
        ASSUME["AssumeRole"]
        PROFILE["AWS Profile"]
        CHAIN["SDK Provider Chain"]
    end

    subgraph storage["S3互換ストレージ"]
        AWS["AWS S3<br/>(https://s3.amazonaws.com)"]
        MINIO["MinIO<br/>(http://localhost:9000)"]
    end

    ENV -->|設定値| AUTH
    AUTH --> STATIC
    AUTH --> ASSUME
    AUTH --> PROFILE
    AUTH --> CHAIN

    STATIC --> CLIENT
    ASSUME --> CLIENT
    PROFILE --> CLIENT
    CHAIN --> CLIENT

    CLIENT -->|build| SDK
    SDK -->|https + virtual-hosted| AWS
    SDK -->|http + path-style| MINIO
```

### 6.6 設定パターン

#### パターン1: ローカルMinIO接続

```bash
export CAFCE_AWS_SERVER_ADDRESS="localhost:9000"
export CAFCE_AWS_ACCESS_KEY="minioadmin"
export CAFCE_AWS_SECRET_KEY="minioadmin"
export CAFCE_AWS_INSECURE="true"  # http を使用
# CAFCE_AWS_REGION は省略（デフォルト: us-east-1）
# CAFCE_AWS_FORCE_PATH_STYLE は省略（自動判定: true = path-style）
```

#### パターン2: AWS静的クレデンシャル接続

```bash
export CAFCE_AWS_ACCESS_KEY="AKIAIOSFODNN7EXAMPLE"
export CAFCE_AWS_SECRET_KEY="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
export CAFCE_AWS_REGION="ap-northeast-1"
# CAFCE_AWS_SERVER_ADDRESS は省略（SDKデフォルト: AWS S3）
# CAFCE_AWS_INSECURE は省略（デフォルト: false = https）
# CAFCE_AWS_FORCE_PATH_STYLE は省略（自動判定: false = virtual-hosted）
```

#### パターン3: AWS Profile使用

```bash
export CAFCE_AWS_PROFILE="my-profile"
export CAFCE_AWS_REGION="ap-northeast-1"
# ~/.aws/config の [profile my-profile] が使用される
# プロファイル内でrole_arn設定があれば自動でAssumeRole
```

`~/.aws/config` の例（AssumeRole設定あり）:
```ini
[profile my-profile]
role_arn = arn:aws:iam::123456789012:role/my-role
source_profile = base-profile
region = ap-northeast-1

[profile base-profile]
aws_access_key_id = AKIAIOSFODNN7EXAMPLE
aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
```

#### パターン4: 環境変数でAssumeRole

```bash
export CAFCE_AWS_ROLE_ARN="arn:aws:iam::123456789012:role/my-role"
export CAFCE_AWS_ACCESS_KEY="AKIAIOSFODNN7EXAMPLE"  # ソースクレデンシャル
export CAFCE_AWS_SECRET_KEY="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
export CAFCE_AWS_ROLE_SESSION_NAME="my-session"  # 省略可
export CAFCE_AWS_REGION="ap-northeast-1"
```

#### パターン5: SDK自動検出（EC2/ECS環境など）

```bash
export CAFCE_AWS_REGION="ap-northeast-1"
# 他の認証関連環境変数は設定しない
# SDK が AWS_ACCESS_KEY_ID, ~/.aws/credentials, EC2 IMDS 等から自動検出
```

#### パターン6: `aws login` コマンド使用（Login credentials provider）

`aws login` コマンドでAWS Management Consoleの認証情報を使用してログインし、短期クレデンシャルを取得するパターン。

**事前準備**:
```bash
# AWS CLIでログイン（ブラウザ認証フロー）
aws login --profile console
```

**cafce実行時**:
```bash
export CAFCE_AWS_PROFILE="console"
export CAFCE_AWS_REGION="ap-northeast-1"
```

`~/.aws/config` の例:
```ini
[profile console]
login_session = arn:aws:iam::123456789012:user/username
region = ap-northeast-1
```

**特徴**:
- 一時クレデンシャルは `~/.aws/login/cache` にキャッシュ
- 15分で期限切れだが、SDKが12時間まで自動リフレッシュ
- 12時間後に再ログインが必要

#### パターン7: `aws sso login` コマンド使用（IAM Identity Center SSO）

IAM Identity Center (旧AWS SSO) を使用したシングルサインオン認証パターン。

**事前準備**:
```bash
# AWS CLIでSSOログイン
aws sso login --sso-session my-sso
```

**cafce実行時**:
```bash
export CAFCE_AWS_PROFILE="dev"
export CAFCE_AWS_REGION="ap-northeast-1"
```

`~/.aws/config` の例（SSO token provider configuration、推奨）:
```ini
[profile dev]
sso_session = my-sso
sso_account_id = 111122223333
sso_role_name = SampleRole

[sso-session my-sso]
sso_region = us-east-1
sso_start_url = https://my-sso-portal.awsapps.com/start
sso_registration_scopes = sso:account:access
```

**特徴**:
- トークンは `~/.aws/sso/cache` にキャッシュ
- SSO token provider configurationでは自動リフレッシュ対応
- セッション期間はIAM Identity Centerの設定に依存

## 7. Alternative Solution (代替案)

### 7.1 aws_mode フラグの追加

**概要**: `aws_mode: Option<String>` (`"minio" | "aws" | "auto"`) を追加し、接続モードを明示的に指定する

**Pros**:
- 設定の意図が明確
- モード別のバリデーションが可能
- 将来的な他のS3互換サービス（Ceph, Wasabi等）への拡張が容易

**Cons**:
- 環境変数が増える
- `aws_server_address` と `aws_force_path_style` で十分に判定可能
- `aws_mode` と他の設定の整合性チェックが必要

**判断**: 不採用。`aws_server_address` のドメイン判定と `aws_force_path_style` の明示指定で十分に対応可能。

### 7.2 minio crateの使用

**概要**: MinIO公式のRust SDK (`minio` crate) を使用

**Pros**:
- MinIO固有の最適化が期待できる
- MinIO公式サポート

**Cons**:
- AWS S3とMinIOで別のSDKを使い分ける必要がある
- 抽象化レイヤーの実装が必要
- aws-sdk-s3で十分に対応可能

**判断**: 不採用。AWS SDK for Rustに一本化する。

### 7.3 AssumeRoleをProfile経由のみに限定

**概要**: `CAFCE_AWS_ROLE_ARN` は設けず、AssumeRoleは全て `~/.aws/config` のプロファイル設定経由とする

**Pros**:
- 環境変数が減る
- AWS SDKの標準的な方法に準拠

**Cons**:
- CI/CD環境などで `~/.aws/config` を用意するのが煩雑な場合がある
- 環境変数のみで完結させたいユースケースに対応できない

**判断**: 不採用。環境変数でRole ARNを指定できる方が柔軟性が高い。

## 8. Concerns (懸念事項)

- **自己署名証明書への対応**:
  - ローカルMinIOでhttps + 自己署名証明書を使用する場合、hyper + rustls のカスタム設定が必要
  - 本設計ではhttp使用を推奨し、自己署名証明書対応は範囲外とする

- **バケット名に `.` を含む場合**:
  - httpsでvirtual-hosted styleを使用する場合、証明書検証が失敗する可能性がある
  - 該当する場合は `CAFCE_AWS_FORCE_PATH_STYLE=true` を明示的に設定する

- **AssumeRoleの一時クレデンシャル有効期限**:
  - デフォルトの有効期限（1時間）を超える長時間実行の場合、クレデンシャルのリフレッシュが必要
  - 本設計では初回取得のみ対応し、リフレッシュは範囲外とする

- **MFA（多要素認証）**:
  - MFAが必要なAssumeRoleには対応しない（`mfa_serial` 設定は範囲外）

## 10. Safety and Reliability (安全性と信頼性)

### テストの実施

- **ユニットテスト**:
  - `build_endpoint()` のロジック検証（正規ポート省略、スキーム切り替え、aws_server_address未指定時）
  - `should_use_path_style()` の自動判定ロジック検証
  - `get_region()` のデフォルト値検証
  - `apply_s3_config()` のS3設定適用検証

- **統合テスト**:
  - MinIO (Docker) への接続テスト: List/Put/Get/Delete操作
  - AWS S3への接続テスト（CI環境）: List操作
    - `doc/20260101_aws_integration_test_design.md` 参照

### ユニットテストの設計方針

**ネットワーク通信を伴わないテスト設計**

本設計では、S3クライアント生成ロジックを以下の2つに分離することで、認証処理を除くユニットテストをネットワーク通信なしで実行可能としている：

| 関数 | テスト方法 | ネットワーク通信 |
|------|-----------|-----------------|
| `Env::build_endpoint()` | `Env`構造体を直接構築してテスト | なし |
| `Env::should_use_path_style()` | `Env`構造体を直接構築してテスト | なし |
| `Env::get_region()` | `Env`構造体を直接構築してテスト | なし |
| `apply_s3_config()` | `aws_sdk_s3::config::Builder::new()`から空のビルダーを作成し、設定適用後の状態を検証 | なし |
| `build_s3_client()` | 統合テストで検証 | あり（SDK設定ロード、認証） |

**`apply_s3_config()` のテスト例**:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_env(
        server_address: Option<&str>,
        insecure: bool,
        force_path_style: Option<bool>,
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
            aws_region: None,
            aws_force_path_style: force_path_style,
        }
    }

    #[test]
    fn test_apply_s3_config_minio() {
        let env = create_test_env(Some("localhost:9000"), true, None);
        let builder = aws_sdk_s3::config::Builder::new();

        let result = apply_s3_config(builder, &env);
        assert!(result.is_ok());

        let config = result.unwrap().build();
        // endpoint_urlとforce_path_styleが正しく設定されていることを検証
        // （aws_sdk_s3::config::Configの内部状態を直接検証するAPIは限定的なため、
        //   実際のテストではビルダーの戻り値の型やエラーの有無で検証する）
    }

    #[test]
    fn test_apply_s3_config_aws_s3() {
        let env = create_test_env(None, false, None);
        let builder = aws_sdk_s3::config::Builder::new();

        let result = apply_s3_config(builder, &env);
        assert!(result.is_ok());
        // AWS S3の場合はエンドポイント未設定、virtual-hosted style
    }
}
```

### テストカバレッジ(ユニットテスト)

- `src/env.rs` のカバレッジ 80%以上を目標
- `src/s3_client.rs` のカバレッジ 80%以上を目標（`apply_s3_config()` を中心にテスト）

### 静的型付け

- Rustの型システムにより、設定値の型安全性を保証
- `Option<bool>` による3値（None/Some(true)/Some(false)）の明確な区別
- `Option<String>` によるオプショナル設定の明示

## 11. References (参考資料)

- [AWS SDK for Rust - credential providers](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/credproviders.html)
- [AWS SDK for Rust - エンドポイント設定](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/endpoints.html)
- [AWS SDK for Rust - HTTPクライアント設定](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/http.html)
- [Assume role credential provider - AWS SDKs and Tools](https://docs.aws.amazon.com/sdkref/latest/guide/feature-assume-role-credentials.html)
- [Login credentials provider - AWS SDKs and Tools](https://docs.aws.amazon.com/sdkref/latest/guide/feature-login-credentials.html)
- [IAM Identity Center credential provider - AWS SDKs and Tools](https://docs.aws.amazon.com/sdkref/latest/guide/feature-sso-credentials.html)
- [GitLab Runner S3 Cache設定](https://docs.gitlab.com/runner/configuration/advanced-configuration/#the-runnerscaches3-section)
- [Amazon S3 path-style 廃止について](https://aws.amazon.com/jp/blogs/news/amazon-s3-path-deprecation-plan-the-rest-of-the-story/)
- [aws-sdk-rust Virtual Hosted Style Support](https://github.com/awslabs/aws-sdk-rust/discussions/485)
- [Using AWS SDK S3 with MinIO (Rust Users Forum)](https://users.rust-lang.org/t/using-aws-sdk-s3-with-minio/99274)

## 12. Work Log (作業ログ)

### 2025-12-31

- 初版作成
- MinIO / AWS S3 両対応の設計を策定
- `aws_mode` フラグは不採用とし、`aws_server_address` のドメイン判定で対応する方針を決定
- AWS Profile (`CAFCE_AWS_PROFILE`) 対応を追加
- 環境変数によるAssumeRole (`CAFCE_AWS_ROLE_ARN`) 対応を追加
- SDK credential provider chain への対応を追加（`aws_access_key`/`aws_secret_key` を Option型に変更）
- `aws_server_address` を Option型に変更（省略時はSDKデフォルト）
- `aws login` コマンド (Login credentials provider) 対応を確認・ドキュメント化
- `aws sso login` コマンド (IAM Identity Center SSO) 対応を確認・ドキュメント化
- `url` crateを使用したエンドポイント生成ロジックに変更（IPv6対応、入力検証の改善）

### 2026-01-01

- S3クライアント生成ロジックを `apply_s3_config()` と `build_s3_client()` に分離
  - `apply_s3_config()`: エンドポイント・Path-style設定を適用する純粋関数（ネットワーク通信なし）
  - `build_s3_client()`: SDK設定ロード・認証・クライアント生成を統括するasync関数
- ユニットテストの設計方針を追記（認証処理を除くテストをネットワーク通信なしで実行可能）
- `apply_s3_config()` のテスト例を追加
