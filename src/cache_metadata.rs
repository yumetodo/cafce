//! S3 user metadata のキー名定数と、生成・検証（パース）のロジック
//!
//! S3 の user metadata はキーが `x-amz-meta-` 接頭辞付きで送られ、取得時は小文字化されて返る。
//! 値は US-ASCII で、全体で 2 KB の上限がある。cafce は 3 つだけを付与する。
//!
//! `cafce-content-sha256` の対象を「オブジェクト本体（tar.zst のバイト列）」ではなく
//! 「圧縮前の tar ストリーム」にするのは、ハッシュを**内容の同一性**の表現にしたいためである。
//! zstd の版数やレベルが変われば同じ内容でも本体バイト列は変わるが、tar ストリームは変わらない。
//! CI 群の中に cafce の版数が混在していても、内容が同じ限り再アップロードは起きない。
//!
//! このハッシュは転送・保管の破損検出には使わない。破損検出は S3 のフレキシブルチェックサム
//! （`x-amz-checksum-sha256`）に担当させ、役割を分離する（設計doc 6.6 / 6.7）。

/// メタデータおよびアーカイブレイアウトのスキーマ版数
pub const SCHEMA_VERSION: u32 = 1;

/// ペイロードのアーカイブ形式
pub const ARCHIVE_FORMAT: &str = "tar+zstd";

/// スキーマ版数を格納する user metadata のキー名（`x-amz-meta-` を除いた名前）
pub const KEY_SCHEMA_VERSION: &str = "cafce-schema-version";

/// アーカイブ形式を格納する user metadata のキー名
pub const KEY_ARCHIVE_FORMAT: &str = "cafce-archive-format";

/// 展開後の内容の同一性を表すハッシュを格納する user metadata のキー名
pub const KEY_CONTENT_SHA256: &str = "cafce-content-sha256";

/// `restore` が解釈した cafce の user metadata
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMetadata {
    pub schema_version: u32,
    pub archive_format: String,
    /// 圧縮前の tar ストリーム全体の SHA-256（小文字 16 進 64 文字）
    pub content_sha256: String,
}

/// `put_object` に添える user metadata を組み立てる
pub fn build_metadata(content_sha256: &str) -> std::collections::HashMap<String, String> {
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(KEY_SCHEMA_VERSION.to_string(), SCHEMA_VERSION.to_string());
    metadata.insert(KEY_ARCHIVE_FORMAT.to_string(), ARCHIVE_FORMAT.to_string());
    metadata.insert(KEY_CONTENT_SHA256.to_string(), content_sha256.to_string());
    metadata
}

/// 内容ハッシュが小文字 16 進 64 文字であることを確認する
fn validate_content_sha256(value: &str) -> Result<(), crate::error::CacheMetadataError> {
    let is_valid = value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));

    if is_valid {
        Ok(())
    } else {
        Err(crate::error::CacheMetadataError::MalformedContentHash {
            value: value.to_string(),
        })
    }
}

/// S3 互換サーバによる大文字小文字の揺れを吸収してキーを引く
fn lookup<'a>(
    metadata: &'a std::collections::HashMap<String, String>,
    key: &str,
) -> Option<&'a str> {
    metadata
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// `head_object` / `get_object` が返した user metadata を解釈する
///
/// cafce の metadata が 1 つも無いオブジェクト（cafce 以外が置いたもの、あるいは
/// `aws s3 cp` で手置きしたもの）は `Ok(None)` を返す。呼び出し側は
/// `restore` なら内容ハッシュ検証をスキップし、`store` ならハッシュ不一致と同じ扱いにする。
///
/// # Errors
///
/// - スキーマ版数が 10 進整数として読めない
/// - スキーマ版数が未知（将来の版数）。黙って展開せずエラーにする
/// - アーカイブ形式が未知
/// - 内容ハッシュの形式が不正
pub fn parse_metadata(
    metadata: Option<&std::collections::HashMap<String, String>>,
) -> Result<Option<CacheMetadata>, crate::error::CacheMetadataError> {
    let metadata = match metadata {
        Some(metadata) => metadata,
        None => return Ok(None),
    };

    // cafce が置いたオブジェクトかどうかはスキーマ版数の有無で判断する
    let raw_version = match lookup(metadata, KEY_SCHEMA_VERSION) {
        Some(raw_version) => raw_version,
        None => return Ok(None),
    };

    let schema_version = raw_version.parse::<u32>().map_err(|_| {
        crate::error::CacheMetadataError::MalformedSchemaVersion {
            value: raw_version.to_string(),
        }
    })?;
    if schema_version > SCHEMA_VERSION {
        return Err(crate::error::CacheMetadataError::UnknownSchemaVersion {
            found: schema_version,
            supported: SCHEMA_VERSION,
        });
    }

    let archive_format = lookup(metadata, KEY_ARCHIVE_FORMAT).unwrap_or_default();
    if archive_format != ARCHIVE_FORMAT {
        return Err(crate::error::CacheMetadataError::UnknownArchiveFormat {
            found: archive_format.to_string(),
            supported: ARCHIVE_FORMAT,
        });
    }

    let content_sha256 = lookup(metadata, KEY_CONTENT_SHA256).unwrap_or_default();
    validate_content_sha256(content_sha256)?;

    Ok(Some(CacheMetadata {
        schema_version,
        archive_format: archive_format.to_string(),
        content_sha256: content_sha256.to_string(),
    }))
}

/// 内容ハッシュを照合する
pub fn verify_content_sha256(
    expected: &str,
    actual: &str,
) -> Result<(), crate::error::CacheMetadataError> {
    if expected == actual {
        Ok(())
    } else {
        Err(crate::error::CacheMetadataError::ContentHashMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        })
    }
}

/// 圧縮後バイト列の SHA-256 を、S3 が `x-amz-checksum-sha256` で要求する Base64 形式へ変換する
///
/// user metadata の内容ハッシュは小文字 16 進なので、両者を取り違えないよう形式を分けている。
/// `base64` crate を新規に足すのではなく `aws-smithy-types` を使うのは、
/// SDK と同じ実装を使う方が値の食い違いを起こしにくいためである。
pub fn to_checksum_base64(digest: &[u8; 32]) -> String {
    aws_smithy_types::base64::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn metadata_map(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn valid_metadata() -> std::collections::HashMap<String, String> {
        metadata_map(&[
            (KEY_SCHEMA_VERSION, "1"),
            (KEY_ARCHIVE_FORMAT, ARCHIVE_FORMAT),
            (KEY_CONTENT_SHA256, VALID_HASH),
        ])
    }

    mod build_metadata_tests {
        use super::*;

        #[test]
        fn test_contains_all_three_keys() {
            // Arrange
            let content_sha256 = VALID_HASH;

            // Act
            let metadata = build_metadata(content_sha256);

            // Assert
            assert_eq!(metadata.len(), 3);
            assert_eq!(metadata.get(KEY_SCHEMA_VERSION).unwrap(), "1");
            assert_eq!(metadata.get(KEY_ARCHIVE_FORMAT).unwrap(), "tar+zstd");
            assert_eq!(metadata.get(KEY_CONTENT_SHA256).unwrap(), VALID_HASH);
        }

        #[test]
        fn test_roundtrips_through_parse() {
            // Arrange
            let built = build_metadata(VALID_HASH);

            // Act
            let parsed = parse_metadata(Some(&built)).unwrap().unwrap();

            // Assert
            assert_eq!(
                parsed,
                CacheMetadata {
                    schema_version: SCHEMA_VERSION,
                    archive_format: ARCHIVE_FORMAT.to_string(),
                    content_sha256: VALID_HASH.to_string(),
                }
            );
        }

        #[test]
        fn test_values_are_us_ascii_and_within_2kb() {
            // Arrange: S3 の user metadata は US-ASCII かつ全体 2 KB 上限
            let metadata = build_metadata(VALID_HASH);

            // Act
            let total_size: usize = metadata.iter().map(|(k, v)| k.len() + v.len()).sum();

            // Assert
            assert!(metadata.iter().all(|(k, v)| k.is_ascii() && v.is_ascii()));
            assert!(total_size < 2048, "total_size={total_size}");
        }
    }

    mod parse_metadata_tests {
        use super::*;

        #[test]
        fn test_none_metadata_is_absent() {
            // Arrange
            let metadata = None;

            // Act
            let parsed = parse_metadata(metadata).unwrap();

            // Assert
            assert_eq!(parsed, None);
        }

        #[test]
        fn test_empty_metadata_is_absent() {
            // Arrange: cafce 以外が置いたオブジェクト
            let metadata = metadata_map(&[]);

            // Act
            let parsed = parse_metadata(Some(&metadata)).unwrap();

            // Assert
            assert_eq!(parsed, None);
        }

        #[test]
        fn test_foreign_metadata_is_absent() {
            // Arrange: cafce と無関係な metadata だけがある
            let metadata = metadata_map(&[("some-other-tool", "1")]);

            // Act
            let parsed = parse_metadata(Some(&metadata)).unwrap();

            // Assert
            assert_eq!(parsed, None);
        }

        #[test]
        fn test_valid_metadata_is_parsed() {
            // Arrange
            let metadata = valid_metadata();

            // Act
            let parsed = parse_metadata(Some(&metadata)).unwrap().unwrap();

            // Assert
            assert_eq!(parsed.schema_version, 1);
            assert_eq!(parsed.archive_format, ARCHIVE_FORMAT);
            assert_eq!(parsed.content_sha256, VALID_HASH);
        }

        #[test]
        fn test_uppercase_keys_are_accepted() {
            // Arrange: S3 互換サーバによる大文字小文字の揺れを想定する
            let metadata = metadata_map(&[
                ("Cafce-Schema-Version", "1"),
                ("CAFCE-ARCHIVE-FORMAT", ARCHIVE_FORMAT),
                ("Cafce-Content-Sha256", VALID_HASH),
            ]);

            // Act
            let parsed = parse_metadata(Some(&metadata)).unwrap().unwrap();

            // Assert
            assert_eq!(parsed.content_sha256, VALID_HASH);
        }

        #[test]
        fn test_future_schema_version_is_error() {
            // Arrange: 将来の版数のオブジェクトは黙って展開しない
            let mut metadata = valid_metadata();
            metadata.insert(KEY_SCHEMA_VERSION.to_string(), "2".to_string());

            // Act
            let result = parse_metadata(Some(&metadata));

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::CacheMetadataError::UnknownSchemaVersion {
                    found: 2,
                    supported: 1
                })
            ));
        }

        #[test]
        fn test_non_numeric_schema_version_is_error() {
            // Arrange
            let mut metadata = valid_metadata();
            metadata.insert(KEY_SCHEMA_VERSION.to_string(), "v1".to_string());

            // Act
            let result = parse_metadata(Some(&metadata));

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::CacheMetadataError::MalformedSchemaVersion { .. })
            ));
        }

        #[test]
        fn test_unknown_archive_format_is_error() {
            // Arrange
            let mut metadata = valid_metadata();
            metadata.insert(KEY_ARCHIVE_FORMAT.to_string(), "zip".to_string());

            // Act
            let result = parse_metadata(Some(&metadata));

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::CacheMetadataError::UnknownArchiveFormat { .. })
            ));
        }

        #[test]
        fn test_missing_archive_format_is_error() {
            // Arrange: スキーマ版数があるのに形式が欠けているのは壊れたメタデータ
            let mut metadata = valid_metadata();
            metadata.remove(KEY_ARCHIVE_FORMAT);

            // Act
            let result = parse_metadata(Some(&metadata));

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::CacheMetadataError::UnknownArchiveFormat { .. })
            ));
        }

        #[test]
        fn test_short_content_hash_is_error() {
            // Arrange
            let mut metadata = valid_metadata();
            metadata.insert(KEY_CONTENT_SHA256.to_string(), "abc123".to_string());

            // Act
            let result = parse_metadata(Some(&metadata));

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::CacheMetadataError::MalformedContentHash { .. })
            ));
        }

        #[test]
        fn test_uppercase_content_hash_is_error() {
            // Arrange: 値の形式は小文字 16 進に固定する（比較を単純な文字列一致に保つため）
            let mut metadata = valid_metadata();
            metadata.insert(
                KEY_CONTENT_SHA256.to_string(),
                VALID_HASH.to_ascii_uppercase(),
            );

            // Act
            let result = parse_metadata(Some(&metadata));

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::CacheMetadataError::MalformedContentHash { .. })
            ));
        }

        #[test]
        fn test_non_hex_content_hash_is_error() {
            // Arrange: 長さは 64 文字だが 16 進ではない
            let mut metadata = valid_metadata();
            metadata.insert(KEY_CONTENT_SHA256.to_string(), "z".repeat(64));

            // Act
            let result = parse_metadata(Some(&metadata));

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::CacheMetadataError::MalformedContentHash { .. })
            ));
        }

        #[test]
        fn test_missing_content_hash_is_error() {
            // Arrange
            let mut metadata = valid_metadata();
            metadata.remove(KEY_CONTENT_SHA256);

            // Act
            let result = parse_metadata(Some(&metadata));

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::CacheMetadataError::MalformedContentHash { .. })
            ));
        }
    }

    mod verify_content_sha256_tests {
        use super::*;

        #[test]
        fn test_matching_hashes_are_ok() {
            // Arrange
            let expected = VALID_HASH;
            let actual = VALID_HASH;

            // Act
            let result = verify_content_sha256(expected, actual);

            // Assert
            assert!(result.is_ok());
        }

        #[test]
        fn test_mismatching_hashes_are_error() {
            // Arrange
            let expected = VALID_HASH;
            let actual = "0".repeat(64);

            // Act
            let result = verify_content_sha256(expected, &actual);

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::CacheMetadataError::ContentHashMismatch { .. })
            ));
        }

        #[test]
        fn test_mismatch_error_mentions_partial_restore() {
            // Arrange: 展開後にしか判明しないため、中途半端な木が残る旨を伝える必要がある
            let expected = VALID_HASH;
            let actual = "0".repeat(64);

            // Act
            let message = verify_content_sha256(expected, &actual)
                .unwrap_err()
                .to_string();

            // Assert
            assert!(message.contains("中途半端に復元されたファイルが残っている可能性"));
        }
    }

    mod to_checksum_base64_tests {
        use super::*;

        #[test]
        fn test_known_value_of_empty_sha256() {
            // Arrange: 空文字列の SHA-256（既知値による回帰テスト）
            use sha2::Digest as _;
            let digest: [u8; 32] = sha2::Sha256::digest(b"").into();

            // Act
            let encoded = to_checksum_base64(&digest);

            // Assert: `printf '' | sha256sum | xxd -r -p | base64` と一致する
            assert_eq!(encoded, "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=");
        }

        #[test]
        fn test_known_value_of_test_content() {
            // Arrange: hash_calculator の既知値テストと同じ入力を使う
            use sha2::Digest as _;
            let digest: [u8; 32] = sha2::Sha256::digest(b"test content").into();

            // Act
            let encoded = to_checksum_base64(&digest);

            // Assert
            assert_eq!(encoded, "auinVVUgn9bEQVfArtgBbnY/9DWhnPGG92hjFAFD/3I=");
        }

        #[test]
        fn test_encoded_length_is_44() {
            // Arrange: 32 バイトの Base64 は必ず 44 文字（パディング込み）
            let digest = [0u8; 32];

            // Act
            let encoded = to_checksum_base64(&digest);

            // Assert
            assert_eq!(encoded.len(), 44);
        }

        #[test]
        fn test_hex_and_base64_are_different_formats() {
            // Arrange: metadata の 16 進と混同しないことを明示する
            use sha2::Digest as _;
            let digest: [u8; 32] = sha2::Sha256::digest(b"test content").into();
            let hex = format!("{:x}", sha2::Sha256::digest(b"test content"));

            // Act
            let base64 = to_checksum_base64(&digest);

            // Assert
            assert_ne!(hex, base64);
            assert_eq!(hex.len(), 64);
            assert_eq!(base64.len(), 44);
        }
    }
}
