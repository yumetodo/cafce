const MAX_FILES: usize = 50;

pub struct FileMatcher {
    max_files: usize,
}

impl FileMatcher {
    pub fn new() -> Self {
        Self {
            max_files: MAX_FILES,
        }
    }

    pub fn with_max_files(max_files: usize) -> Self {
        Self { max_files }
    }

    pub fn resolve_patterns(
        &self,
        patterns: &[String],
        base_path: &std::path::Path,
    ) -> anyhow::Result<std::vec::Vec<std::path::PathBuf>> {
        use anyhow::Context;
        
        let mut all_files = std::collections::HashSet::new();
        
        for pattern in patterns {
            // 相対パターンの場合はbase_pathと結合
            let full_pattern = if std::path::Path::new(pattern).is_absolute() {
                pattern.clone()
            } else {
                base_path.join(pattern).to_string_lossy().to_string()
            };
            
            // globパターンでファイルを検索
            let glob_result = glob::glob(&full_pattern)
                .with_context(|| format!("パターンマッチングに失敗しました: {}", pattern))?;
            
            // OKの結果のみを取得し、ファイルのみをフィルタリング
            for path in glob_result.filter_map(Result::ok) {
                // ファイルのみを対象とし、ディレクトリは除外
                if path.is_file() {
                    // base_pathより外側のファイルは除外（セキュリティ対策）
                    if path.starts_with(base_path) {
                        all_files.insert(path);
                    }
                    // base_pathより外側のファイルは無視
                }
            }
        }
        
        // ファイル数制限チェック
        if all_files.len() > self.max_files {
            return Err(crate::error::CacheKeyError::TooManyFiles {
                count: all_files.len(),
                limit: self.max_files,
            }.into());
        }
        
        // ソートして一貫性を保つ
        let mut result: std::vec::Vec<std::path::PathBuf> = all_files.into_iter().collect();
        result.sort();
        
        Ok(result)
    }
}

impl Default for FileMatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_new() {
        let matcher = super::FileMatcher::new();
        assert_eq!(matcher.max_files, super::MAX_FILES);
    }

    #[test]
    fn test_with_max_files() {
        let matcher = super::FileMatcher::with_max_files(10);
        assert_eq!(matcher.max_files, 10);
    }

    #[test]
    fn test_default() {
        let matcher = super::FileMatcher::default();
        assert_eq!(matcher.max_files, super::MAX_FILES);
    }

    #[test]
    fn test_resolve_patterns_single_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();
        
        // テスト用ファイルを作成
        let test_file = temp_path.join("test.txt");
        std::fs::write(&test_file, "test content").unwrap();
        
        let matcher = super::FileMatcher::new();
        let patterns = vec!["test.txt".to_string()];
        
        let result = matcher.resolve_patterns(&patterns, temp_path);
        assert!(result.is_ok());
        let files = result.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], test_file);
    }

    #[test]
    fn test_resolve_patterns_wildcard() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();
        
        // テスト用ファイルを作成
        std::fs::write(temp_path.join("test1.txt"), "content1").unwrap();
        std::fs::write(temp_path.join("test2.txt"), "content2").unwrap();
        
        let matcher = super::FileMatcher::new();
        let patterns = vec!["*.txt".to_string()];
        
        let result = matcher.resolve_patterns(&patterns, temp_path);
        assert!(result.is_ok());
        let files = result.unwrap();
        assert_eq!(files.len(), 2);
        // ソートされているはず
        assert!(files[0].file_name().unwrap().to_str().unwrap() < files[1].file_name().unwrap().to_str().unwrap());
    }

    #[test]
    fn test_resolve_patterns_max_files_limit() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();
        
        // 制限を超える数のファイルを作成
        for i in 0..60 {
            std::fs::write(temp_path.join(format!("test{}.txt", i)), "content").unwrap();
        }
        
        let matcher = super::FileMatcher::with_max_files(50);
        let patterns = vec!["*.txt".to_string()];
        
        let result = matcher.resolve_patterns(&patterns, temp_path);
        assert!(result.is_err());
        // TooManyFilesエラーかどうか確認
        let error = result.unwrap_err();
        assert!(error.to_string().contains("ファイル数が制限を超えています"));
    }

    #[test]
    fn test_resolve_patterns_nonexistent_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();
        
        let matcher = super::FileMatcher::new();
        let patterns = vec!["nonexistent.txt".to_string()];
        
        let result = matcher.resolve_patterns(&patterns, temp_path);
        assert!(result.is_ok());
        let files = result.unwrap();
        assert_eq!(files.len(), 0); // 存在しないファイルは無視される（GitLab CI互換）
    }

    #[test]
    fn test_resolve_patterns_nested_wildcard() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();
        
        // ネストしたディレクトリ構造を作成
        let nested_dir = temp_path.join("nested");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(nested_dir.join("package.json"), "{}").unwrap();
        
        let matcher = super::FileMatcher::new();
        let patterns = vec!["**/package.json".to_string()];
        
        let result = matcher.resolve_patterns(&patterns, temp_path);
        assert!(result.is_ok());
        let files = result.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], nested_dir.join("package.json"));
    }
}
