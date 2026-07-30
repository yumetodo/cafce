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

    /// 内容ハッシュとして形式が妥当な 2 値
    ///
    /// 「一致 / 不一致」を作り分けるだけなので値そのものに意味は無いが、
    /// cache_metadata の形式検証（小文字 16 進 64 文字）を通る必要がある。
    const HASH_A: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const HASH_B: &str = "6ae8a75555209fd6c44157c0aed8016e763ff435a19cf186f76863140143ff72";

    mod should_retry_without_checksum_tests {
        use super::*;

        #[test]
        fn test_bad_digest_is_not_retried() {
            // Arrange: サーバが機能を理解したうえで内容の不一致を検出した結果。
            // 400 番台なので、ステータスだけを見る実装だと非対応と誤認する
            let status = Some(400);
            let error_code = Some("BadDigest");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert: 本物の破損を意味するので再試行しない。チェックサム無しで送り直すと
            // 壊れたキャッシュをサーバの検証をすり抜けて置いてしまう
            assert!(!retry);
        }

        #[test]
        fn test_access_denied_is_not_retried() {
            // Arrange: 権限不足。これも 400 番台に含まれる
            let status = Some(403);
            let error_code = Some("AccessDenied");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert: チェックサムを外しても通らないので、再試行は無駄なリクエストになるだけ。
            // 権限不足という本当の原因をエラーとして見せる
            assert!(!retry);
        }

        #[test]
        fn test_invalid_request_is_retried() {
            // Arrange: チェックサム非対応のサーバが返しうるコード。
            // 実際にどれが返るかはサーバ実装依存なので広めに拾う
            let status = Some(400);
            let error_code = Some("InvalidRequest");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert: ここだけがフォールバックする経路。判定を誤って不要に
            // フォールバックしても metadata の内容ハッシュ検証が残るため安全側に倒れる
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
            // Arrange: 5xx は非対応ではなくサーバ側の一時障害
            let status = Some(500);
            let error_code = Some("InternalError");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert: チェックサムを外して再試行すると、たまたま成功したときに
            // 「このサーバは非対応」という誤った学習をしたのと同じ結果になる。
            // 一時障害の再送は SDK のリトライに任せる
            assert!(!retry);
        }

        #[test]
        fn test_network_error_without_response_is_not_retried() {
            // Arrange: HTTP レスポンスに到達しなかった場合（接続失敗・タイムアウト）。
            // SdkError からステータスもコードも取れない
            let status: Option<u16> = None;
            let error_code: Option<&str> = None;

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert: 情報が無いときは非対応と決めつけない（既定で安全側）
            assert!(!retry);
        }

        #[test]
        fn test_unknown_client_error_code_is_not_retried() {
            // Arrange: 400 番台だがチェックサムと無関係な失敗（バケット名の間違い等）
            let status = Some(404);
            let error_code = Some("NoSuchBucket");

            // Act
            let retry = should_retry_without_checksum(status, error_code);

            // Assert: 既知のコードだけを許可リストで拾う。「400 番台なら全部再試行」に
            // すると、設定ミスの本当の原因が 2 回目の失敗で上書きされて分かりにくくなる
            assert!(!retry);
        }

        #[test]
        fn test_bad_digest_without_status_is_not_retried() {
            // Arrange: BadDigest の判定をステータス検査より前に置いていることの確認。
            // 順序が逆だと、ステータスが取れないケースで BadDigest の意図が失われる
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
            // Arrange: head_object が 404（そのキーに何も置かれていない初回）
            let existing_metadata = None;

            // Act
            let needed = needs_upload(existing_metadata, HASH_A);

            // Assert: 比較対象が無いのでアップロードする
            assert!(needed);
        }

        #[test]
        fn test_matching_hash_skips_upload() {
            // Arrange: 既存オブジェクトと、これから送ろうとしている内容のハッシュが同じ。
            // literal String のキー（`cache-${CI_COMMIT_REF_SLUG}` 等）で
            // 同じブランチのジョブが何度も走る典型ケース
            let existing_metadata = crate::cache_metadata::build_metadata(HASH_A);

            // Act
            let needed = needs_upload(Some(&existing_metadata), HASH_A);

            // Assert: この 1 件だけが省略される経路。並列度の高い CI では
            // 同一内容のアップロードが人数分走って帯域を食うため、ここが効く
            assert!(!needed);
        }

        #[test]
        fn test_mismatching_hash_needs_upload() {
            // Arrange: キーは同じだが中身が変わった（ブランチは同じでコードが進んだ）
            let existing_metadata = crate::cache_metadata::build_metadata(HASH_A);

            // Act
            let needed = needs_upload(Some(&existing_metadata), HASH_B);

            // Assert: 上書きする
            assert!(needed);
        }

        #[test]
        fn test_absent_metadata_needs_upload() {
            // Arrange: オブジェクトはあるが cafce の metadata が無い
            // （`aws s3 cp` で手置きした、あるいは user metadata を保持しないサーバ）
            let existing_metadata = std::collections::HashMap::new();

            // Act
            let needed = needs_upload(Some(&existing_metadata), HASH_A);

            // Assert: 比較できないので上書きする。metadata が返らないサーバでは
            // 毎回アップロードになるが、正しさは保たれる（省略が効かないだけ）
            assert!(needed);
        }

        #[test]
        fn test_unparsable_metadata_needs_upload() {
            // Arrange: 未知のスキーマ版数。新しい cafce が先に走った状況で、
            // restore ならエラーにする入力
            let mut existing_metadata = crate::cache_metadata::build_metadata(HASH_A);
            existing_metadata.insert(
                crate::cache_metadata::KEY_SCHEMA_VERSION.to_string(),
                "999".to_string(),
            );

            // Act
            let needed = needs_upload(Some(&existing_metadata), HASH_A);

            // Assert: store では読み取りと違ってエラーにしない。同一キーへの store は
            // last-write-wins が前提であり、新しい cafce が先に走ったというだけで
            // CI を落とす方が害が大きいと判断した（設計doc の Non-Goal と対応）
            assert!(needed);
        }
    }
}
