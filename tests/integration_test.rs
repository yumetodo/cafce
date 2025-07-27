#[cfg(test)]
mod tests {
    #[test]
    fn test_cache_key_generation_integration() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().to_path_buf();
        
        // テスト用ファイルを作成
        std::fs::write(temp_dir.path().join("package.json"), r#"{"name": "test"}"#).unwrap();
        std::fs::write(temp_dir.path().join("yarn.lock"), "lock content").unwrap();
        
        let generator = cafce::cache_key::CacheKeyGenerator::new(50, base_path);
        let key_config = cafce::setting::Key {
            files: vec!["package.json".to_string(), "*.lock".to_string()],
            prefix: Some("cache-v1".to_string()),
        };

        let result = generator.generate_key(&key_config);
        assert!(result.is_ok());
        let key = result.unwrap();
        assert!(key.starts_with("cache-v1-"));
        assert_eq!(key.len(), "cache-v1-".len() + 64); // プレフィックス + ハイフン + SHA-256
    }

    #[test]
    fn test_file_matcher_integration() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();
        
        // ネストしたディレクトリ構造を作成
        let nested_dir = temp_path.join("src").join("components");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(nested_dir.join("package.json"), "{}").unwrap();
        std::fs::write(temp_path.join("package.json"), "{}").unwrap();
        
        let matcher = cafce::file_matcher::FileMatcher::new();
        let patterns = vec!["**/package.json".to_string()];
        
        let result = matcher.resolve_patterns(&patterns, temp_path);
        assert!(result.is_ok());
        let files = result.unwrap();
        assert_eq!(files.len(), 2); // ルートとネストしたディレクトリの両方
    }

    #[test]
    fn test_hash_calculator_integration() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file1 = temp_dir.path().join("file1.txt");
        let temp_file2 = temp_dir.path().join("file2.txt");
        
        std::fs::write(&temp_file1, "content1").unwrap();
        std::fs::write(&temp_file2, "content2").unwrap();
        
        let files = vec![temp_file1, temp_file2];
        
        let result = cafce::hash_calculator::HashCalculator::calculate_files_hash(&files);
        assert!(result.is_ok());
        let hash = result.unwrap();
        assert_eq!(hash.len(), 64); // SHA-256は64文字の16進数文字列
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_setting_key_structure() {
        let key = cafce::setting::Key {
            files: vec!["*.json".to_string(), "*.lock".to_string()],
            prefix: Some("v1".to_string()),
        };
        
        assert_eq!(key.files.len(), 2);
        assert_eq!(key.files[0], "*.json");
        assert_eq!(key.files[1], "*.lock");
        assert_eq!(key.prefix, Some("v1".to_string()));
    }

    #[test]
    fn test_error_types() {
        let error = cafce::error::CacheKeyError::TooManyFiles { count: 60, limit: 50 };
        let error_string = format!("{}", error);
        assert!(error_string.contains("ファイル数が制限を超えています"));
        assert!(error_string.contains("60"));
        assert!(error_string.contains("50"));
    }
}
