pub struct CacheKeyGenerator {
    max_files: usize,
    base_path: std::path::PathBuf,
}

impl CacheKeyGenerator {
    pub fn new(max_files: usize, base_path: std::path::PathBuf) -> Self {
        Self {
            max_files,
            base_path,
        }
    }

    pub fn generate_key(&self, key_config: &crate::setting::Key) -> anyhow::Result<String> {
        // FileMatcherを使ってパターンからファイルを解決
        let file_matcher = crate::file_matcher::FileMatcher::with_max_files(self.max_files);
        let matched_files = file_matcher.resolve_patterns(&key_config.files, &self.base_path)?;
        
        // HashCalculatorを使ってファイルのハッシュを計算
        let files_hash = crate::hash_calculator::HashCalculator::calculate_files_hash(&matched_files)?;
        
        // プレフィックスがある場合は結合
        let final_key = match &key_config.prefix {
            Some(prefix) => format!("{}-{}", prefix, files_hash),
            None => files_hash,
        };
        
        Ok(final_key)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_new() {
        let base_path = std::path::PathBuf::from("/tmp");
        let generator = super::CacheKeyGenerator::new(50, base_path.clone());
        assert_eq!(generator.max_files, 50);
        assert_eq!(generator.base_path, base_path);
    }

    #[test]
    fn test_generate_key_with_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().to_path_buf();
        
        // テスト用ファイルを作成
        std::fs::write(temp_dir.path().join("test.txt"), "test content").unwrap();
        
        let generator = super::CacheKeyGenerator::new(50, base_path);
        let key_config = crate::setting::Key {
            files: vec!["test.txt".to_string()],
            prefix: None,
        };

        let result = generator.generate_key(&key_config);
        assert!(result.is_ok());
        let key = result.unwrap();
        assert_eq!(key.len(), 64); // SHA-256は64文字の16進数文字列
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_key_with_prefix() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().to_path_buf();
        
        // テスト用ファイルを作成
        std::fs::write(temp_dir.path().join("test.txt"), "test content").unwrap();
        
        let generator = super::CacheKeyGenerator::new(50, base_path);
        let key_config = crate::setting::Key {
            files: vec!["test.txt".to_string()],
            prefix: Some("my-prefix".to_string()),
        };

        let result = generator.generate_key(&key_config);
        assert!(result.is_ok());
        let key = result.unwrap();
        assert!(key.starts_with("my-prefix-"));
        assert_eq!(key.len(), "my-prefix-".len() + 64); // プレフィックス + ハイフン + SHA-256
    }

    #[test]
    fn test_generate_key_with_wildcard() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().to_path_buf();
        
        // テスト用ファイルを作成
        std::fs::write(temp_dir.path().join("test1.txt"), "content1").unwrap();
        std::fs::write(temp_dir.path().join("test2.txt"), "content2").unwrap();
        
        let generator = super::CacheKeyGenerator::new(50, base_path);
        let key_config = crate::setting::Key {
            files: vec!["*.txt".to_string()],
            prefix: None,
        };

        let result = generator.generate_key(&key_config);
        assert!(result.is_ok());
        let key = result.unwrap();
        assert_eq!(key.len(), 64); // SHA-256は64文字の16進数文字列
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_key_with_multiple_patterns() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().to_path_buf();
        
        // テスト用ファイルを作成
        std::fs::write(temp_dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(temp_dir.path().join("yarn.lock"), "lock content").unwrap();
        
        let generator = super::CacheKeyGenerator::new(50, base_path);
        let key_config = crate::setting::Key {
            files: vec!["package.json".to_string(), "*.lock".to_string()],
            prefix: None,
        };

        let result = generator.generate_key(&key_config);
        assert!(result.is_ok());
        let key = result.unwrap();
        assert_eq!(key.len(), 64); // SHA-256は64文字の16進数文字列
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_key_no_matching_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().to_path_buf();
        
        let generator = super::CacheKeyGenerator::new(50, base_path);
        let key_config = crate::setting::Key {
            files: vec!["nonexistent.txt".to_string()],
            prefix: None,
        };

        let result = generator.generate_key(&key_config);
        assert!(result.is_ok());
        let key = result.unwrap();
        assert_eq!(key.len(), 64); // 空のファイルリストでもハッシュは生成される
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_key_same_files_same_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().to_path_buf();
        
        // テスト用ファイルを作成
        std::fs::write(temp_dir.path().join("test.txt"), "test content").unwrap();
        
        let generator = super::CacheKeyGenerator::new(50, base_path);
        let key_config = crate::setting::Key {
            files: vec!["test.txt".to_string()],
            prefix: None,
        };

        let result1 = generator.generate_key(&key_config);
        let result2 = generator.generate_key(&key_config);
        
        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert_eq!(result1.unwrap(), result2.unwrap());
    }

    #[test]
    fn test_generate_key_different_content_different_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().to_path_buf();
        
        // 最初のファイル内容
        std::fs::write(temp_dir.path().join("test.txt"), "content1").unwrap();
        
        let generator = super::CacheKeyGenerator::new(50, base_path);
        let key_config = crate::setting::Key {
            files: vec!["test.txt".to_string()],
            prefix: None,
        };

        let result1 = generator.generate_key(&key_config);
        
        // ファイル内容を変更
        std::fs::write(temp_dir.path().join("test.txt"), "content2").unwrap();
        
        let result2 = generator.generate_key(&key_config);
        
        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert_ne!(result1.unwrap(), result2.unwrap());
    }
}
