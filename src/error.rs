#[derive(Debug, thiserror::Error)]
pub enum CacheKeyError {
    #[error("ファイル数が制限を超えています: {count} > {limit}")]
    TooManyFiles { count: usize, limit: usize },

    #[error("絶対パスのパターンは指定できません: {pattern}")]
    AbsolutePathNotAllowed { pattern: String },

    #[error("指定されたパターンにマッチするファイルがありません")]
    NoFilesMatched,
}
