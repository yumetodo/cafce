//! 決定論的な tar + zstd アーカイブの生成と展開
//!
//! ローカル I/O のみを扱い、S3 を知らない。S3 の I/O 無しに単体テストできる境界に置く。
//!
//! zstd フレーム自体はタイムスタンプを持たないため、非決定性の発生源は tar 側にある。
//! 生成時は以下を正規化することで、同一内容の入力からは常に同一バイト列を得る
//! （同一プラットフォーム上で、という限定付き。設計doc の Non-Goal を参照）。
//!
//! | 項目 | 正規化の内容 |
//! |---|---|
//! | エントリ順 | tar のエントリ名（`/` 区切り）のバイト列昇順 |
//! | mtime | 固定値 `0`（UNIX epoch） |
//! | uid / gid | `0` |
//! | uname / gname | 空文字 |
//! | パーミッション | ファイルは owner の実行ビット有無で `0o755` / `0o644`、ディレクトリは `0o755` |
//! | tar フォーマット | GNU 形式（pax 拡張ヘッダに atime/ctime を載せない） |
//! | zstd | 圧縮レベル固定、シングルスレッド、フレームチェックサム有効 |

/// zstd の圧縮レベル
///
/// レベルが変われば出力バイト列が変わるため固定する。
const ZSTD_LEVEL: i32 = 3;

/// 単一 `PutObject` で送れるオブジェクトサイズの上限（5 GiB）
///
/// cafce はマルチパートアップロードに未対応のため、これを超えたら明示的なエラーにする。
pub const MAX_SINGLE_PUT_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// 実行可能ファイル・ディレクトリのパーミッション
const MODE_EXECUTABLE: u32 = 0o755;

/// 実行可能でない通常ファイルのパーミッション
const MODE_REGULAR: u32 = 0o644;

/// 生成したアーカイブとその 2 種類のハッシュ
pub struct BuiltArchive {
    /// tar.zst を書き出した一時ファイル。drop されると削除される
    ///
    /// メモリ上に全体を載せず、そのまま `ByteStream::from_path` でアップロードする。
    pub temp_file: tempfile::NamedTempFile,

    /// 圧縮前の tar ストリーム全体の SHA-256（小文字 16 進 64 文字）
    ///
    /// **内容の同一性**を表すハッシュ。zstd の版数・レベルが変わっても変わらない。
    /// S3 user metadata の `cafce-content-sha256` として記録し、再アップロードの抑止に使う。
    pub content_sha256: String,

    /// 圧縮後バイト列（オブジェクト本体）の SHA-256（生の 32 バイト）
    ///
    /// S3 フレキシブルチェックサム（`x-amz-checksum-sha256`）用。転送・保管の破損検出に使う。
    pub object_sha256: [u8; 32],

    /// 圧縮後バイト列のサイズ
    pub size: u64,
}

/// 書き込まれたバイト列の SHA-256 を計算しつつ、内側の writer へそのまま流す
///
/// tar ストリームと圧縮後バイト列の 2 本のハッシュを、1 パスの中で同時に得るために使う。
struct HashingWriter<W: std::io::Write> {
    inner: W,
    hasher: sha2::Sha256,
}

impl<W: std::io::Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        use sha2::Digest as _;
        Self {
            inner,
            hasher: sha2::Sha256::new(),
        }
    }

    fn into_parts(self) -> (W, sha2::Sha256) {
        (self.inner, self.hasher)
    }
}

impl<W: std::io::Write> std::io::Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest as _;
        // 部分書き込みに備え、実際に書けたバイトだけをハッシュ対象にする
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// 読み出したバイト列の SHA-256 を計算しつつ、内側の reader からそのまま返す
struct HashingReader<R: std::io::Read> {
    inner: R,
    hasher: sha2::Sha256,
}

impl<R: std::io::Read> HashingReader<R> {
    fn new(inner: R) -> Self {
        use sha2::Digest as _;
        Self {
            inner,
            hasher: sha2::Sha256::new(),
        }
    }
}

impl<R: std::io::Read> std::io::Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use sha2::Digest as _;
        let read = self.inner.read(buf)?;
        self.hasher.update(&buf[..read]);
        Ok(read)
    }
}

fn to_hex(digest: sha2::digest::Output<sha2::Sha256>) -> String {
    format!("{digest:x}")
}

/// ファイルの実行ビットの有無からパーミッションを丸める
///
/// umask 差による揺れを断ちつつ、実行可能ファイルの実行ビットは保つ。
/// Windows では実行ビットを取得できないため、常に `0o644` になる（設計doc の Non-Goal）。
fn normalized_file_mode(metadata: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o100 != 0 {
            MODE_EXECUTABLE
        } else {
            MODE_REGULAR
        }
    }
    #[cfg(not(unix))]
    {
        let _unused = metadata;
        MODE_REGULAR
    }
}

/// 正規化済みの tar ヘッダを作る
fn new_normalized_header(entry_type: tar::EntryType, mode: u32, size: u64) -> tar::Header {
    // GNU 形式にするのは、pax 拡張ヘッダに atime/ctime を載せさせないため
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_mode(mode);
    header.set_size(size);
    // 実行時刻・checkout 時刻への依存を断つ
    header.set_mtime(0);
    // 実行ユーザー依存を断つ（uname / gname は new_gnu のゼロ埋めで既に空文字）
    header.set_uid(0);
    header.set_gid(0);
    header
}

fn append_entry<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    entry: &crate::path_matcher::ArchiveEntry,
    base_path: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let absolute_path = base_path.join(&entry.relative_path);

    match entry.kind {
        crate::path_matcher::EntryKind::Directory => {
            let mut header = new_normalized_header(tar::EntryType::Directory, MODE_EXECUTABLE, 0);
            builder
                .append_data(&mut header, &entry.archive_path, std::io::empty())
                .with_context(|| {
                    format!("ディレクトリの追加に失敗しました: {}", entry.archive_path)
                })?;
        }
        crate::path_matcher::EntryKind::Symlink => {
            let target = std::fs::read_link(&absolute_path).with_context(|| {
                format!(
                    "シンボリックリンクの読み取りに失敗しました: {}",
                    absolute_path.display()
                )
            })?;
            // シンボリックリンク自身のパーミッションは多くの環境で無意味だが、
            // 値が揺れないよう固定する
            let mut header = new_normalized_header(tar::EntryType::Symlink, 0o777, 0);
            builder
                .append_link(&mut header, &entry.archive_path, &target)
                .with_context(|| {
                    format!(
                        "シンボリックリンクの追加に失敗しました: {}",
                        entry.archive_path
                    )
                })?;
        }
        crate::path_matcher::EntryKind::File => {
            let file = std::fs::File::open(&absolute_path).with_context(|| {
                format!("ファイルを開けませんでした: {}", absolute_path.display())
            })?;
            let metadata = file.metadata().with_context(|| {
                format!(
                    "メタデータの取得に失敗しました: {}",
                    absolute_path.display()
                )
            })?;
            let mut header = new_normalized_header(
                tar::EntryType::Regular,
                normalized_file_mode(&metadata),
                metadata.len(),
            );
            builder
                .append_data(&mut header, &entry.archive_path, file)
                .with_context(|| format!("ファイルの追加に失敗しました: {}", entry.archive_path))?;
        }
    }

    Ok(())
}

/// エントリ列から決定論的な tar + zstd アーカイブを一時ファイルへ生成する
///
/// tar ストリームの SHA-256（内容ハッシュ）と圧縮後バイト列の SHA-256（転送チェックサム）を
/// 同時に計算する。データを流すパスは 1 回きりで、ファイルの読み直しは発生しない。
///
/// `entries` は `path_matcher::resolve_paths` が既にソート済みだが、この関数単体で呼ばれても
/// 結果が入力順序に依存しないよう、常に自前でソートし直す契約とする。
pub fn create_archive(
    entries: &[crate::path_matcher::ArchiveEntry],
    base_path: &std::path::Path,
) -> anyhow::Result<BuiltArchive> {
    use anyhow::Context as _;
    use sha2::Digest as _;
    use std::io::Write as _;

    let mut sorted_entries = entries.to_vec();
    sorted_entries.sort_by(|a, b| a.archive_path.as_bytes().cmp(b.archive_path.as_bytes()));

    let temp_file = tempfile::NamedTempFile::new()
        .context("アーカイブ用の一時ファイルを作成できませんでした")?;

    // 一時ファイル → 圧縮後バイト列のハッシュ器 → zstd エンコーダ → tar ストリームのハッシュ器 → tar
    let compressed_writer = HashingWriter::new(std::io::BufWriter::new(
        temp_file
            .reopen()
            .context("一時ファイルを書き込み用に開けませんでした")?,
    ));
    let mut encoder = zstd::Encoder::new(compressed_writer, ZSTD_LEVEL)
        .context("zstd エンコーダの初期化に失敗しました")?;
    encoder
        .include_checksum(true)
        .context("zstd フレームチェックサムの有効化に失敗しました")?;
    // zstd crate は既定でシングルスレッド。マルチスレッド化は出力バイト列を変えるため行わない。
    let mut builder = tar::Builder::new(HashingWriter::new(encoder));

    for entry in &sorted_entries {
        append_entry(&mut builder, entry, base_path)?;
    }

    let tar_writer = builder
        .into_inner()
        .context("tar アーカイブの終端処理に失敗しました")?;
    let (encoder, tar_hasher) = tar_writer.into_parts();
    let compressed_writer = encoder
        .finish()
        .context("zstd ストリームの終端処理に失敗しました")?;
    let (mut buffered_file, compressed_hasher) = compressed_writer.into_parts();
    buffered_file
        .flush()
        .context("一時ファイルへの書き出しに失敗しました")?;
    let mut file = buffered_file
        .into_inner()
        .context("一時ファイルのバッファ解放に失敗しました")?;
    file.flush()
        .context("一時ファイルへの書き出しに失敗しました")?;

    let size = file
        .metadata()
        .context("一時ファイルのサイズ取得に失敗しました")?
        .len();
    if size > MAX_SINGLE_PUT_SIZE {
        return Err(crate::error::ArchiveError::TooLargeForSinglePut {
            size,
            limit: MAX_SINGLE_PUT_SIZE,
        }
        .into());
    }

    let object_sha256: [u8; 32] = compressed_hasher.finalize().into();

    log::info!(
        "アーカイブを生成しました: {} エントリ, {} bytes",
        sorted_entries.len(),
        size
    );

    Ok(BuiltArchive {
        temp_file,
        content_sha256: to_hex(tar_hasher.finalize()),
        object_sha256,
        size,
    })
}

/// tar のエントリ名を展開先の絶対パスへ解決する（いわゆる zip-slip 対策）
///
/// 絶対パス、あるいは正規化後に基準ディレクトリの外を指すエントリは拒否する。
/// 基準ディレクトリ自身を指す場合は `Ok(None)` を返す（作成対象が無い）。
fn resolve_entry_destination(
    entry_path: &std::path::Path,
    base_path: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, crate::error::ArchiveError> {
    if entry_path.is_absolute() {
        return Err(crate::error::ArchiveError::AbsoluteEntryPath {
            path: entry_path.to_string_lossy().into_owned(),
        });
    }

    let normalized_base = crate::path_matcher::normalize_lexically(base_path);
    let destination = crate::path_matcher::normalize_lexically(&base_path.join(entry_path));

    if destination == normalized_base {
        return Ok(None);
    }
    if !destination.starts_with(&normalized_base) {
        return Err(crate::error::ArchiveError::EntryEscapesBaseDirectory {
            path: entry_path.to_string_lossy().into_owned(),
        });
    }

    Ok(Some(destination))
}

/// シンボリックリンクのリンク先が展開先ディレクトリの外を指していないか確認する
fn check_symlink_target(
    destination: &std::path::Path,
    target: &std::path::Path,
    base_path: &std::path::Path,
) -> Result<(), crate::error::ArchiveError> {
    let normalized_base = crate::path_matcher::normalize_lexically(base_path);

    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        // リンクは自身が置かれるディレクトリを基準に解決される
        let link_dir = destination.parent().unwrap_or(base_path);
        link_dir.join(target)
    };
    let resolved = crate::path_matcher::normalize_lexically(&resolved);

    if !resolved.starts_with(&normalized_base) {
        return Err(crate::error::ArchiveError::SymlinkEscapesBaseDirectory {
            path: destination.to_string_lossy().into_owned(),
            target: target.to_string_lossy().into_owned(),
        });
    }

    Ok(())
}

#[cfg(unix)]
fn apply_mode(path: &std::path::Path, mode: u32) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("パーミッションの設定に失敗しました: {}", path.display()))
}

#[cfg(not(unix))]
fn apply_mode(_path: &std::path::Path, _mode: u32) -> anyhow::Result<()> {
    // Windows には Unix パーミッションの概念が無いため何もしない
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &std::path::Path, destination: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;

    std::os::unix::fs::symlink(target, destination).with_context(|| {
        format!(
            "シンボリックリンクの作成に失敗しました: {} -> {}",
            destination.display(),
            target.display()
        )
    })
}

/// Windows でシンボリックリンクを作る
///
/// `CreateSymbolicLinkW` はリンク先がファイルかディレクトリかを作成時に指定する必要があり、
/// Rust の `std` もそれを `symlink_file` / `symlink_dir` の 2 関数に分けている。そのため
/// リンク先を解決して種別を判定する。この判定を成立させるために、呼び出し側は全エントリの
/// 展開が終わってからシンボリックリンクを作る（`extract_archive` の遅延パス）。
///
/// 非昇格プロセスでも、開発者モードが有効なら作成できる。`std` が
/// `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE` を渡してくれるためである
/// （フラグを解さない古い Windows では `std` 側がフラグ無しで再試行する）。
/// 開発者モードでも `SeCreateSymbolicLinkPrivilege` でもない場合は作成できないため、
/// 黙ってスキップせずエラーにする。中身が欠けたキャッシュを正常な復元として返すと、
/// 後続のビルドが不可解な形で失敗するためである。
#[cfg(windows)]
fn create_symlink(target: &std::path::Path, destination: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let resolved_target = match destination.parent() {
        Some(link_dir) => link_dir.join(target),
        None => target.to_path_buf(),
    };

    let result = if resolved_target.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    };

    result.with_context(|| {
        format!(
            "シンボリックリンクの作成に失敗しました: {} -> {}\n\
             Windows でシンボリックリンクを作るには、開発者モードを有効にするか、\n\
             SeCreateSymbolicLinkPrivilege を持つ（管理者として実行する）必要があります",
            destination.display(),
            target.display()
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(target: &std::path::Path, destination: &std::path::Path) -> anyhow::Result<()> {
    // シンボリックリンクを作る標準 API が無いプラットフォーム（wasm 等）
    log::warn!(
        "このプラットフォームにはシンボリックリンクを作る API が無いためスキップします: {} -> {}",
        destination.display(),
        target.display()
    );
    Ok(())
}

/// 既存のシンボリックリンクを消す
///
/// Unix の `unlink(2)` はリンク先の種別によらずシンボリックリンクを消せる。
#[cfg(unix)]
fn remove_symlink(path: &std::path::Path, _file_type: &std::fs::FileType) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

/// 既存のシンボリックリンクを消す
///
/// Windows の `remove_file` は `DeleteFileW` であり、**ディレクトリへのシンボリックリンクを
/// 消せない**（`RemoveDirectoryW` が必要）。そのため種別で使い分ける。
/// `remove_dir` はリンク自体（reparse point）を消すだけで、リンク先の中身には触らない。
#[cfg(windows)]
fn remove_symlink(path: &std::path::Path, file_type: &std::fs::FileType) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt as _;

    if file_type.is_symlink_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(not(any(unix, windows)))]
fn remove_symlink(path: &std::path::Path, _file_type: &std::fs::FileType) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

/// シンボリックリンクを作る前に、同じパスにある既存のエントリを取り除く
///
/// `symlink(2)` も `CreateSymbolicLinkW` も、既存のパスに対しては失敗するため先に消す。
///
/// 実ディレクトリがあった場合だけはエラーにする。置き換えるには木ごと再帰削除するしかなく、
/// cafce が作ったとは限らないディレクトリを黙って消すのは影響が大きすぎるためである
/// （通常ファイル・ディレクトリのエントリでも、種別が食い違えば OS のエラーで落ちるので
/// 挙動としては揃っている）。
fn remove_existing_symlink_destination(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        // 存在しないなら何もしなくてよい
        Err(_) => return Ok(()),
    };
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        return remove_symlink(path, &file_type).with_context(|| {
            format!(
                "既存のシンボリックリンクの削除に失敗しました: {}",
                path.display()
            )
        });
    }

    if file_type.is_dir() {
        return Err(crate::error::ArchiveError::SymlinkDestinationIsDirectory {
            path: path.to_string_lossy().into_owned(),
        }
        .into());
    }

    std::fs::remove_file(path)
        .with_context(|| format!("既存ファイルの削除に失敗しました: {}", path.display()))
}

/// 展開後に作るシンボリックリンク（作成先, リンク先）
type DeferredSymlink = (std::path::PathBuf, std::path::PathBuf);

fn extract_one_entry<R: std::io::Read>(
    entry: &mut tar::Entry<'_, R>,
    base_path: &std::path::Path,
    deferred_symlinks: &mut std::vec::Vec<DeferredSymlink>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let entry_path = entry
        .path()
        .context("tar エントリのパスを解釈できませんでした")?
        .into_owned();

    let destination = match resolve_entry_destination(&entry_path, base_path)? {
        Some(destination) => destination,
        None => return Ok(()),
    };
    let mode = entry.header().mode().unwrap_or(MODE_REGULAR);

    match entry.header().entry_type() {
        tar::EntryType::Directory => {
            std::fs::create_dir_all(&destination).with_context(|| {
                format!(
                    "ディレクトリの作成に失敗しました: {}",
                    destination.display()
                )
            })?;
            apply_mode(&destination, mode)?;
        }
        tar::EntryType::Regular => {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("親ディレクトリの作成に失敗しました: {}", parent.display())
                })?;
            }
            // キャッシュ復元の意味論として、既にあるものを残す方が事故になりやすいため上書きする。
            // 自前で書き出すことで mtime は展開時刻になり、mtime ベースの差分ビルドと噛み合う
            // （アーカイブ内の mtime は epoch 固定である。設計doc 6.5）
            let mut file = std::fs::File::create(&destination).with_context(|| {
                format!("ファイルの作成に失敗しました: {}", destination.display())
            })?;
            std::io::copy(entry, &mut file).with_context(|| {
                format!(
                    "ファイルの書き出しに失敗しました: {}",
                    destination.display()
                )
            })?;
            drop(file);
            apply_mode(&destination, mode)?;
        }
        tar::EntryType::Symlink => {
            let target = entry
                .link_name()
                .context("シンボリックリンクのリンク先を解釈できませんでした")?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "シンボリックリンクのリンク先が空です: {}",
                        destination.display()
                    )
                })?
                .into_owned();
            check_symlink_target(&destination, &target, base_path)?;
            // 作成は全エントリの展開後にまとめて行う（Windows ではリンク先がファイルか
            // ディレクトリかを作成時に指定する必要があり、tar のエントリ順では
            // リンク先がまだ存在しないことがあるため）
            deferred_symlinks.push((destination, target));
        }
        other => {
            log::warn!(
                "対応していないエントリ型のためスキップします: {} ({other:?})",
                entry_path.display()
            );
        }
    }

    Ok(())
}

/// tar + zstd アーカイブを基準ディレクトリへ展開し、tar ストリームの SHA-256 を返す
///
/// 戻り値は `create_archive` の `content_sha256` と比較できる小文字 16 進 64 文字。
///
/// 展開の安全性のため、以下を拒否する:
///
/// - 絶対パスのエントリ、正規化後に基準ディレクトリの外を指すエントリ
/// - リンク先が基準ディレクトリの外を指すシンボリックリンク
///
/// 通常ファイル・ディレクトリ・シンボリックリンク以外のエントリ型はスキップして警告を出す。
///
/// シンボリックリンクはファイル・ディレクトリを全て展開し終えてから作る。Windows の
/// `CreateSymbolicLinkW` はリンク先がファイルかディレクトリかを作成時に指定する必要があり、
/// tar のエントリ順（パスのバイト列昇順）ではリンク先が後から現れることがあるためである。
pub fn extract_archive(
    archive_path: &std::path::Path,
    base_path: &std::path::Path,
) -> anyhow::Result<String> {
    use anyhow::Context as _;
    use sha2::Digest as _;

    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("アーカイブを開けませんでした: {}", archive_path.display()))?;
    let decoder = zstd::Decoder::new(std::io::BufReader::new(file))
        .context("zstd デコーダの初期化に失敗しました")?;
    let mut archive = tar::Archive::new(HashingReader::new(decoder));

    let mut extracted_count: usize = 0;
    let mut deferred_symlinks: std::vec::Vec<DeferredSymlink> = std::vec::Vec::new();
    for entry in archive
        .entries()
        .context("tar エントリの列挙に失敗しました")?
    {
        let mut entry = entry.context("tar エントリの読み出しに失敗しました")?;
        extract_one_entry(&mut entry, base_path, &mut deferred_symlinks)?;
        extracted_count += 1;
    }

    // 内容ハッシュは tar ストリーム全体に対して取るため、終端の 0 ブロックまで読み切る
    let mut hashing_reader = archive.into_inner();
    std::io::copy(&mut hashing_reader, &mut std::io::sink())
        .context("tar ストリームの読み切りに失敗しました")?;

    for (destination, target) in &deferred_symlinks {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("親ディレクトリの作成に失敗しました: {}", parent.display())
            })?;
        }
        remove_existing_symlink_destination(destination)?;
        create_symlink(target, destination)?;
    }

    log::info!(
        "アーカイブを展開しました: {extracted_count} エントリ（うちシンボリックリンク {} 件）",
        deferred_symlinks.len()
    );

    Ok(to_hex(hashing_reader.hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 基準ディレクトリ配下のファイル木からアーカイブを生成する
    ///
    /// `path_matcher` を通すのは本番と同じ経路（ソート済みエントリ）を再現するため。
    fn build_from(base_path: &std::path::Path, patterns: &[&str]) -> BuiltArchive {
        let patterns: std::vec::Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        let entries = crate::path_matcher::resolve_paths(&patterns, base_path)
            .expect("パターンの解決に失敗しました");
        create_archive(&entries, base_path).expect("アーカイブの生成に失敗しました")
    }

    fn read_archive_bytes(archive: &BuiltArchive) -> std::vec::Vec<u8> {
        std::fs::read(archive.temp_file.path()).expect("アーカイブの読み出しに失敗しました")
    }

    /// tar ストリームを展開せずヘッダだけ読む
    ///
    /// 正規化の確認は「展開結果」ではなく「ヘッダに書かれた値」を見る必要がある。
    /// 展開後のファイル属性は復元時刻や umask に左右されるが、決定論性を左右するのは
    /// あくまでアーカイブの中身であるため。
    fn read_headers(archive: &BuiltArchive) -> std::vec::Vec<tar::Header> {
        let file = std::fs::File::open(archive.temp_file.path()).unwrap();
        let decoder = zstd::Decoder::new(std::io::BufReader::new(file)).unwrap();
        let mut tar_archive = tar::Archive::new(decoder);
        tar_archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().header().clone())
            .collect()
    }

    mod determinism_tests {
        use super::*;

        #[test]
        fn test_same_input_produces_identical_bytes() {
            // Arrange: ディレクトリとファイルを混ぜた木（ヘッダ種別が 2 種類出る）
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::create_dir_all(base_path.join("dir")).unwrap();
            std::fs::write(base_path.join("dir/a.txt"), "content-a").unwrap();
            std::fs::write(base_path.join("b.txt"), "content-b").unwrap();

            // Act: 同じ入力から 2 回生成する
            let first = build_from(base_path, &["dir", "b.txt"]);
            let second = build_from(base_path, &["dir", "b.txt"]);

            // Assert: バイト列まで一致すること。ハッシュだけの比較では
            // tar ヘッダに実行時刻が混ざる退行を取り逃す（ハッシュ対象外の
            // フィールドがあれば気づけない）ため、生バイトも比べる
            assert_eq!(read_archive_bytes(&first), read_archive_bytes(&second));
            assert_eq!(first.content_sha256, second.content_sha256);
            assert_eq!(first.object_sha256, second.object_sha256);
        }

        #[test]
        fn test_mtime_change_does_not_change_archive() {
            // Arrange: CI は毎回 fresh checkout するためソースの mtime が実行ごとに変わる。
            // それでも再アップロードが起きないことがこのテストの主題
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::write(base_path.join("a.txt"), "content").unwrap();
            let before = build_from(base_path, &["a.txt"]);

            // Act: 同じ内容を書き直して mtime だけを進める
            // （sleep は mtime の解像度でタイムスタンプが同値になるのを避けるため）
            std::thread::sleep(std::time::Duration::from_millis(10));
            std::fs::write(base_path.join("a.txt"), "content").unwrap();
            let after = build_from(base_path, &["a.txt"]);

            // Assert: ヘッダの mtime を epoch 固定にしている効果。ここが崩れると
            // アップロード抑止の仕組みそのものが機能しなくなる
            assert_eq!(read_archive_bytes(&before), read_archive_bytes(&after));
            assert_eq!(before.content_sha256, after.content_sha256);
        }

        #[test]
        fn test_entry_order_does_not_change_archive() {
            // Arrange: path_matcher はソート済みを返すので、意図的に逆順の入力を作る。
            // path_matcher を経由しない呼び出しでも決定論性が保たれるかを見たい
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::write(base_path.join("a.txt"), "content-a").unwrap();
            std::fs::write(base_path.join("b.txt"), "content-b").unwrap();
            let patterns = vec!["a.txt".to_string(), "b.txt".to_string()];
            let entries = crate::path_matcher::resolve_paths(&patterns, base_path).unwrap();
            let mut reversed_entries = entries.clone();
            reversed_entries.reverse();

            // Act
            let forward = create_archive(&entries, base_path).unwrap();
            let reversed = create_archive(&reversed_entries, base_path).unwrap();

            // Assert: create_archive 自身が常にソートし直す契約（HashCalculator と同じ方針）。
            // 呼び出し側のソート漏れに決定論性を依存させない
            assert_eq!(read_archive_bytes(&forward), read_archive_bytes(&reversed));
        }

        #[test]
        fn test_content_change_changes_hashes() {
            // Arrange: 上の 2 つは「変わらないこと」を見ているので、
            // 正規化しすぎて内容の違いまで潰していないことを対で確認する
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::write(base_path.join("a.txt"), "content-a").unwrap();
            let before = build_from(base_path, &["a.txt"]);

            // Act: 中身を書き換える
            std::fs::write(base_path.join("a.txt"), "content-modified").unwrap();
            let after = build_from(base_path, &["a.txt"]);

            // Assert: 2 本のハッシュがどちらも変わる。content 側が変わらなければ
            // 内容が変わってもアップロードが省略されてしまう
            assert_ne!(before.content_sha256, after.content_sha256);
            assert_ne!(before.object_sha256, after.object_sha256);
        }

        #[test]
        fn test_content_sha256_is_lowercase_hex_64() {
            // Arrange
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::write(base_path.join("a.txt"), "content").unwrap();

            // Act
            let archive = build_from(base_path, &["a.txt"]);

            // Assert: この値は S3 metadata の cafce-content-sha256 としてそのまま入る。
            // cache_metadata 側は小文字 16 進 64 文字のみを受け付けるため、
            // 大文字混じりや長さ違いになると restore が MalformedContentHash で落ちる
            assert_eq!(archive.content_sha256.len(), 64);
            assert!(archive
                .content_sha256
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        }

        #[test]
        fn test_object_sha256_matches_archive_bytes() {
            // Arrange
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::write(base_path.join("a.txt"), "content").unwrap();
            let archive = build_from(base_path, &["a.txt"]);

            // Act: 書き込みと同時に計算した値ではなく、一時ファイルを読み直して独立に求める
            use sha2::Digest as _;
            let bytes = read_archive_bytes(&archive);
            let recomputed: [u8; 32] = sha2::Sha256::digest(&bytes).into();

            // Assert: object_sha256 は S3 へ x-amz-checksum-sha256 として送る値であり、
            // 実際に送られるバイト列（= 一時ファイルの中身）と食い違えばサーバに
            // BadDigest で拒否される。HashingWriter が部分書き込みを取りこぼしていない
            // ことの確認も兼ねる
            assert_eq!(archive.object_sha256, recomputed);
            assert_eq!(archive.size, bytes.len() as u64);
        }
    }

    mod header_normalization_tests {
        use super::*;

        #[test]
        fn test_mtime_uid_gid_are_zeroed() {
            // Arrange: ディレクトリとファイルの両方のヘッダを対象にする
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::create_dir_all(base_path.join("dir")).unwrap();
            std::fs::write(base_path.join("dir/a.txt"), "content").unwrap();
            let archive = build_from(base_path, &["dir"]);

            // Act
            let headers = read_headers(&archive);

            // Assert: 全エントリで実行環境依存の値がゼロに潰れていること。
            // uid/gid/uname は「誰が CI を回したか」で変わり、mtime は「いつ回したか」で変わる。
            // 1 つでも残るとホスト間・実行間でハッシュが割れる
            assert!(!headers.is_empty());
            for header in &headers {
                assert_eq!(header.mtime().unwrap(), 0);
                assert_eq!(header.uid().unwrap(), 0);
                assert_eq!(header.gid().unwrap(), 0);
                // GNU ヘッダの uname / gname はゼロ埋めのため空文字として読める
                assert_eq!(header.username().unwrap(), Some(""));
                assert_eq!(header.groupname().unwrap(), Some(""));
            }
        }

        #[cfg(unix)]
        #[test]
        fn test_permissions_are_rounded() {
            // Arrange: 実行ビットの有無だけで 2 値に丸める仕様の確認。
            // 0o640 / 0o700 のような中途半端な mode を与え、丸めた結果が
            // 0o644 / 0o755 になることを見る
            use std::os::unix::fs::PermissionsExt as _;
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::create_dir_all(base_path.join("dir")).unwrap();
            std::fs::write(base_path.join("dir/plain.txt"), "content").unwrap();
            std::fs::write(base_path.join("dir/exec.sh"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(
                base_path.join("dir/plain.txt"),
                std::fs::Permissions::from_mode(0o640),
            )
            .unwrap();
            std::fs::set_permissions(
                base_path.join("dir/exec.sh"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            let archive = build_from(base_path, &["dir"]);

            // Act
            let headers = read_headers(&archive);
            let mode_of = |name: &str| {
                headers
                    .iter()
                    .find(|h| h.path().unwrap().to_string_lossy() == name)
                    .unwrap_or_else(|| panic!("エントリ {name} が見つかりません"))
                    .mode()
                    .unwrap()
            };

            // Assert: 実行ビットは保つ（ビルド成果物のバイナリが復元後に実行できなくなると困る）。
            // ディレクトリは中身を辿れるよう常に 0o755
            assert_eq!(mode_of("dir"), MODE_EXECUTABLE);
            assert_eq!(mode_of("dir/plain.txt"), MODE_REGULAR);
            assert_eq!(mode_of("dir/exec.sh"), MODE_EXECUTABLE);
        }

        #[cfg(unix)]
        #[test]
        fn test_umask_difference_does_not_change_archive() {
            // Arrange: umask の違う 2 台のランナーを模す。同じ内容・同じ実行ビット（どちらも
            // 実行不可）で、group/other のビットだけが違うファイルを別ディレクトリに置く
            use std::os::unix::fs::PermissionsExt as _;
            let temp_dir = tempfile::tempdir().unwrap();
            let first_base = temp_dir.path().join("first");
            let second_base = temp_dir.path().join("second");
            std::fs::create_dir_all(&first_base).unwrap();
            std::fs::create_dir_all(&second_base).unwrap();
            std::fs::write(first_base.join("a.txt"), "content").unwrap();
            std::fs::write(second_base.join("a.txt"), "content").unwrap();
            std::fs::set_permissions(
                first_base.join("a.txt"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            std::fs::set_permissions(
                second_base.join("a.txt"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();

            // Act
            let first = build_from(&first_base, &["a.txt"]);
            let second = build_from(&second_base, &["a.txt"]);

            // Assert: 丸めが効いていれば同じハッシュになる。効いていないと、
            // umask の違うランナー間でキャッシュを共有するたびに再アップロードが走る
            assert_eq!(first.content_sha256, second.content_sha256);
        }

        #[test]
        fn test_paths_use_slash_separator() {
            // Arrange: 2 段ネストしたディレクトリ
            let temp_dir = tempfile::tempdir().unwrap();
            let base_path = temp_dir.path();
            std::fs::create_dir_all(base_path.join("a/b")).unwrap();
            std::fs::write(base_path.join("a/b/c.txt"), "content").unwrap();
            let archive = build_from(base_path, &["a"]);

            // Act
            let headers = read_headers(&archive);
            let paths: std::vec::Vec<String> = headers
                .iter()
                .map(|h| h.path().unwrap().to_string_lossy().into_owned())
                .collect();

            // Assert: ヘッダに書かれた名前が `/` 区切りの相対パスで、浅い順に並ぶこと。
            // Windows で `a\b\c.txt` と書かれると同じ内容でもバイト列が割れるうえ、
            // Unix 側で展開したときに 1 つの妙な名前のファイルになる
            assert_eq!(paths, vec!["a", "a/b", "a/b/c.txt"]);
        }
    }

    mod roundtrip_tests {
        use super::*;

        #[test]
        fn test_roundtrip_restores_file_contents() {
            // Arrange: 生成元と展開先を別ディレクトリにして、
            // 「たまたま元の木が残っていた」ことで通ってしまう事故を避ける
            let source_dir = tempfile::tempdir().unwrap();
            let source_path = source_dir.path();
            std::fs::create_dir_all(source_path.join("nested/deep")).unwrap();
            std::fs::write(source_path.join("top.txt"), "top-content").unwrap();
            std::fs::write(source_path.join("nested/deep/inner.txt"), "inner-content").unwrap();
            let archive = build_from(source_path, &["top.txt", "nested"]);
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            let content_sha256 =
                extract_archive(archive.temp_file.path(), dest_dir.path()).unwrap();

            // Assert: 展開側が返すハッシュが生成側と一致すること。この 2 つは別経路
            // （書き込み時 / 読み出し時）で計算しているので、片方だけ tar ストリームの
            // 範囲を取り違えていると一致しない。restore の内容検証はこの一致に依存している
            assert_eq!(content_sha256, archive.content_sha256);
            assert_eq!(
                std::fs::read_to_string(dest_dir.path().join("top.txt")).unwrap(),
                "top-content"
            );
            assert_eq!(
                std::fs::read_to_string(dest_dir.path().join("nested/deep/inner.txt")).unwrap(),
                "inner-content"
            );
        }

        #[test]
        fn test_roundtrip_restores_empty_directory() {
            // Arrange: 中身の無いディレクトリだけを含むアーカイブ
            let source_dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(source_dir.path().join("empty")).unwrap();
            let archive = build_from(source_dir.path(), &["empty"]);
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            extract_archive(archive.temp_file.path(), dest_dir.path()).unwrap();

            // Assert: ファイルの親としてしかディレクトリを作らない実装だと、
            // 中身の無いディレクトリは復元されず消えてしまう
            assert!(dest_dir.path().join("empty").is_dir());
        }

        #[cfg(unix)]
        #[test]
        fn test_roundtrip_restores_executable_bit() {
            // Arrange
            use std::os::unix::fs::PermissionsExt as _;
            let source_dir = tempfile::tempdir().unwrap();
            let source_path = source_dir.path();
            std::fs::write(source_path.join("exec.sh"), "#!/bin/sh\n").unwrap();
            std::fs::write(source_path.join("plain.txt"), "content").unwrap();
            std::fs::set_permissions(
                source_path.join("exec.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            let archive = build_from(source_path, &["exec.sh", "plain.txt"]);
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            extract_archive(archive.temp_file.path(), dest_dir.path()).unwrap();

            // Assert: ヘッダの mode を展開時に適用できているか。ここが漏れると
            // 復元したバイナリが実行できず、キャッシュがヒットしてもビルドが失敗する
            let exec_mode = std::fs::metadata(dest_dir.path().join("exec.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let plain_mode = std::fs::metadata(dest_dir.path().join("plain.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(exec_mode, MODE_EXECUTABLE);
            assert_eq!(plain_mode, MODE_REGULAR);
        }

        #[cfg(unix)]
        #[test]
        fn test_roundtrip_restores_symlink_as_link() {
            // Arrange: ファイルへの相対リンク
            let source_dir = tempfile::tempdir().unwrap();
            let source_path = source_dir.path();
            std::fs::write(source_path.join("real.txt"), "content").unwrap();
            std::os::unix::fs::symlink("real.txt", source_path.join("link.txt")).unwrap();
            let archive = build_from(source_path, &["real.txt", "link.txt"]);
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            extract_archive(archive.temp_file.path(), dest_dir.path()).unwrap();

            // Assert: 実体をコピーした通常ファイルではなくリンクとして復元されること。
            // リンク先の文字列も元のまま（絶対パスへ書き換わっていない）であること
            let restored_link = dest_dir.path().join("link.txt");
            assert!(std::fs::symlink_metadata(&restored_link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                std::fs::read_link(&restored_link).unwrap(),
                std::path::Path::new("real.txt")
            );
        }

        #[cfg(unix)]
        #[test]
        fn test_roundtrip_restores_symlink_to_directory() {
            // Arrange: リンク名がリンク先より前にソートされる配置にする
            // （tar のエントリ順ではリンク先ディレクトリがまだ存在しないタイミングで
            // リンクが現れる。Windows は作成時にファイル/ディレクトリを指定する必要があり、
            // 展開後にまとめて作る遅延パスが無いと種別を誤る）
            let source_dir = tempfile::tempdir().unwrap();
            let source_path = source_dir.path();
            std::fs::create_dir_all(source_path.join("zdir")).unwrap();
            std::fs::write(source_path.join("zdir/inner.txt"), "content").unwrap();
            std::os::unix::fs::symlink("zdir", source_path.join("a-link")).unwrap();
            let archive = build_from(source_path, &["a-link", "zdir"]);
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            extract_archive(archive.temp_file.path(), dest_dir.path()).unwrap();

            // Assert
            let restored_link = dest_dir.path().join("a-link");
            assert!(std::fs::symlink_metadata(&restored_link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                std::fs::read_link(&restored_link).unwrap(),
                std::path::Path::new("zdir")
            );
            // リンク経由でリンク先の中身に到達できる
            assert_eq!(
                std::fs::read_to_string(restored_link.join("inner.txt")).unwrap(),
                "content"
            );
        }

        #[cfg(unix)]
        #[test]
        fn test_extraction_overwrites_existing_symlink() {
            // Arrange: 既存のリンクがあると symlink(2) は EEXIST になるため、
            // 先に消してから作り直す必要がある
            let source_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("real.txt"), "content").unwrap();
            std::os::unix::fs::symlink("real.txt", source_dir.path().join("link.txt")).unwrap();
            let archive = build_from(source_dir.path(), &["real.txt", "link.txt"]);
            let dest_dir = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink("stale-target", dest_dir.path().join("link.txt")).unwrap();

            // Act
            extract_archive(archive.temp_file.path(), dest_dir.path()).unwrap();

            // Assert
            assert_eq!(
                std::fs::read_link(dest_dir.path().join("link.txt")).unwrap(),
                std::path::Path::new("real.txt")
            );
        }

        #[cfg(unix)]
        #[test]
        fn test_extraction_overwrites_existing_symlink_to_directory() {
            // Arrange: 既存がディレクトリへのリンクの場合。Windows では DeleteFileW で消せず
            // RemoveDirectoryW が必要になるため、種別ごとに削除方法を分けている
            let source_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("real.txt"), "content").unwrap();
            std::os::unix::fs::symlink("real.txt", source_dir.path().join("link")).unwrap();
            let archive = build_from(source_dir.path(), &["real.txt", "link"]);

            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dest_dir.path().join("stale_dir")).unwrap();
            std::os::unix::fs::symlink("stale_dir", dest_dir.path().join("link")).unwrap();

            // Act
            extract_archive(archive.temp_file.path(), dest_dir.path()).unwrap();

            // Assert: リンクは張り替わり、リンク先だったディレクトリは消されていない
            assert_eq!(
                std::fs::read_link(dest_dir.path().join("link")).unwrap(),
                std::path::Path::new("real.txt")
            );
            assert!(dest_dir.path().join("stale_dir").is_dir());
        }

        #[cfg(unix)]
        #[test]
        fn test_extraction_fails_when_symlink_destination_is_real_directory() {
            // Arrange: リンクを作る位置に実ディレクトリがある
            let source_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("real.txt"), "content").unwrap();
            std::os::unix::fs::symlink("real.txt", source_dir.path().join("link")).unwrap();
            let archive = build_from(source_dir.path(), &["real.txt", "link"]);

            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dest_dir.path().join("link")).unwrap();
            std::fs::write(dest_dir.path().join("link/precious.txt"), "do not delete").unwrap();

            // Act
            let result = extract_archive(archive.temp_file.path(), dest_dir.path());

            // Assert: 黙って再帰削除せずエラーにし、中身を残す
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("実ディレクトリがあります"));
            assert_eq!(
                std::fs::read_to_string(dest_dir.path().join("link/precious.txt")).unwrap(),
                "do not delete"
            );
        }

        #[test]
        fn test_extraction_overwrites_existing_file() {
            // Arrange: 既存ファイルをアーカイブ内より**長く**しておく。
            // 追記や部分上書きになっていると古い末尾が残り、それを検出できる
            let source_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("a.txt"), "new-content").unwrap();
            let archive = build_from(source_dir.path(), &["a.txt"]);
            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dest_dir.path().join("a.txt"),
                "stale-content-that-is-longer",
            )
            .unwrap();

            // Act
            extract_archive(archive.temp_file.path(), dest_dir.path()).unwrap();

            // Assert: 既存を残すのではなく truncate して上書きする。キャッシュ復元では
            // 古いものが中途半端に残る方が事故になりやすいという判断（設計doc 6.4）
            assert_eq!(
                std::fs::read_to_string(dest_dir.path().join("a.txt")).unwrap(),
                "new-content"
            );
        }

        #[test]
        fn test_extraction_sets_mtime_to_now() {
            // Arrange: アーカイブ内の mtime は epoch 固定。そのまま復元すると成果物が
            // ソースより古く見え、cargo や make が全再ビルドしてキャッシュの意味が消える。
            // そのため展開時刻を入れる（設計doc 6.5）
            let source_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("a.txt"), "content").unwrap();
            let archive = build_from(source_dir.path(), &["a.txt"]);
            let dest_dir = tempfile::tempdir().unwrap();
            let before_extract = std::time::SystemTime::now();

            // Act
            extract_archive(archive.temp_file.path(), dest_dir.path()).unwrap();

            // Assert: epoch のままなら 1970 年になるので大きく下回る。
            // 5 秒の許容はファイルシステムの時刻解像度とテスト実行時間のぶれを吸収するためで、
            // 「epoch ではない」ことを見るには十分な粗さ
            let mtime = std::fs::metadata(dest_dir.path().join("a.txt"))
                .unwrap()
                .modified()
                .unwrap();
            assert!(
                mtime >= before_extract - std::time::Duration::from_secs(5),
                "展開後の mtime が epoch のままになっている: {mtime:?}"
            );
        }
    }

    mod extraction_safety_tests {
        use super::*;

        /// 細工済み tar（zstd 圧縮済み）を組み立てる
        ///
        /// tar の中身は S3 から降ってくる外部入力であり、cafce 自身が書いたとは限らない。
        /// `create_archive` は正常なアーカイブしか作れないので、攻撃者が置いた
        /// オブジェクトを模すには tar を手で組む必要がある。
        fn build_malicious_archive<F>(configure: F) -> tempfile::NamedTempFile
        where
            F: FnOnce(&mut tar::Builder<zstd::Encoder<'static, std::fs::File>>),
        {
            let temp_file = tempfile::NamedTempFile::new().unwrap();
            let encoder = zstd::Encoder::new(temp_file.reopen().unwrap(), ZSTD_LEVEL).unwrap();
            let mut builder = tar::Builder::new(encoder);
            configure(&mut builder);
            let encoder = builder.into_inner().unwrap();
            encoder.finish().unwrap();
            temp_file
        }

        fn append_raw_entry(
            builder: &mut tar::Builder<zstd::Encoder<'static, std::fs::File>>,
            path: &str,
            data: &[u8],
        ) {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(MODE_REGULAR);
            header.set_size(data.len() as u64);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            // `Header::set_path` は長い名前を GNU longname エントリへ回すなどの加工を行う。
            // ここでは「攻撃者が書いたそのままのバイト列」を再現したいので、
            // GNU ヘッダの name フィールドへ直接書き込む
            header
                .as_gnu_mut()
                .expect("new_gnu なので GNU ヘッダのはず")
                .name[..path.len()]
                .copy_from_slice(path.as_bytes());
            header.set_cksum();
            builder.append(&header, data).unwrap();
        }

        #[test]
        fn test_parent_dir_entry_is_rejected() {
            // Arrange: 展開先の 1 階層上へ書き込もうとするエントリ（いわゆる zip-slip）
            let archive = build_malicious_archive(|builder| {
                append_raw_entry(builder, "../evil.txt", b"pwned");
            });
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            let result = extract_archive(archive.path(), dest_dir.path());

            // Assert: スキップではなくエラーにする。CI では作業ディレクトリの親に
            // 他ジョブの成果物や設定が置かれていることがあり、黙って書かれると気づけない
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("展開先ディレクトリの外を指しています"));
        }

        #[test]
        fn test_nested_parent_dir_entry_is_rejected() {
            // Arrange: いったん潜ってから `..` を重ねて抜け出す形。単純に先頭が `..` か
            // どうかだけを見る実装だとこれを通してしまう
            let archive = build_malicious_archive(|builder| {
                append_raw_entry(builder, "a/b/../../../evil.txt", b"pwned");
            });
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            let result = extract_archive(archive.path(), dest_dir.path());

            // Assert: 畳んだ結果で判定しているので検出できる
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("展開先ディレクトリの外を指しています"));
        }

        #[test]
        fn test_absolute_path_entry_is_rejected() {
            // Arrange: 絶対パスのエントリ。`base.join("/etc/evil.txt")` は Rust では
            // `/etc/evil.txt` になり、展開先を完全に無視して書き込まれてしまう
            let archive = build_malicious_archive(|builder| {
                append_raw_entry(builder, "/etc/evil.txt", b"pwned");
            });
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            let result = extract_archive(archive.path(), dest_dir.path());

            // Assert: join する前に絶対パスを弾く
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("エントリが絶対パスです"));
        }

        #[test]
        fn test_symlink_escaping_base_is_rejected() {
            // Arrange: エントリ名自体は展開先の中（`escape.txt`）だが、リンク先が外を指す。
            // 作成そのものは害が無く見えるが、後続の書き込みがリンク経由で
            // 展開先の外へ届く経路になる
            let archive = build_malicious_archive(|builder| {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_size(0);
                header.set_mtime(0);
                header.set_uid(0);
                header.set_gid(0);
                builder
                    .append_link(&mut header, "escape.txt", "../../../../etc/passwd")
                    .unwrap();
            });
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            let result = extract_archive(archive.path(), dest_dir.path());

            // Assert: エントリ名だけでなくリンク先も検証している
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("リンク先が展開先ディレクトリの外を指しています"));
        }

        #[test]
        fn test_absolute_symlink_target_is_rejected() {
            // Arrange: リンク先が絶対パスの場合。相対リンクと違い `..` を数えても
            // 検出できないので、判定を分けている
            let archive = build_malicious_archive(|builder| {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_size(0);
                header.set_mtime(0);
                header.set_uid(0);
                header.set_gid(0);
                builder
                    .append_link(&mut header, "escape.txt", "/etc/passwd")
                    .unwrap();
            });
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            let result = extract_archive(archive.path(), dest_dir.path());

            // Assert
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("リンク先が展開先ディレクトリの外を指しています"));
        }

        #[test]
        fn test_relative_symlink_inside_base_is_allowed() {
            // Arrange: `sub/link.txt -> ../real.txt` は `..` を含むが、リンクが置かれる
            // `sub/` を基準に解決すると展開先直下に収まる。正当なアーカイブにも普通に現れる形で、
            // 「リンク先に `..` があれば拒否」という雑な実装だと誤検知する
            let archive = build_malicious_archive(|builder| {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_size(0);
                header.set_mtime(0);
                header.set_uid(0);
                header.set_gid(0);
                builder
                    .append_link(&mut header, "sub/link.txt", "../real.txt")
                    .unwrap();
            });
            let dest_dir = tempfile::tempdir().unwrap();

            // Act
            let result = extract_archive(archive.path(), dest_dir.path());

            // Assert: 拒否されない（リンク先が存在しない dangling リンクでも作成は成功する）
            assert!(result.is_ok());
        }
    }

    mod resolve_entry_destination_tests {
        use super::*;

        #[test]
        fn test_normal_relative_path() {
            // Arrange: 何の細工もない普通のエントリ名
            let base_path = std::path::Path::new("/base");
            let entry_path = std::path::Path::new("dir/file.txt");

            // Act
            let result = resolve_entry_destination(entry_path, base_path).unwrap();

            // Assert: 展開先の絶対パスへ素直に解決される（正常系を潰していないことの確認）
            assert_eq!(result, Some(std::path::PathBuf::from("/base/dir/file.txt")));
        }

        #[test]
        fn test_cur_dir_entry_maps_to_base_itself() {
            // Arrange: `tar cf` で作ったアーカイブには先頭に `./` エントリが入ることがある
            let base_path = std::path::Path::new("/base");
            let entry_path = std::path::Path::new("./");

            // Act
            let result = resolve_entry_destination(entry_path, base_path).unwrap();

            // Assert: 展開先そのものを指すので作成対象は無い。エラーにすると
            // 他のツールが作った正当なアーカイブを展開できなくなるため `None` で読み飛ばす
            assert_eq!(result, None);
        }

        #[test]
        fn test_absolute_entry_is_error() {
            // Arrange
            let base_path = std::path::Path::new("/base");
            let entry_path = std::path::Path::new("/etc/passwd");

            // Act
            let result = resolve_entry_destination(entry_path, base_path);

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::ArchiveError::AbsoluteEntryPath { .. })
            ));
        }

        #[test]
        fn test_escaping_entry_is_error() {
            // Arrange
            let base_path = std::path::Path::new("/base");
            let entry_path = std::path::Path::new("../etc/passwd");

            // Act
            let result = resolve_entry_destination(entry_path, base_path);

            // Assert
            assert!(matches!(
                result,
                Err(crate::error::ArchiveError::EntryEscapesBaseDirectory { .. })
            ));
        }

        #[test]
        fn test_parent_dir_returning_inside_is_allowed() {
            // Arrange: 途中で `..` を通るが最終的に展開先へ戻るエントリ
            let base_path = std::path::Path::new("/base");
            let entry_path = std::path::Path::new("dir/../file.txt");

            // Act
            let result = resolve_entry_destination(entry_path, base_path).unwrap();

            // Assert: 展開側は `..` の有無ではなく畳んだ結果で判定する。
            // `paths`（store 側）が `..` を含むパターンを一律拒否するのとは方針が違う。
            // あちらは利用者が書き直せる設定値、こちらは他ツールが作ったアーカイブも
            // 受け入れる必要がある外部入力、という差である
            assert_eq!(result, Some(std::path::PathBuf::from("/base/file.txt")));
        }
    }
}
