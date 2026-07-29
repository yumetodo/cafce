#[derive(Debug, thiserror::Error)]
pub enum CacheKeyError {
    #[error("ファイル数が制限を超えています: {count} > {limit}")]
    TooManyFiles { count: usize, limit: usize },

    #[error("絶対パスのパターンは指定できません: {pattern}")]
    AbsolutePathNotAllowed { pattern: String },
}

/// `Setting.paths` からキャッシュ対象を解決する際のエラー
#[derive(Debug, thiserror::Error)]
pub enum PathResolveError {
    #[error("paths が空です。store するキャッシュ対象を 1 つ以上指定してください")]
    EmptyPaths,

    #[error(
        "paths のどのパターンにも 1 件もマッチしませんでした: {patterns:?}\n\
         設定ミス、あるいは前段のビルドが成果物を生成していない可能性があります"
    )]
    NoMatch { patterns: Vec<String> },

    #[error("絶対パスのパターンは指定できません: {pattern}")]
    AbsolutePathNotAllowed { pattern: String },

    #[error("基準ディレクトリの外を指すパスは指定できません: {path}")]
    EscapesBaseDirectory { path: String },
}

/// アーカイブの生成・展開に関するエラー
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("アーカイブ内のエントリが絶対パスです: {path}")]
    AbsoluteEntryPath { path: String },

    #[error("アーカイブ内のエントリが展開先ディレクトリの外を指しています: {path}")]
    EntryEscapesBaseDirectory { path: String },

    #[error(
        "シンボリックリンクのリンク先が展開先ディレクトリの外を指しています: {path} -> {target}"
    )]
    SymlinkEscapesBaseDirectory { path: String, target: String },

    #[error(
        "アーカイブサイズが単一 PutObject の上限を超えています: {size} bytes > {limit} bytes\n\
         cafce はマルチパートアップロードに未対応です。paths を絞り込んでください"
    )]
    TooLargeForSinglePut { size: u64, limit: u64 },
}

/// S3 user metadata の生成・検証に関するエラー
#[derive(Debug, thiserror::Error)]
pub enum CacheMetadataError {
    #[error(
        "このキャッシュは未知のスキーマ版数 {found} で作られています（この cafce が解釈できるのは {supported} までです）\n\
         cafce を更新してください"
    )]
    UnknownSchemaVersion { found: u32, supported: u32 },

    #[error("スキーマ版数を 10 進整数として解釈できません: {value}")]
    MalformedSchemaVersion { value: String },

    #[error(
        "未知のアーカイブ形式です: {found}（この cafce が対応しているのは {supported} のみです）"
    )]
    UnknownArchiveFormat {
        found: String,
        supported: &'static str,
    },

    #[error("内容ハッシュの形式が不正です（小文字 16 進 64 文字である必要があります）: {value}")]
    MalformedContentHash { value: String },

    #[error(
        "キャッシュの内容ハッシュが一致しません (expected={expected}, actual={actual})\n\
         作業ディレクトリに中途半端に復元されたファイルが残っている可能性があります"
    )]
    ContentHashMismatch { expected: String, actual: String },

    #[error(
        "CAFCE_S3_CHECKSUM=required ですが、S3 がオブジェクトのチェックサムを返しませんでした\n\
         サーバがフレキシブルチェックサムに対応していない可能性があります"
    )]
    ChecksumMissingButRequired,
}
