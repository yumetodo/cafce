# AWS統合テスト設計ドキュメント

## 1. Overview (概要)

本ドキュメントは、cafceプロジェクトにおけるAWS S3接続機能の統合テスト設計を記述したものである。

AWS側との実際の通信を伴う統合テストについて、認証方式ごとの自動化可能性を分析し、GitHub ActionsおよびローカルPCでの実行方法を定義する。

## 2. Context (背景)

- `doc/20251231_s3_connect_design.md` にて、複数の認証方式（静的クレデンシャル、AssumeRole、AWS Profile、SDK provider chain、aws login、aws sso login）への対応を設計した
- S3のList/Put/Get/Delete操作の完全なテストはMinIO（ローカル）で実施する
- AWSとの実通信テストは**認証 → List操作の確認のみ**に限定し、テストの複雑さを軽減する
- 一部の認証方式（aws login、aws sso login）はブラウザ認証が必須のため、自動化対象外とする

## 3. Scope (範囲)

### テスト対象

| 認証方式 | CI自動化 | ローカル実行 | 備考 |
|----------|----------|--------------|------|
| 静的クレデンシャル | ✅ | ✅ | IAMユーザーのアクセスキー |
| 環境変数AssumeRole | ✅ | ✅ | 静的キー + Role ARN |
| AWS Profile | ✅ | ✅ | ~/.aws/config のプロファイル |
| SDK provider chain (OIDC) | ✅ | ❌ | GitHub ActionsのOIDC連携 |
| `aws login` | ❌ | ✅（手動） | ブラウザ認証必須 |
| `aws sso login` | ❌ | ✅（手動） | ブラウザ認証必須 |

### テスト内容

- 各認証方式でS3クライアントを構築
- `list_objects_v2` APIを実行し、認証・接続が成功することを確認

## 4. Goal (目標)

- 自動化可能な認証パターン（4パターン）をCI/ローカルの両方でテスト可能にする
- 認証情報をGitリポジトリにコミットしない安全な設計
- JUnit/xUnit形式のレポート出力に対応（各パターンが独立したテストケースとして表示）
- 1コマンドで複数パターンをテスト可能

## 5. Non-Goal (目標外)

- S3のPut/Get/Delete操作のテスト（MinIOテストでカバー）
- `aws login` / `aws sso login` の自動化
- AWSリソースのセットアップ手順の詳細（事前条件のみ記載）

## 6. Solution / Technical Architecture (解決策 / 技術アーキテクチャ)

### 6.1 テストフレームワーク

`rstest` crateを使用し、各認証パターンを独立したテストケースとして実装する。

```toml
# Cargo.toml
[dev-dependencies]
rstest = "0.18"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
temp_env = "0.3"
toml = "0.8"
```

### 6.2 ファイル構成

```
test/
├── aws_integration/
│   ├── config.toml          # 本番設定（.gitignore対象）
│   └── config.toml.sample   # サンプル（コミット対象）
tests/
└── aws_integration.rs       # テストコード（コミット対象）
.gitignore                   # config.toml を除外
```

### 6.3 設定ファイル形式

#### `test/aws_integration/config.toml.sample`（コミット対象）

```toml
# AWS統合テスト設定ファイルのサンプル
# このファイルを config.toml にコピーして、実際の値を設定してください
#
# cp test/aws_integration/config.toml.sample test/aws_integration/config.toml

# テスト対象のS3バケット
test_bucket = "your-test-bucket"
test_region = "ap-northeast-1"

# =============================================================================
# 認証パターン設定
# enabled = true のパターンのみテストが実行されます
# =============================================================================

# パターン1: 静的クレデンシャル
[patterns.static_credentials]
enabled = false
aws_access_key = "AKIAIOSFODNN7EXAMPLE"
aws_secret_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"

# パターン2: AssumeRole（環境変数指定）
[patterns.assume_role]
enabled = false
aws_access_key = "AKIAIOSFODNN7EXAMPLE"
aws_secret_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
aws_role_arn = "arn:aws:iam::123456789012:role/your-role"

# パターン3: AWS Profile
[patterns.profile]
enabled = false
aws_profile = "your-profile-name"

# パターン4: SDK Provider Chain（OIDC等）
# CI環境でのみ有効化推奨
[patterns.sdk_provider_chain]
enabled = false
# 環境変数やIMDSから自動検出
```

### 6.4 テストコード構造

```rust
// tests/aws_integration.rs
use rstest::{fixture, rstest};

#[derive(Debug, Deserialize)]
struct TestConfig {
    test_bucket: String,
    test_region: String,
    patterns: Patterns,
}

#[derive(Debug, Deserialize)]
struct Patterns {
    static_credentials: Option<PatternConfig>,
    assume_role: Option<PatternConfig>,
    profile: Option<PatternConfig>,
    sdk_provider_chain: Option<PatternConfig>,
}

#[fixture]
fn test_config() -> Option<TestConfig> {
    // test/aws_integration/config.toml を読み込み
}

#[rstest]
#[tokio::test]
async fn test_static_credentials(test_config: Option<TestConfig>) {
    // 設定ファイルから認証情報を取得し、List操作をテスト
}

#[rstest]
#[tokio::test]
async fn test_assume_role(test_config: Option<TestConfig>) { ... }

#[rstest]
#[tokio::test]
async fn test_profile(test_config: Option<TestConfig>) { ... }

#[rstest]
#[tokio::test]
async fn test_sdk_provider_chain(test_config: Option<TestConfig>) { ... }
```

### 6.5 認証パターン別の動作

#### パターン1: 静的クレデンシャル

```mermaid
flowchart LR
    CONFIG[config.toml] --> TEST[テストコード]
    TEST -->|CAFCE_AWS_ACCESS_KEY<br/>CAFCE_AWS_SECRET_KEY| ENV[環境変数設定]
    ENV --> CLIENT[S3クライアント構築]
    CLIENT -->|ListObjectsV2| S3[AWS S3]
```

#### パターン2: AssumeRole

```mermaid
flowchart LR
    CONFIG[config.toml] --> TEST[テストコード]
    TEST -->|CAFCE_AWS_ACCESS_KEY<br/>CAFCE_AWS_SECRET_KEY<br/>CAFCE_AWS_ROLE_ARN| ENV[環境変数設定]
    ENV --> STS[STSでAssumeRole]
    STS --> CLIENT[S3クライアント構築]
    CLIENT -->|ListObjectsV2| S3[AWS S3]
```

#### パターン3: AWS Profile

```mermaid
flowchart LR
    CONFIG[config.toml] --> TEST[テストコード]
    TEST -->|CAFCE_AWS_PROFILE| ENV[環境変数設定]
    ENV --> AWS_CONFIG[~/.aws/config<br/>~/.aws/credentials]
    AWS_CONFIG --> CLIENT[S3クライアント構築]
    CLIENT -->|ListObjectsV2| S3[AWS S3]
```

#### パターン4: SDK Provider Chain (OIDC)

```mermaid
flowchart LR
    GHA[GitHub Actions] -->|OIDC Token| AWS[AWS STS]
    AWS -->|AssumeRoleWithWebIdentity| CREDS[一時クレデンシャル]
    CREDS --> ENV[AWS_* 環境変数]
    ENV --> CLIENT[S3クライアント構築]
    CLIENT -->|ListObjectsV2| S3[AWS S3]
```

## 7. 実行方法

### 7.1 ローカル実行

#### 事前準備

1. `test/aws_integration/config.toml.sample` を `config.toml` にコピー
2. 使用する認証パターンの `enabled = true` に設定
3. 認証情報を記入

#### 実行

```bash
# 全パターン実行（enabled=true のもののみ）
cargo test --test aws_integration

# 特定パターンのみ実行
cargo test --test aws_integration -- test_static_credentials
cargo test --test aws_integration -- test_profile

# 詳細出力
cargo test --test aws_integration -- --nocapture
```

### 7.2 GitHub Actions実行

#### ワークフロー設定

```yaml
# .github/workflows/aws-integration-test.yml
name: AWS Integration Test

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
  workflow_dispatch:

jobs:
  aws-integration:
    runs-on: ubuntu-latest
    permissions:
      id-token: write   # OIDC連携に必要
      contents: read
    
    steps:
      - uses: actions/checkout@v4
      
      - name: Setup Rust
        uses: dtolnay/rust-action@stable
      
      - name: Configure AWS Credentials (OIDC)
        uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-assume: ${{ secrets.AWS_OIDC_ROLE_ARN }}
          aws-region: ap-northeast-1
      
      - name: Generate test config
        run: |
          mkdir -p test/aws_integration
          cat > test/aws_integration/config.toml << 'EOF'
          test_bucket = "${{ secrets.AWS_TEST_BUCKET }}"
          test_region = "ap-northeast-1"
          
          [patterns.sdk_provider_chain]
          enabled = true
          EOF
      
      - name: Run integration tests
        run: cargo test --test aws_integration
      
      - name: Generate JUnit report
        if: always()
        run: |
          cargo install cargo-nextest --locked
          cargo nextest run --test aws_integration --profile ci || true
      
      - name: Upload test results
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: test-results
          path: target/nextest/ci/junit.xml
```

#### GitHub Secrets設定

| Secret名 | 説明 |
|----------|------|
| `AWS_OIDC_ROLE_ARN` | OIDC連携用IAMロールのARN |
| `AWS_TEST_BUCKET` | テスト用S3バケット名 |

## 8. AWS事前条件

### 8.1 必要なAWSリソース一覧

| リソース種別 | 名前（例） | 用途 | 必要なパターン |
|-------------|-----------|------|----------------|
| S3バケット | `cafce-ci-test-bucket` | テスト用（空でOK） | 全パターン |
| IAMユーザー | `cafce-ci-user` | 静的クレデンシャル用 | パターン1, 2, 3 |
| IAMロール | `cafce-ci-assume-role` | AssumeRole先 | パターン2 |
| IAMロール | `cafce-ci-oidc-role` | OIDC連携用 | パターン4 |
| OIDC Provider | GitHub Actions | OIDC連携用 | パターン4 |

### 8.2 IAMポリシー要件

#### S3アクセスポリシー（全ロール/ユーザー共通）

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "AllowListTestBucket",
      "Effect": "Allow",
      "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::cafce-ci-test-bucket"
    }
  ]
}
```

#### AssumeRole用IAMユーザーポリシー

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "AllowAssumeTestRole",
      "Effect": "Allow",
      "Action": "sts:AssumeRole",
      "Resource": "arn:aws:iam::ACCOUNT_ID:role/cafce-ci-assume-role"
    }
  ]
}
```

### 8.3 IAMロール信頼ポリシー要件

#### cafce-ci-assume-role（パターン2用）

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": {
        "AWS": "arn:aws:iam::ACCOUNT_ID:user/cafce-ci-user"
      },
      "Action": "sts:AssumeRole"
    }
  ]
}
```

#### cafce-ci-oidc-role（パターン4用）

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": {
        "Federated": "arn:aws:iam::ACCOUNT_ID:oidc-provider/token.actions.githubusercontent.com"
      },
      "Action": "sts:AssumeRoleWithWebIdentity",
      "Condition": {
        "StringEquals": {
          "token.actions.githubusercontent.com:aud": "sts.amazonaws.com"
        },
        "StringLike": {
          "token.actions.githubusercontent.com:sub": "repo:yumetodo/cafce:*"
        }
      }
    }
  ]
}
```

### 8.4 OIDC Provider設定

| 項目 | 値 |
|------|------|
| Provider URL | `https://token.actions.githubusercontent.com` |
| Audience | `sts.amazonaws.com` |

### 8.5 事前条件チェックリスト

#### 全パターン共通
- [ ] テスト用S3バケットが存在する
- [ ] バケットは空でよい（ListObjectsV2のみ実行）

#### パターン1: 静的クレデンシャル
- [ ] IAMユーザーが作成されている
- [ ] アクセスキーが発行されている
- [ ] S3 ListBucket権限がアタッチされている

#### パターン2: AssumeRole
- [ ] パターン1の条件を満たす
- [ ] AssumeRole先のIAMロールが作成されている
- [ ] ロールにS3 ListBucket権限がアタッチされている
- [ ] ロールの信頼ポリシーでIAMユーザーが許可されている
- [ ] IAMユーザーにsts:AssumeRole権限がアタッチされている

#### パターン3: AWS Profile
- [ ] ローカル: `~/.aws/config` および `~/.aws/credentials` が設定されている
- [ ] CI: パターン1または4の認証情報で `~/.aws/config` を動的生成

#### パターン4: SDK Provider Chain (OIDC)
- [ ] GitHub OIDC Providerが登録されている
- [ ] OIDC連携用IAMロールが作成されている
- [ ] ロールにS3 ListBucket権限がアタッチされている
- [ ] ロールの信頼ポリシーでGitHubリポジトリが許可されている

## 9. JUnitレポート出力

`cargo-nextest` を使用してJUnit形式のレポートを出力する。

### nextest設定

```toml
# .config/nextest.toml
[profile.ci]
junit = { path = "target/nextest/ci/junit.xml" }
```

### 出力例

```xml
<testsuite name="aws_integration" tests="4" failures="0" skipped="1">
  <testcase name="test_static_credentials" time="1.23"/>
  <testcase name="test_assume_role" time="1.56"/>
  <testcase name="test_profile" time="1.12"/>
  <testcase name="test_sdk_provider_chain" time="0.01">
    <skipped message="config.toml not found or pattern disabled"/>
  </testcase>
</testsuite>
```

## 10. References (参考資料)

- [rstest - GitHub](https://github.com/la10736/rstest)
- [cargo-nextest - JUnit support](https://nexte.st/book/junit.html)
- [GitHub Actions OIDC - AWS](https://docs.github.com/en/actions/deployment/security-hardening-your-deployments/configuring-openid-connect-in-amazon-web-services)
- [aws-actions/configure-aws-credentials](https://github.com/aws-actions/configure-aws-credentials)
- `doc/20251231_s3_connect_design.md` - S3接続設計ドキュメント

## 11. Work Log (作業ログ)

### 2026-01-01

- 初版作成
- 認証パターン別の自動化可能性を分析
- rstest + 設定ファイル分離方式を採用
- GitHub Actions / ローカル実行の両方に対応する設計を策定
- AWS事前条件を明確化
