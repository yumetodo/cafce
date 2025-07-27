#[derive(Debug, thiserror::Error)]
pub enum CacheKeyError {
    #[error("ファイル数が制限を超えています: {count} > {limit}")]
    TooManyFiles { count: usize, limit: usize },
    
    #[error("ファイルの読み込みに失敗しました: {path}")]
    FileReadError { path: std::path::PathBuf },
    
    #[error("パターンマッチングに失敗しました: {pattern}")]
    PatternMatchError { pattern: String },
    
    #[error("ハッシュ計算に失敗しました")]
    HashCalculationError,
}
