//! `Setting.paths` の glob パターンを、アーカイブ対象のエントリ列へ解決する
//!
//! `key.files` を解決する `crate::file_matcher::FileMatcher` とは責務が異なるため再利用しない:
//!
//! - `FileMatcher` はディレクトリを結果から除外する。`paths` は空ディレクトリも含めて
//!   ディレクトリ構造を保存する必要がある
//! - `FileMatcher` は 50 件上限を持つ。キャッシュ本体には不適切
//! - `FileMatcher` は基準ディレクトリ外のマッチを黙って無視する。`paths` では
//!   設定ミスに気づけるようエラーにしたい

/// アーカイブ対象エントリの種別
///
/// シンボリックリンクは辿らずリンクとして保存するため、`Directory` / `File` とは別扱いにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

/// アーカイブ対象の 1 エントリ
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// 基準ディレクトリからの**相対**パス（絶対パスは決して入らない）
    pub relative_path: std::path::PathBuf,

    /// tar のエントリ名。区切りは `/` に統一済みで、プラットフォーム間で一致する
    pub archive_path: String,

    pub kind: EntryKind,
}

/// 相対パスを tar のエントリ名（`/` 区切り）へ変換する
///
/// 非 UTF-8 のパスは lossy 変換する。CI のキャッシュ対象に非 UTF-8 のパスは想定しない。
pub fn to_archive_path(relative_path: &std::path::Path) -> String {
    relative_path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<std::vec::Vec<String>>()
        .join("/")
}

/// ファイルシステムに触れずに `.` / `..` を畳んでパスを正規化する
///
/// `Path::starts_with` は components 単位の**字句**比較であり、
/// `/base/../etc` のようなパスでも `/base` で始まると判定してしまう。
/// 脱出判定の前に必ずこれを通す。
pub(crate) fn normalize_lexically(path: &std::path::Path) -> std::path::PathBuf {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn has_parent_dir_component(pattern: &str) -> bool {
    std::path::Path::new(pattern)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

/// 絶対パスを基準ディレクトリからの相対パスへ変換する
///
/// 基準ディレクトリの外を指す場合はエラーにする（`FileMatcher` のように黙って無視しない）。
fn relative_within_base(
    path: &std::path::Path,
    base_path: &std::path::Path,
) -> Result<std::path::PathBuf, crate::error::PathResolveError> {
    let normalized_path = normalize_lexically(path);
    let normalized_base = normalize_lexically(base_path);

    normalized_path
        .strip_prefix(&normalized_base)
        .map(std::path::Path::to_path_buf)
        .map_err(|_| crate::error::PathResolveError::EscapesBaseDirectory {
            path: path.to_string_lossy().into_owned(),
        })
}

/// 1 つのパスをエントリ集合へ追加する（ディレクトリなら再帰的に配下も追加する）
fn collect_entry(
    path: &std::path::Path,
    base_path: &std::path::Path,
    entries: &mut std::collections::BTreeMap<String, ArchiveEntry>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("メタデータの取得に失敗しました: {}", path.display()))?;

    // シンボリックリンクは辿らない（同じ実体の重複格納や、基準ディレクトリ外の
    // ファイルの持ち出しを避けるため）。ディレクトリへのリンクもリンクとして保存する。
    if !metadata.file_type().is_symlink() && metadata.is_dir() {
        for entry in walkdir::WalkDir::new(path).follow_links(false) {
            let entry = entry
                .with_context(|| format!("ディレクトリの走査に失敗しました: {}", path.display()))?;
            push_entry(entry.path(), base_path, entries)?;
        }
        return Ok(());
    }

    push_entry(path, base_path, entries)
}

fn push_entry(
    path: &std::path::Path,
    base_path: &std::path::Path,
    entries: &mut std::collections::BTreeMap<String, ArchiveEntry>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let relative_path = relative_within_base(path, base_path)?;
    // 基準ディレクトリ自身は tar のエントリにしない
    if relative_path.as_os_str().is_empty() {
        return Ok(());
    }

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("メタデータの取得に失敗しました: {}", path.display()))?;
    let file_type = metadata.file_type();

    let kind = if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        // FIFO・デバイスファイル・ソケットなどは保存対象外（Non-Goal）
        log::warn!(
            "通常ファイル・ディレクトリ・シンボリックリンクのいずれでもないためスキップします: {}",
            path.display()
        );
        return Ok(());
    };

    let archive_path = to_archive_path(&relative_path);
    entries.insert(
        archive_path.clone(),
        ArchiveEntry {
            relative_path,
            archive_path,
            kind,
        },
    );
    Ok(())
}

/// glob パターン列をアーカイブ対象エントリ列へ解決する
///
/// 結果は重複排除済みで、tar のエントリ名（`/` 区切り）のバイト列昇順にソートされている。
/// ソート順を固定するのは、ファイルシステムの列挙順が環境依存であり、
/// アーカイブの決定論性を壊すためである。
///
/// # Errors
///
/// - `patterns` が空（`store` では設定ミス）
/// - 絶対パスのパターン
/// - `..` を含む、あるいは解決結果が基準ディレクトリの外を指すパターン
/// - どのパターンにも 1 件もマッチしない
pub fn resolve_paths(
    patterns: &[String],
    base_path: &std::path::Path,
) -> anyhow::Result<std::vec::Vec<ArchiveEntry>> {
    use anyhow::Context as _;

    if patterns.is_empty() {
        return Err(crate::error::PathResolveError::EmptyPaths.into());
    }

    // BTreeMap のキーを tar のエントリ名にすることで、重複排除とバイト列昇順ソートを同時に満たす
    let mut entries: std::collections::BTreeMap<String, ArchiveEntry> =
        std::collections::BTreeMap::new();

    for pattern in patterns {
        if std::path::Path::new(pattern).is_absolute() {
            return Err(crate::error::PathResolveError::AbsolutePathNotAllowed {
                pattern: pattern.clone(),
            }
            .into());
        }
        if has_parent_dir_component(pattern) {
            return Err(crate::error::PathResolveError::EscapesBaseDirectory {
                path: pattern.clone(),
            }
            .into());
        }

        // base_path に `[` や `*` などの glob メタ文字が含まれていても、
        // それ自体はパターンとして解釈させない
        let escaped_base_path = glob::Pattern::escape(&base_path.to_string_lossy());
        let full_pattern = std::path::Path::new(&escaped_base_path)
            .join(pattern)
            .to_string_lossy()
            .into_owned();

        let matched_paths = glob::glob(&full_pattern)
            .with_context(|| format!("パターンマッチングに失敗しました: {pattern}"))?;

        for matched_path in matched_paths {
            let matched_path =
                matched_path.with_context(|| format!("パターンの走査に失敗しました: {pattern}"))?;
            collect_entry(&matched_path, base_path, &mut entries)?;
        }
    }

    if entries.is_empty() {
        return Err(crate::error::PathResolveError::NoMatch {
            patterns: patterns.to_vec(),
        }
        .into());
    }

    log::debug!("キャッシュ対象エントリ数: {}", entries.len());

    Ok(entries.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_paths(entries: &[ArchiveEntry]) -> std::vec::Vec<&str> {
        entries
            .iter()
            .map(|entry| entry.archive_path.as_str())
            .collect()
    }

    fn kind_of<'a>(entries: &'a [ArchiveEntry], archive_path: &str) -> &'a EntryKind {
        &entries
            .iter()
            .find(|entry| entry.archive_path == archive_path)
            .unwrap_or_else(|| panic!("エントリ {archive_path} が見つかりません"))
            .kind
    }

    mod to_archive_path_tests {
        use super::*;

        #[test]
        fn test_single_component() {
            // Arrange
            let relative_path = std::path::Path::new("foo.txt");

            // Act
            let archive_path = to_archive_path(relative_path);

            // Assert: 単一要素では区切り文字が絡まないので素通しになる
            assert_eq!(archive_path, "foo.txt");
        }

        #[test]
        fn test_nested_components_joined_with_slash() {
            // Arrange: `collect()` でプラットフォーム既定の区切り文字で組む
            // （Windows なら `a\b\c.txt`）
            let relative_path: std::path::PathBuf = ["a", "b", "c.txt"].iter().collect();

            // Act
            let archive_path = to_archive_path(&relative_path);

            // Assert: tar のエントリ名は常に `/` 区切り。ここが OS 依存になると
            // 同じ内容でもアーカイブのバイト列が割れ、決定論性が崩れる
            assert_eq!(archive_path, "a/b/c.txt");
        }
    }

    mod normalize_lexically_tests {
        use super::*;

        #[test]
        fn test_removes_cur_dir() {
            // Arrange: glob の結果には `./` が混ざりうる
            let path = std::path::Path::new("/base/./foo");

            // Act
            let normalized = normalize_lexically(path);

            // Assert: `.` は取り除く。残っていると strip_prefix での相対化に失敗する
            assert_eq!(normalized, std::path::Path::new("/base/foo"));
        }

        #[test]
        fn test_resolves_parent_dir() {
            // Arrange: 途中で親へ戻るが最終的には基準内に収まるパス
            let path = std::path::Path::new("/base/sub/../foo");

            // Act
            let normalized = normalize_lexically(path);

            // Assert: `..` は直前の要素を打ち消す
            assert_eq!(normalized, std::path::Path::new("/base/foo"));
        }

        #[test]
        fn test_parent_dir_can_escape_base() {
            // Arrange: この関数が存在する理由そのものの入力。
            // `Path::new("/base/../etc/passwd").starts_with("/base")` は
            // components 単位の字句比較なので **true** を返してしまう
            let path = std::path::Path::new("/base/../etc/passwd");

            // Act
            let normalized = normalize_lexically(path);

            // Assert: 畳んだ結果は `/base` 配下ではないと分かる形になる。
            // これを starts_with に掛けることで初めて脱出を検出できる
            assert_eq!(normalized, std::path::Path::new("/etc/passwd"));
        }
    }

    mod resolve_paths_tests {
        use super::*;

        #[test]
        fn test_single_file() {
            // Arrange: ワイルドカードを含まない、ファイル名そのままのパターン
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::write(base_path.join("foo.txt"), "content").unwrap();
            let patterns = vec!["foo.txt".to_string()];

            // Act
            let entries = resolve_paths(&patterns, base_path).unwrap();

            // Assert: 結果は基準ディレクトリからの相対パスであること（絶対パスが漏れないこと）と、
            // 種別が File として判定されることを固定する
            assert_eq!(archive_paths(&entries), vec!["foo.txt"]);
            assert_eq!(kind_of(&entries, "foo.txt"), &EntryKind::File);
        }

        #[test]
        fn test_directory_is_expanded_recursively() {
            // Arrange: ディレクトリ名だけを 1 つ指定する（GitLab CI の cache:paths と同じ書き方）。
            // ドットファイルも入れているのは、glob の `*` では拾えないものが
            // walkdir 経由なら入ることを確認するため
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::create_dir_all(base_path.join("target/debug")).unwrap();
            std::fs::write(base_path.join("target/debug/app"), "binary").unwrap();
            std::fs::write(base_path.join("target/.rustc_info.json"), "{}").unwrap();
            let patterns = vec!["target".to_string()];

            // Act
            let entries = resolve_paths(&patterns, base_path).unwrap();

            // Assert: 配下のファイルだけでなく `target` / `target/debug` という
            // ディレクトリエントリ自体も含む（FileMatcher はここを除外する）。
            // 並びはバイト列昇順なので `.rustc_info.json` が `debug` より先に来る
            assert_eq!(
                archive_paths(&entries),
                vec![
                    "target",
                    "target/.rustc_info.json",
                    "target/debug",
                    "target/debug/app",
                ]
            );
            assert_eq!(kind_of(&entries, "target"), &EntryKind::Directory);
            assert_eq!(kind_of(&entries, "target/debug"), &EntryKind::Directory);
            assert_eq!(kind_of(&entries, "target/debug/app"), &EntryKind::File);
        }

        #[test]
        fn test_empty_directory_is_preserved() {
            // Arrange: 中身が無いディレクトリだけを指定する
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::create_dir_all(base_path.join("empty")).unwrap();
            let patterns = vec!["empty".to_string()];

            // Act
            let entries = resolve_paths(&patterns, base_path).unwrap();

            // Assert: 0 件ではなくディレクトリエントリ 1 件になる。ファイルだけを集める実装だと
            // ここが空になり、復元時にディレクトリが失われる（ビルドツールが
            // 空の出力ディレクトリの存在を前提にしていると壊れる）
            assert_eq!(archive_paths(&entries), vec!["empty"]);
            assert_eq!(kind_of(&entries, "empty"), &EntryKind::Directory);
        }

        #[test]
        fn test_wildcard_pattern() {
            // Arrange: マッチしない `c.md` を混ぜて、パターンが絞り込みとして効くことを見る
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::write(base_path.join("a.txt"), "a").unwrap();
            std::fs::write(base_path.join("b.txt"), "b").unwrap();
            std::fs::write(base_path.join("c.md"), "c").unwrap();
            let patterns = vec!["*.txt".to_string()];

            // Act
            let entries = resolve_paths(&patterns, base_path).unwrap();

            // Assert: `.txt` の 2 件だけが入る
            assert_eq!(archive_paths(&entries), vec!["a.txt", "b.txt"]);
        }

        #[test]
        fn test_recursive_wildcard_pattern() {
            // Arrange: `**` が複数階層をまたぐこと（`nested/deep` の 2 段下）を確認する
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::create_dir_all(base_path.join("nested/deep")).unwrap();
            std::fs::write(base_path.join("nested/deep/x.lock"), "x").unwrap();
            let patterns = vec!["**/*.lock".to_string()];

            // Act
            let entries = resolve_paths(&patterns, base_path).unwrap();

            // Assert: ファイルにマッチした場合は途中のディレクトリを含めない
            // （ディレクトリ指定のときだけ再帰展開する、という切り分け）
            assert_eq!(archive_paths(&entries), vec!["nested/deep/x.lock"]);
        }

        #[test]
        fn test_duplicates_are_deduplicated() {
            // Arrange: 3 つのパターンが全て同じ 1 ファイルにマッチする
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::write(base_path.join("foo.txt"), "content").unwrap();
            let patterns = vec![
                "foo.txt".to_string(),
                "*.txt".to_string(),
                "foo.*".to_string(),
            ];

            // Act
            let entries = resolve_paths(&patterns, base_path).unwrap();

            // Assert: 3 件ではなく 1 件。重複したまま tar へ渡すと同じパスのエントリが
            // 複数書かれ、アーカイブサイズも内容ハッシュもパターンの書き方に左右される
            assert_eq!(archive_paths(&entries), vec!["foo.txt"]);
        }

        #[test]
        fn test_result_is_sorted_regardless_of_pattern_order() {
            // Arrange: 作成順もパターンの並びも昇順ではない状態を作る。
            // ファイルシステムの列挙順は環境依存なので、そこに引きずられないことを見たい
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            for name in ["c.txt", "a.txt", "b.txt"] {
                std::fs::write(base_path.join(name), name).unwrap();
            }
            let patterns_forward = vec![
                "a.txt".to_string(),
                "b.txt".to_string(),
                "c.txt".to_string(),
            ];
            let patterns_reversed = vec![
                "c.txt".to_string(),
                "b.txt".to_string(),
                "a.txt".to_string(),
            ];

            // Act: パターンの並びだけを逆にして 2 回解決する
            let forward = resolve_paths(&patterns_forward, base_path).unwrap();
            let reversed = resolve_paths(&patterns_reversed, base_path).unwrap();

            // Assert: 常にバイト列昇順で、かつ入力順に依存しない。
            // ここが崩れると同じ内容から違うアーカイブが生まれ、
            // 無駄な再アップロードが起きる（決定論性の前提）
            assert_eq!(archive_paths(&forward), vec!["a.txt", "b.txt", "c.txt"]);
            assert_eq!(forward, reversed);
        }

        #[test]
        fn test_empty_patterns_is_error() {
            // Arrange: config に `paths = []` と書いた（あるいは書き忘れた）状態
            let temp_dir = tempfile::tempdir().unwrap();
            let patterns: std::vec::Vec<String> = vec![];

            // Act
            let result = resolve_paths(&patterns, temp_dir.path());

            // Assert: 空アーカイブを置くと、以後の restore が「ヒットしたのに何も復元されない」
            // 状態になり原因追跡が難しい。store の入口で落とす
            assert!(result.unwrap_err().to_string().contains("paths が空です"));
        }

        #[test]
        fn test_no_match_is_error() {
            // Arrange: パターンは書かれているのに 1 件もマッチしない
            // （設定ミス、あるいは前段のビルドが成果物を作れなかった状況）
            let temp_dir = tempfile::tempdir().unwrap();
            let patterns = vec!["does-not-exist/**".to_string()];

            // Act
            let result = resolve_paths(&patterns, temp_dir.path());

            // Assert: `key.files` の 0 件マッチは GitLab CI 互換のため `default` キーへ倒すが、
            // キャッシュ本体の 0 件マッチは別の判断軸としてエラーにする。
            // ビルド失敗を「正常な空キャッシュ」として固定してしまわないため
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("1 件もマッチしませんでした"));
        }

        #[test]
        fn test_absolute_pattern_is_rejected() {
            // Arrange: 基準ディレクトリと無関係な場所を指す絶対パス
            let temp_dir = tempfile::tempdir().unwrap();
            #[cfg(not(windows))]
            let patterns = vec!["/etc/passwd".to_string()];
            #[cfg(windows)]
            let patterns = vec!["C:\\Windows\\win.ini".to_string()];

            // Act
            let result = resolve_paths(&patterns, temp_dir.path());

            // Assert: 許すとキャッシュに機密ファイルを吸い上げて S3 へ送れてしまう。
            // `key.files` と同じくパターンの段階で拒否する
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("絶対パスのパターンは指定できません"));
        }

        #[test]
        fn test_parent_dir_escape_is_rejected() {
            // Arrange: 相対パスのまま基準ディレクトリの外へ出るパターン。
            // 実際に外側へ読まれうるファイルを置いて、拾われないことを見る
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path().join("base");
            std::fs::create_dir_all(&base_path).unwrap();
            std::fs::write(temp_dir.path().join("outside.txt"), "secret").unwrap();
            let patterns = vec!["../outside.txt".to_string()];

            // Act
            let result = resolve_paths(&patterns, &base_path);

            // Assert: FileMatcher は基準外のマッチを黙って無視するが、
            // paths では設定ミスに気づけるようエラーにする
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("基準ディレクトリの外を指すパス"));
        }

        #[test]
        fn test_parent_dir_in_middle_is_rejected() {
            // Arrange: `sub/../foo.txt` は畳めば `foo.txt` で基準内に収まる。
            // それでも拒否するのは、`..` を一切許さない方が判定を単純に保てるためで、
            // 利用者は `foo.txt` と書き直すだけで済む
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::create_dir_all(base_path.join("sub")).unwrap();
            std::fs::write(base_path.join("foo.txt"), "content").unwrap();
            let patterns = vec!["sub/../foo.txt".to_string()];

            // Act
            let result = resolve_paths(&patterns, base_path);

            // Assert: 「結果が基準内なら通す」ではなく、パターンに `..` があれば入口で弾く
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("基準ディレクトリの外を指すパス"));
        }

        #[test]
        fn test_base_path_with_glob_meta_chars() {
            // Arrange: 基準ディレクトリ名に `[`・`]`（glob の character class メタ文字）が入る。
            // パターンではなく基準パス側にメタ文字がある場合の話で、
            // エスケープを忘れると `build[1]` が「build のうち 1 文字」として解釈され 0 件になる
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path().join("build[1]");
            std::fs::create_dir_all(&base_path).unwrap();
            std::fs::write(base_path.join("foo.txt"), "content").unwrap();
            let patterns = vec!["foo.txt".to_string()];

            // Act
            let entries = resolve_paths(&patterns, &base_path).unwrap();

            // Assert: エスケープが効いていれば普通に 1 件見つかる
            assert_eq!(archive_paths(&entries), vec!["foo.txt"]);
        }

        #[cfg(unix)]
        #[test]
        fn test_symlink_to_file_is_not_followed() {
            // Arrange: リンクだけをパターンに指定する（リンク先の real.txt は指定しない）
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::write(base_path.join("real.txt"), "content").unwrap();
            std::os::unix::fs::symlink("real.txt", base_path.join("link.txt")).unwrap();
            let patterns = vec!["link.txt".to_string()];

            // Act
            let entries = resolve_paths(&patterns, base_path).unwrap();

            // Assert: 辿ってしまうと File として実体の中身を重複格納することになる。
            // Symlink として入ることで、復元時もリンクのまま再現できる
            assert_eq!(archive_paths(&entries), vec!["link.txt"]);
            assert_eq!(kind_of(&entries, "link.txt"), &EntryKind::Symlink);
        }

        #[cfg(unix)]
        #[test]
        fn test_symlink_to_directory_is_not_traversed() {
            // Arrange: ディレクトリへのリンク。辿ると walkdir が配下まで降りてしまう
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::create_dir_all(base_path.join("real_dir")).unwrap();
            std::fs::write(base_path.join("real_dir/inner.txt"), "content").unwrap();
            std::os::unix::fs::symlink("real_dir", base_path.join("link_dir")).unwrap();
            let patterns = vec!["link_dir".to_string()];

            // Act
            let entries = resolve_paths(&patterns, base_path).unwrap();

            // Assert: `link_dir/inner.txt` は入らない。辿る実装だと、リンク先が
            // 基準ディレクトリの外にある場合に想定外のファイルを持ち出すことになる。
            // 判定に `is_dir()` ではなく `symlink_metadata` を使っているのがここで効く
            assert_eq!(archive_paths(&entries), vec!["link_dir"]);
            assert_eq!(kind_of(&entries, "link_dir"), &EntryKind::Symlink);
        }

        #[cfg(unix)]
        #[test]
        fn test_symlink_inside_directory_is_kept_as_link() {
            // Arrange: 再帰展開の途中で出てくるリンク。上の 2 つはパターンが直接
            // リンクを指す場合で、こちらは walkdir が見つける場合の経路
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::create_dir_all(base_path.join("dir")).unwrap();
            std::fs::write(base_path.join("dir/real.txt"), "content").unwrap();
            std::os::unix::fs::symlink("real.txt", base_path.join("dir/link.txt")).unwrap();
            let patterns = vec!["dir".to_string()];

            // Act
            let entries = resolve_paths(&patterns, base_path).unwrap();

            // Assert: リンクと実体の両方が別エントリとして入り、リンク側は Symlink のまま
            assert_eq!(
                archive_paths(&entries),
                vec!["dir", "dir/link.txt", "dir/real.txt"]
            );
            assert_eq!(kind_of(&entries, "dir/link.txt"), &EntryKind::Symlink);
        }
    }
}
