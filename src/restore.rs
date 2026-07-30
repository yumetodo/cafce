//! `restore` サブコマンドのロジック
//!
//! キー候補の順序試行 → `get_object` → 展開 → 内容ハッシュ検証。
//!
//! 存在確認に `head_object` を挟まず `get_object` を直接使うのは、ヒット時のラウンドトリップを
//! 1 回減らせるうえ、必要な IAM 権限が変わらないためである（設計doc 6.9）。
//!
//! `paths` は参照しない。アーカイブに何が入っているかはアーカイブ自身が持つ情報であり、
//! 展開時に config を再解釈すると `store` 時と `restore` 時で `paths` が食い違ったときに
//! 挙動が読めなくなる。

/// `get_object` の失敗を cache miss として次の候補へ進めてよいか判定する
///
/// S3 呼び出しから切り離して単体テストできるよう、HTTP ステータスとエラーコードだけを受け取る。
/// 403 を miss として扱わないのは、silent な auth failure を恒常的な cache miss に
///見せかけないためである（`probe` と同じ判断軸）。
fn is_cache_miss(status: Option<u16>, error_code: Option<&str>) -> bool {
    error_code == Some("NoSuchKey") || status == Some(404)
}

/// `get_object` のボディを一時ファイルへ書き切る
///
/// ここで SDK が `x-amz-checksum-sha256` を検証する。ボディを読み切らないと検証は行われない。
/// 展開を始める前にこれを終えることで、転送破損を検出しても作業ディレクトリは一切汚れない。
/// zstd のフレームチェックサムではフレームをデコードし終えるまで判定できず、
/// その時点ではファイル木が既に書き換わっている（設計doc 6.7）。
async fn download_to_temp_file(
    mut body: aws_sdk_s3::primitives::ByteStream,
) -> anyhow::Result<tempfile::NamedTempFile> {
    use anyhow::Context as _;
    use std::io::Write as _;

    let mut temp_file = tempfile::NamedTempFile::new()
        .context("ダウンロード用の一時ファイルを作成できませんでした")?;

    let mut downloaded_size: u64 = 0;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context(
            "S3 からのボディ受信に失敗しました\n\
             チェックサム不一致の場合は転送中にオブジェクトが壊れています",
        )?;
        temp_file
            .write_all(&chunk)
            .context("一時ファイルへの書き出しに失敗しました")?;
        downloaded_size += chunk.len() as u64;
    }
    temp_file
        .flush()
        .context("一時ファイルへの書き出しに失敗しました")?;

    log::debug!("キャッシュをダウンロードしました: {downloaded_size} bytes");

    Ok(temp_file)
}

/// 1 つのキー候補について `get_object` を試みる
///
/// - ヒット → `Ok(Some(出力))`
/// - `NoSuchKey` / 404 → `Ok(None)`（次の候補へ）
/// - 403 その他 → `Err`（後続の候補は試さない）
async fn get_object_if_exists(
    client: &aws_sdk_s3::Client,
    env: &crate::env::Env,
    object_key: &str,
) -> anyhow::Result<Option<aws_sdk_s3::operation::get_object::GetObjectOutput>> {
    use anyhow::Context as _;
    use aws_sdk_s3::error::ProvideErrorMetadata as _;

    let mut request = client.get_object().bucket(env.bucket()).key(object_key);

    // SDK の既定（ResponseChecksumValidation::WhenSupported）でも同等の検証は働くが、
    // AWS_RESPONSE_CHECKSUM_VALIDATION 等で無効化された環境でも検証が外れないよう明示する
    if env.s3_checksum() != crate::env::S3ChecksumMode::Off {
        request = request.checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled);
    }

    let error = match request.send().await {
        Ok(output) => return Ok(Some(output)),
        Err(error) => error,
    };

    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    let error_code = error.code().map(str::to_string);

    if is_cache_miss(status, error_code.as_deref()) {
        return Ok(None);
    }

    Err(anyhow::anyhow!(error)).with_context(|| {
        format!(
            "S3 GetObject に失敗しました (bucket={}, key={object_key})\n\
             403 の場合は s3:GetObject 権限を確認してください",
            env.bucket()
        )
    })
}

/// ヒットしたオブジェクトを展開し、内容ハッシュを検証する
async fn extract_and_verify(
    output: aws_sdk_s3::operation::get_object::GetObjectOutput,
    env: &crate::env::Env,
    base_path: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    // スキーマ版数の検証はダウンロード前に済ませる（未知の版数なら転送する意味が無い）
    let metadata = crate::cache_metadata::parse_metadata(output.metadata())
        .context("キャッシュの metadata を解釈できませんでした")?;

    if env.s3_checksum() == crate::env::S3ChecksumMode::Required
        && output.checksum_sha256().is_none()
    {
        return Err(crate::error::CacheMetadataError::ChecksumMissingButRequired.into());
    }

    let temp_file = download_to_temp_file(output.body).await?;

    let content_sha256 = crate::archive::extract_archive(temp_file.path(), base_path)
        .context("キャッシュの展開に失敗しました")?;

    match metadata {
        Some(metadata) => {
            crate::cache_metadata::verify_content_sha256(
                &metadata.content_sha256,
                &content_sha256,
            )?;
        }
        None => {
            // cafce 以外が置いたオブジェクト。検証する材料が無いので展開だけ行う
            log::warn!("cafce の metadata が無いため内容ハッシュ検証をスキップしました");
        }
    }

    Ok(())
}

/// `restore` サブコマンドの本体
///
/// primary key → `fallback_keys` の順に `get_object` を試み、最初にヒットしたオブジェクトを
/// カレントディレクトリへ展開する。戻り値は展開したかどうか（全 miss なら `false`）。
pub async fn restore(
    setting: &crate::setting::Setting,
    env: &crate::env::Env,
    client: &aws_sdk_s3::Client,
    base_path: &std::path::Path,
) -> anyhow::Result<bool> {
    // probe と同じ順序・同じ解決経路を通す（設計doc 6.9 の不変条件）
    let cache_keys = setting.resolve_key_candidates(base_path)?;

    for cache_key in &cache_keys {
        let object_key =
            crate::probe::build_object_key(env.s3_prefix(), &setting.project, cache_key);
        log::debug!("GetObject を発行します: {object_key}");

        let output = match get_object_if_exists(client, env, &object_key).await? {
            Some(output) => output,
            None => continue,
        };

        log::info!("キャッシュがヒットしました: {object_key}");
        extract_and_verify(output, env, base_path).await?;
        return Ok(true);
    }

    log::info!("全てのキー候補が miss しました: {cache_keys:?}");
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod is_cache_miss_tests {
        use super::*;

        #[test]
        fn test_no_such_key_is_miss() {
            // Arrange: AWS S3 が GetObject で返す典型的な cache miss
            let status = Some(404);
            let error_code = Some("NoSuchKey");

            // Act
            let miss = is_cache_miss(status, error_code);

            // Assert: miss なら次のキー候補へ進む
            assert!(miss);
        }

        #[test]
        fn test_bare_404_is_miss() {
            // Arrange: NoSuchKey を返さない S3 互換サーバを想定する。
            // 設計doc 6.9 は NoSuchKey しか挙げていなかったが、コードに依存しない
            // 404 判定も併せ持たせた
            let status = Some(404);
            let error_code = Some("NotFound");

            // Act
            let miss = is_cache_miss(status, error_code);

            // Assert: ここを取りこぼすと、キャッシュが無いだけで restore がエラー終了し
            // CI が落ちる（本来は false を返して full build へ進めばよい）
            assert!(miss);
        }

        #[test]
        fn test_access_denied_is_not_miss() {
            // Arrange: 権限不足。AWS S3 では s3:ListBucket が無いと、存在しないキーへの
            // アクセスが 404 ではなく 403 で返ることがある
            let status = Some(403);
            let error_code = Some("AccessDenied");

            // Act
            let miss = is_cache_miss(status, error_code);

            // Assert: miss として扱うと、権限設定を間違えている間ずっと
            // 「キャッシュが無い」ように見え、毎回フルビルドしていることに誰も気づかない。
            // probe と同じくフェイルファストにする
            assert!(!miss);
        }

        #[test]
        fn test_server_error_is_not_miss() {
            // Arrange: サーバ側の一時障害
            let status = Some(500);
            let error_code = Some("InternalError");

            // Act
            let miss = is_cache_miss(status, error_code);

            // Assert: 「無い」と「取れなかった」を混ぜない。混ぜると障害中に
            // 全キー候補を miss と判定して、あるはずのキャッシュを捨ててしまう
            assert!(!miss);
        }

        #[test]
        fn test_network_error_without_response_is_not_miss() {
            // Arrange: HTTP レスポンスに到達しなかった場合（接続失敗・タイムアウト）
            let status: Option<u16> = None;
            let error_code: Option<&str> = None;

            // Act
            let miss = is_cache_miss(status, error_code);

            // Assert: 情報が無いときは miss と決めつけない（既定で安全側）
            assert!(!miss);
        }
    }

    /// キー候補の順序試行の分岐を、S3 を介さずに確認する
    ///
    /// `restore` 本体は `get_object` と展開処理に密結合しているため、そのままでは
    /// S3 無しに呼べない。ここでは「候補を順に試し、最初のヒットで打ち切り、
    /// エラーなら即座に伝播する」という**ループの形**だけを同じ構造で書き写し、
    /// 分岐を網羅する（`probe` の同種のテストと同じ手法）。
    /// 本体との同期はコードレビューで担保し、実際の疎通は統合テストで確認する。
    mod key_candidate_iteration_tests {
        /// `get_object` の結果を先に用意しておき、ヒットしたキーを返す
        ///
        /// `Ok(true)` = ヒット、`Ok(false)` = miss、`Err` = 403 等の異常。
        fn restore_with_scripted_results(
            keys: &[String],
            results: std::vec::Vec<anyhow::Result<bool>>,
        ) -> anyhow::Result<Option<String>> {
            let mut iter = results.into_iter();
            for key in keys {
                match iter.next() {
                    Some(Ok(true)) => return Ok(Some(key.clone())),
                    Some(Ok(false)) => {}
                    Some(Err(e)) => return Err(e),
                    None => break,
                }
            }
            Ok(None)
        }

        #[test]
        fn test_primary_hit_does_not_try_fallback() {
            // Arrange: 候補は 2 つあるが結果は 1 つしか用意しない。
            // fallback まで試そうとすれば結果が尽きて None になり、検出できる
            let keys = vec!["primary".to_string(), "fallback".to_string()];
            let results = vec![Ok(true)];

            // Act
            let hit = restore_with_scripted_results(&keys, results).unwrap();

            // Assert: primary で打ち切る。余分な GetObject を打たないこと
            assert_eq!(hit.as_deref(), Some("primary"));
        }

        #[test]
        fn test_primary_miss_fallback_hit() {
            // Arrange: feature ブランチのキーが無く、main のキャッシュへ落ちる典型ケース。
            // fallback2 まで試さないことを見るため結果は 2 つしか用意しない
            let keys = vec![
                "primary".to_string(),
                "fallback1".to_string(),
                "fallback2".to_string(),
            ];
            let results = vec![Ok(false), Ok(true)];

            // Act
            let hit = restore_with_scripted_results(&keys, results).unwrap();

            // Assert: 最も近い（先に書かれた）候補が選ばれる。順序が入れ替わると
            // 意図より古いキャッシュを引いてしまう
            assert_eq!(hit.as_deref(), Some("fallback1"));
        }

        #[test]
        fn test_all_miss_returns_none() {
            // Arrange: 全候補が miss
            let keys = vec!["primary".to_string(), "fallback".to_string()];
            let results = vec![Ok(false), Ok(false)];

            // Act
            let hit = restore_with_scripted_results(&keys, results).unwrap();

            // Assert: エラーではなく「何もしなかった」として返す。
            // 呼び出し側は stdout に false を出して exit 0 で終わる
            assert_eq!(hit, None);
        }

        #[test]
        fn test_error_stops_further_tries() {
            // Arrange: primary で 403。結果を 1 つしか用意していないので、
            // 後続を試そうとすれば結果が尽きて Ok(None) になり、検出できる
            let keys = vec!["primary".to_string(), "fallback".to_string()];
            let results = vec![Err(anyhow::anyhow!("403 AccessDenied"))];

            // Act
            let result = restore_with_scripted_results(&keys, results);

            // Assert: 即座に伝播する。権限不足のまま候補を舐めても全部失敗するだけで、
            // 最後に「全 miss」と報告すると原因が隠れる
            assert!(result.unwrap_err().to_string().contains("403"));
        }
    }
}
