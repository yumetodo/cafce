pub struct HashCalculator;

impl HashCalculator {
    // 呼び出し元(FileMatcher)がソート済みのリストを渡す場合でも、
    // このメソッド単体で呼ばれても結果が入力順序に依存しないよう、
    // 常に自前でソートし直す契約とする
    pub fn calculate_files_hash(files: &[std::path::PathBuf]) -> anyhow::Result<String> {
        use sha2::Digest;

        if files.is_empty() {
            // 空のファイルリストの場合は空文字列のハッシュを返す
            let mut hasher = sha2::Sha256::new();
            hasher.update(b"");
            let result = hasher.finalize();
            return Ok(format!("{result:x}"));
        }

        // ファイルパスでソートして一貫性を保つ
        let mut sorted_files = files.to_vec();
        sorted_files.sort();
        
        // 各ファイルのハッシュを計算
        let mut file_hashes = std::vec::Vec::new();
        for file in &sorted_files {
            let file_hash = Self::calculate_single_file_hash(file)?;
            file_hashes.push(file_hash);
        }
        
        // すべてのファイルハッシュを結合して最終ハッシュを計算
        let combined = file_hashes.join("\n");
        let mut hasher = sha2::Sha256::new();
        hasher.update(combined.as_bytes());
        let result = hasher.finalize();

        Ok(format!("{result:x}"))
    }

    pub fn calculate_single_file_hash(file: &std::path::Path) -> anyhow::Result<String> {
        use anyhow::Context;
        use sha2::Digest;
        
        let content = std::fs::read(file)
            .with_context(|| format!("ファイルの読み込みに失敗しました: {}", file.display()))?;
        
        let mut hasher = sha2::Sha256::new();
        hasher.update(&content);
        let result = hasher.finalize();

        Ok(format!("{result:x}"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_calculate_single_file_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file = temp_dir.path().join("test.txt");
        std::fs::write(&temp_file, "test content").unwrap();

        let result = super::HashCalculator::calculate_single_file_hash(&temp_file);
        assert!(result.is_ok());
        let hash = result.unwrap();
        assert_eq!(hash.len(), 64); // SHA-256は64文字の16進数文字列
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_calculate_single_file_hash_known_value() {
        // アルゴリズムが誤ってSHA-1やMD5等に差し替わった際に検出するための
        // 既知値による回帰テスト（`printf 'test content' | sha256sum`で計算）
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file = temp_dir.path().join("test.txt");
        std::fs::write(&temp_file, "test content").unwrap();

        let result = super::HashCalculator::calculate_single_file_hash(&temp_file);
        assert_eq!(
            result.unwrap(),
            "6ae8a75555209fd6c44157c0aed8016e763ff435a19cf186f76863140143ff72"
        );
    }

    #[test]
    fn test_calculate_single_file_hash_same_content() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file1 = temp_dir.path().join("test1.txt");
        let temp_file2 = temp_dir.path().join("test2.txt");
        
        let content = "same content";
        std::fs::write(&temp_file1, content).unwrap();
        std::fs::write(&temp_file2, content).unwrap();

        let result1 = super::HashCalculator::calculate_single_file_hash(&temp_file1);
        let result2 = super::HashCalculator::calculate_single_file_hash(&temp_file2);
        
        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert_eq!(result1.unwrap(), result2.unwrap());
    }

    #[test]
    fn test_calculate_single_file_hash_different_content() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file1 = temp_dir.path().join("test1.txt");
        let temp_file2 = temp_dir.path().join("test2.txt");
        
        std::fs::write(&temp_file1, "content1").unwrap();
        std::fs::write(&temp_file2, "content2").unwrap();

        let result1 = super::HashCalculator::calculate_single_file_hash(&temp_file1);
        let result2 = super::HashCalculator::calculate_single_file_hash(&temp_file2);
        
        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert_ne!(result1.unwrap(), result2.unwrap());
    }

    #[test]
    fn test_calculate_single_file_hash_nonexistent_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let nonexistent_file = temp_dir.path().join("nonexistent.txt");

        let result = super::HashCalculator::calculate_single_file_hash(&nonexistent_file);
        assert!(result.is_err());
    }

    #[test]
    fn test_calculate_files_hash_empty() {
        let files = vec![];

        let result = super::HashCalculator::calculate_files_hash(&files);
        assert!(result.is_ok());
        let hash = result.unwrap();
        assert_eq!(hash.len(), 64); // SHA-256は64文字の16進数文字列
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_calculate_files_hash_single_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file = temp_dir.path().join("test.txt");
        std::fs::write(&temp_file, "test content").unwrap();
        
        let files = vec![temp_file];

        let result = super::HashCalculator::calculate_files_hash(&files);
        assert!(result.is_ok());
        let hash = result.unwrap();
        assert_eq!(hash.len(), 64); // SHA-256は64文字の16進数文字列
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_calculate_files_hash_multiple_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file1 = temp_dir.path().join("test1.txt");
        let temp_file2 = temp_dir.path().join("test2.txt");
        
        std::fs::write(&temp_file1, "content1").unwrap();
        std::fs::write(&temp_file2, "content2").unwrap();
        
        let files = vec![temp_file1, temp_file2];

        let result = super::HashCalculator::calculate_files_hash(&files);
        assert!(result.is_ok());
        let hash = result.unwrap();
        assert_eq!(hash.len(), 64); // SHA-256は64文字の16進数文字列
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_calculate_files_hash_known_value() {
        // 単一ファイルの内容ハッシュを16進文字列として結合し、
        // さらにSHA-256をとった値との一致を確認する回帰テスト
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file = temp_dir.path().join("test.txt");
        std::fs::write(&temp_file, "test content").unwrap();

        let result = super::HashCalculator::calculate_files_hash(&[temp_file]);
        assert_eq!(
            result.unwrap(),
            "4b9054a7a40e53c2e310fcd6f696c46c6a40dcdfa5b849785a456756ec512660"
        );
    }

    #[test]
    fn test_calculate_files_hash_sorted_order() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file1 = temp_dir.path().join("a.txt");
        let temp_file2 = temp_dir.path().join("b.txt");
        
        std::fs::write(&temp_file1, "content1").unwrap();
        std::fs::write(&temp_file2, "content2").unwrap();
        
        // 順序を変えて同じハッシュが生成されることを確認
        let files1 = vec![temp_file1.clone(), temp_file2.clone()];
        let files2 = vec![temp_file2, temp_file1];

        let result1 = super::HashCalculator::calculate_files_hash(&files1);
        let result2 = super::HashCalculator::calculate_files_hash(&files2);
        
        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert_eq!(result1.unwrap(), result2.unwrap());
    }
}
