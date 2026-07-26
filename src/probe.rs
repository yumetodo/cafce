/// S3オブジェクトキーを組み立てる
///
/// レイアウト: `{prefix}/{project}/{cache_key}` (prefix なしは `{project}/{cache_key}`)
pub fn build_object_key(prefix: Option<&str>, project: &str, cache_key: &str) -> String {
    match prefix {
        Some(p) => format!("{p}/{project}/{cache_key}"),
        None => format!("{project}/{cache_key}"),
    }
}

/// head_object で S3 オブジェクトの存在を確認する
///
/// - 200 OK → Ok(true)
/// - 404 NotFound → Ok(false)
/// - 403 AccessDenied / その他エラー → Err（後続 fallback は試さない）
async fn check_key_exists(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    object_key: &str,
) -> anyhow::Result<bool> {
    use aws_sdk_s3::error::SdkError;
    use aws_sdk_s3::operation::head_object::HeadObjectError;

    match client
        .head_object()
        .bucket(bucket)
        .key(object_key)
        .send()
        .await
    {
        Ok(_) => Ok(true),
        Err(SdkError::ServiceError(e)) if matches!(e.err(), HeadObjectError::NotFound(_)) => {
            Ok(false)
        }
        Err(e) => {
            use anyhow::Context as _;
            Err(anyhow::anyhow!(e)).with_context(|| {
                format!(
                    "S3 HeadObject に失敗しました (bucket={bucket}, key={object_key})\n\
                     403 の場合は s3:ListBucket 権限を確認してください"
                )
            })
        }
    }
}

/// primary キーと fallback_keys を順に試行し、いずれか存在すれば true を返す
///
/// - いずれかが 200 → true（以降の head_object は打たない）
/// - 全て 404 → false
/// - 404 以外のエラー → エラーを伝播（後続 fallback は試さない）
pub async fn probe(
    setting: &crate::setting::Setting,
    env: &crate::env::Env,
    client: &aws_sdk_s3::Client,
    base_path: &std::path::Path,
) -> anyhow::Result<bool> {
    use anyhow::Context as _;

    let primary_key = setting
        .resolve_primary_key(base_path)
        .context("primary キーの計算に失敗しました")?;

    for cache_key in std::iter::once(&primary_key).chain(setting.fallback_keys.iter()) {
        let object_key = build_object_key(env.s3_prefix(), &setting.project, cache_key);
        if check_key_exists(client, env.bucket(), &object_key).await? {
            return Ok(true);
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod build_object_key_tests {
        use super::*;

        #[test]
        fn test_with_prefix() {
            // Arrange
            let prefix = Some("ci-cache");
            let project = "my-app";
            let cache_key = "abc123";

            // Act
            let key = build_object_key(prefix, project, cache_key);

            // Assert
            assert_eq!(key, "ci-cache/my-app/abc123");
        }

        #[test]
        fn test_without_prefix() {
            // Arrange
            let prefix: Option<&str> = None;
            let project = "my-app";
            let cache_key = "abc123";

            // Act
            let key = build_object_key(prefix, project, cache_key);

            // Assert
            assert_eq!(key, "my-app/abc123");
        }

        #[test]
        fn test_with_hash_like_cache_key() {
            // Arrange
            let prefix: Option<&str> = None;
            let project = "my-project";
            let cache_key = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";

            // Act
            let key = build_object_key(prefix, project, cache_key);

            // Assert
            assert_eq!(
                key,
                "my-project/a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
            );
        }

        #[test]
        fn test_fallback_default_key() {
            // Arrange: 0件フォールバック時の "default" キー
            let prefix = Some("team");
            let project = "api";
            let cache_key = "default";

            // Act
            let key = build_object_key(prefix, project, cache_key);

            // Assert
            assert_eq!(key, "team/api/default");
        }
    }

    mod probe_retry_logic_tests {
        /// head_object の代わりにスクリプト済み結果を返す同期版の retry ロジック
        fn probe_with_scripted_results(
            keys: &[String],
            results: Vec<anyhow::Result<bool>>,
        ) -> anyhow::Result<bool> {
            let mut iter = results.into_iter();
            for _key in keys {
                match iter.next() {
                    Some(Ok(true)) => return Ok(true),
                    Some(Ok(false)) => {}
                    Some(Err(e)) => return Err(e),
                    None => break,
                }
            }
            Ok(false)
        }

        #[test]
        fn test_primary_hit_does_not_try_fallback() {
            // Arrange: primary はヒット、fallback は試されない
            let keys = vec!["primary".to_string(), "fallback".to_string()];
            let results = vec![Ok(true)]; // primary のみ提供（消費されたら panic にはならない）

            // Act
            let result = probe_with_scripted_results(&keys, results);

            // Assert
            assert!(result.unwrap());
        }

        #[test]
        fn test_primary_miss_first_fallback_hit() {
            // Arrange
            let keys = vec![
                "primary".to_string(),
                "fallback1".to_string(),
                "fallback2".to_string(),
            ];
            let results = vec![Ok(false), Ok(true)];

            // Act
            let result = probe_with_scripted_results(&keys, results);

            // Assert
            assert!(result.unwrap());
        }

        #[test]
        fn test_primary_miss_nth_fallback_hit() {
            // Arrange: 2番目のfallbackでヒット
            let keys = vec![
                "primary".to_string(),
                "fallback1".to_string(),
                "fallback2".to_string(),
            ];
            let results = vec![Ok(false), Ok(false), Ok(true)];

            // Act
            let result = probe_with_scripted_results(&keys, results);

            // Assert
            assert!(result.unwrap());
        }

        #[test]
        fn test_all_miss_returns_false() {
            // Arrange
            let keys = vec!["primary".to_string(), "fallback".to_string()];
            let results = vec![Ok(false), Ok(false)];

            // Act
            let result = probe_with_scripted_results(&keys, results);

            // Assert
            assert!(!result.unwrap());
        }

        #[test]
        fn test_empty_fallback_keys_primary_only() {
            // Arrange: fallback_keys が空リスト（primary のみ試行）
            let keys = vec!["primary".to_string()];
            let results = vec![Ok(false)];

            // Act
            let result = probe_with_scripted_results(&keys, results);

            // Assert
            assert!(!result.unwrap());
        }

        #[test]
        fn test_error_stops_further_tries() {
            // Arrange: primary でエラー → 後続を試さない
            let keys = vec!["primary".to_string(), "fallback".to_string()];
            let results = vec![Err(anyhow::anyhow!("403 AccessDenied"))];

            // Act
            let result = probe_with_scripted_results(&keys, results);

            // Assert
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("403"));
        }

        #[test]
        fn test_error_midway_stops_further_tries() {
            // Arrange: 1つ目miss → 2つ目でエラー → 3つ目は試さない
            let keys = vec![
                "primary".to_string(),
                "fallback1".to_string(),
                "fallback2".to_string(),
            ];
            let results = vec![Ok(false), Err(anyhow::anyhow!("403 AccessDenied"))];

            // Act
            let result = probe_with_scripted_results(&keys, results);

            // Assert
            assert!(result.is_err());
        }
    }
}
