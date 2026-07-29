//! `store` サブコマンドのロジック
//!
//! 対象解決 → アーカイブ生成 → 既存オブジェクトのハッシュ照合 → 必要時のみ `put_object`。
//! S3 とのやり取り（`s3_client` / `probe::build_object_key`）と、ローカルのアーカイブ操作
//! （`path_matcher` / `archive`）を組み合わせる薄い層に留める。

/// チェックサム非対応に起因すると判断するエラーコード
///
/// 実際にどのコードが返るかはサーバ実装依存なので、広めに拾う。判定を誤って
/// フォールバックしても、metadata の内容ハッシュによる検証が残るため安全側に倒れる。
const CHECKSUM_UNSUPPORTED_ERROR_CODES: &[&str] =
    &["InvalidRequest", "InvalidArgument", "NotImplemented"];

/// `put_object` の失敗を、チェックサム無しで 1 回だけ再試行すべきか判定する
///
/// S3 呼び出しから切り離して単体テストできるよう、HTTP ステータスとエラーコードだけを受け取る。
///
/// - `BadDigest` は再試行しない。サーバが機能を理解したうえで内容の不一致を検出した結果であり、
///   本物の破損を意味する
/// - 403 やネットワークエラーも再試行しない
/// - 400 系でチェックサム関連のエラーコードが返った場合のみ、非対応とみなして再試行する
fn should_retry_without_checksum(status: Option<u16>, error_code: Option<&str>) -> bool {
    if error_code == Some("BadDigest") {
        return false;
    }

    if !status.is_some_and(|status| (400..500).contains(&status)) {
        return false;
    }

    error_code.is_some_and(|code| CHECKSUM_UNSUPPORTED_ERROR_CODES.contains(&code))
}

/// 既存オブジェクトの user metadata と照合し、アップロードが必要かを判定する
///
/// - オブジェクトが存在しない（404）→ 必要
/// - cafce の metadata が無い（cafce 以外が置いた、`aws s3 cp` で手置きした等）→ 必要
/// - metadata を解釈できない（未知のスキーマ版数等）→ 必要。同一キーへの `store` は
///   last-write-wins が前提であり、CI を止めるより上書きして進める方が実用的である
/// - 内容ハッシュが一致 → 不要
fn needs_upload(
    existing_metadata: Option<&std::collections::HashMap<String, String>>,
    content_sha256: &str,
) -> bool {
    let existing_metadata = match existing_metadata {
        Some(existing_metadata) => existing_metadata,
        None => return true,
    };

    match crate::cache_metadata::parse_metadata(Some(existing_metadata)) {
        Ok(Some(parsed)) => parsed.content_sha256 != content_sha256,
        Ok(None) => {
            log::warn!("既存オブジェクトに cafce の metadata がありません。上書きします");
            true
        }
        Err(e) => {
            log::warn!("既存オブジェクトの metadata を解釈できませんでした（上書きします）: {e}");
            true
        }
    }
}

/// アーカイブを `put_object` する
///
/// `with_checksum` が true のとき、`checksum_algorithm` と**事前計算した** `checksum_sha256` の
/// 両方を指定する。値を渡さず算法だけ指定すると、ストリーミングボディでは `aws-chunked`
/// エンコーディングのトレーラとして送られ、S3 互換サーバでの対応差に当たるためである
/// （設計doc 6.7 / 代替案11）。
async fn put_archive(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    object_key: &str,
    archive: &crate::archive::BuiltArchive,
    with_checksum: bool,
) -> anyhow::Result<
    Result<(), aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::put_object::PutObjectError>>,
> {
    use anyhow::Context as _;

    // ByteStream は送信で消費されるため、再試行のたびに作り直す
    let body = aws_sdk_s3::primitives::ByteStream::from_path(archive.temp_file.path())
        .await
        .with_context(|| {
            format!(
                "アーカイブの読み込みに失敗しました: {}",
                archive.temp_file.path().display()
            )
        })?;

    let mut request = client
        .put_object()
        .bucket(bucket)
        .key(object_key)
        .body(body)
        .set_metadata(Some(crate::cache_metadata::build_metadata(
            &archive.content_sha256,
        )));

    if with_checksum {
        let checksum = crate::cache_metadata::to_checksum_base64(&archive.object_sha256);
        log::debug!("x-amz-checksum-sha256 を付けて PutObject します: {checksum}");
        request = request
            .checksum_algorithm(aws_sdk_s3::types::ChecksumAlgorithm::Sha256)
            .checksum_sha256(checksum);
    }

    Ok(request.send().await.map(|_| ()))
}

/// `SdkError` から HTTP ステータスとエラーコードを取り出す
fn describe_sdk_error(
    error: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::put_object::PutObjectError>,
) -> (Option<u16>, Option<String>) {
    use aws_sdk_s3::error::ProvideErrorMetadata as _;

    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    let error_code = error.code().map(str::to_string);
    (status, error_code)
}

/// アーカイブをアップロードする（必要ならチェックサム無しで 1 回だけ再試行する）
async fn upload_archive(
    client: &aws_sdk_s3::Client,
    env: &crate::env::Env,
    object_key: &str,
    archive: &crate::archive::BuiltArchive,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let checksum_mode = env.s3_checksum();
    let with_checksum = checksum_mode != crate::env::S3ChecksumMode::Off;

    let first_attempt =
        put_archive(client, env.bucket(), object_key, archive, with_checksum).await?;
    let error = match first_attempt {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    let (status, error_code) = describe_sdk_error(&error);
    let can_fall_back = checksum_mode == crate::env::S3ChecksumMode::Auto
        && with_checksum
        && should_retry_without_checksum(status, error_code.as_deref());

    if !can_fall_back {
        return Err(anyhow::anyhow!(error)).with_context(|| {
            format!(
                "S3 PutObject に失敗しました (bucket={}, key={object_key})\n\
                 BadDigest の場合はアップロード内容が転送中に壊れています",
                env.bucket()
            )
        });
    }

    log::warn!(
        "S3 がフレキシブルチェックサムに対応していない可能性があります \
         (status={status:?}, code={error_code:?})。チェックサム無しで再試行します"
    );

    put_archive(client, env.bucket(), object_key, archive, false)
        .await?
        .map_err(anyhow::Error::from)
        .with_context(|| {
            format!(
                "チェックサム無しでの S3 PutObject にも失敗しました (bucket={}, key={object_key})",
                env.bucket()
            )
        })
}

/// `store` サブコマンドの本体
///
/// 書き込み先は常に primary key である（`fallback_keys` は読み出し専用の概念であり、
/// そこへ書くと他ブランチのキャッシュを踏み荒らすことになる）。
///
/// 戻り値はアップロードしたかどうか。内容が同一で省略した場合は `false` を返す。
pub async fn store(
    setting: &crate::setting::Setting,
    env: &crate::env::Env,
    client: &aws_sdk_s3::Client,
    base_path: &std::path::Path,
) -> anyhow::Result<bool> {
    use anyhow::Context as _;

    let primary_key = setting
        .resolve_primary_key(base_path)
        .context("primary キーの計算に失敗しました")?;
    let object_key =
        crate::probe::build_object_key(env.s3_prefix(), &setting.project, &primary_key);

    let entries = crate::path_matcher::resolve_paths(&setting.paths, base_path)
        .context("キャッシュ対象の解決に失敗しました")?;
    let archive = crate::archive::create_archive(&entries, base_path)
        .context("アーカイブの生成に失敗しました")?;

    log::debug!("HeadObject を発行します: {object_key}");
    let existing_metadata =
        crate::probe::head_object_metadata(client, env.bucket(), &object_key).await?;

    if !needs_upload(existing_metadata.as_ref(), &archive.content_sha256) {
        log::info!("内容が同一のためアップロードを省略します: {object_key}");
        return Ok(false);
    }

    log::info!(
        "キャッシュをアップロードします: {object_key} ({} bytes)",
        archive.size
    );
    upload_archive(client, env, &object_key, &archive).await?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const HASH_B: &str = "6ae8a75555209fd6c44157c0aed8016e763ff435a19cf186f76863140143ff72";

    mod should_retry_without_checksum_tests {
        use super::*;

        #[test]
        fn test_bad_digest_is_not_retried() {
            // Arrange: サーバが機能を理解したうえで内容の不一致を検出した結果
            let status = Some(400);
            let error_code = Some("BadDigest");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert
            assert!(!retry);
        }

        #[test]
        fn test_access_denied_is_not_retried() {
            // Arrange
            let status = Some(403);
            let error_code = Some("AccessDenied");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert
            assert!(!retry);
        }

        #[test]
        fn test_invalid_request_is_retried() {
            // Arrange
            let status = Some(400);
            let error_code = Some("InvalidRequest");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert
            assert!(retry);
        }

        #[test]
        fn test_invalid_argument_is_retried() {
            // Arrange
            let status = Some(400);
            let error_code = Some("InvalidArgument");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert
            assert!(retry);
        }

        #[test]
        fn test_not_implemented_is_retried() {
            // Arrange
            let status = Some(400);
            let error_code = Some("NotImplemented");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert
            assert!(retry);
        }

        #[test]
        fn test_server_error_is_not_retried() {
            // Arrange: 5xx は非対応ではなくサーバ側の障害
            let status = Some(500);
            let error_code = Some("InternalError");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert
            assert!(!retry);
        }

        #[test]
        fn test_network_error_without_response_is_not_retried() {
            // Arrange: レスポンスが無い（接続失敗・タイムアウト）
            let status: Option<u16> = None;
            let error_code: Option<&str> = None;

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert
            assert!(!retry);
        }

        #[test]
        fn test_unknown_client_error_code_is_not_retried() {
            // Arrange: 400 系でもチェックサムと無関係なコードは再試行しない
            let status = Some(404);
            let error_code = Some("NoSuchBucket");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert
            assert!(!retry);
        }

        #[test]
        fn test_bad_digest_without_status_is_not_retried() {
            // Arrange: BadDigest はステータスによらず再試行しない
            let status: Option<u16> = None;
            let error_code = Some("BadDigest");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert
            assert!(!retry);
        }
    }

    mod needs_upload_tests {
        use super::*;

        #[test]
        fn test_missing_object_needs_upload() {
            // Arrange: head_object が 404
            let existing_metadata = None;

            // Act
            let needed = needs_upload(existing_metadata, HASH_A);

            // Assert
            assert!(needed);
        }

        #[test]
        fn test_matching_hash_skips_upload() {
            // Arrange
            let existing_metadata = crate::cache_metadata::build_metadata(HASH_A);

            // Act
            let needed = needs_upload(Some(&existing_metadata), HASH_A);

            // Assert
            assert!(!needed);
        }

        #[test]
        fn test_mismatching_hash_needs_upload() {
            // Arrange
            let existing_metadata = crate::cache_metadata::build_metadata(HASH_A);

            // Act
            let needed = needs_upload(Some(&existing_metadata), HASH_B);

            // Assert
            assert!(needed);
        }

        #[test]
        fn test_absent_metadata_needs_upload() {
            // Arrange: cafce 以外が置いたオブジェクト
            let existing_metadata = std::collections::HashMap::new();

            // Act
            let needed = needs_upload(Some(&existing_metadata), HASH_A);

            // Assert
            assert!(needed);
        }

        #[test]
        fn test_unparsable_metadata_needs_upload() {
            // Arrange: 未知のスキーマ版数（新しい cafce が置いた可能性がある）
            let mut existing_metadata = crate::cache_metadata::build_metadata(HASH_A);
            existing_metadata.insert(
                crate::cache_metadata::KEY_SCHEMA_VERSION.to_string(),
                "999".to_string(),
            );

            // Act: last-write-wins が前提なので CI を止めず上書きする
            let needed = needs_upload(Some(&existing_metadata), HASH_A);

            // Assert
            assert!(needed);
        }
    }
}
