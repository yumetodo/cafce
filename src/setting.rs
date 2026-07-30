#[derive(Debug, thiserror::Error)]
pub enum SettingError {
    #[error("設定ファイルを開けませんでした: {0}")]
    Io(#[from] std::io::Error),
    #[error("設定ファイルのパースに失敗しました: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("project が空文字です（展開後も空文字の場合を含む）")]
    EmptyProject,
    #[error("変数 ${{{name}}} が定義されていません")]
    UndefinedVariable { name: String },
    #[error("変数参照 '${{' が閉じられていません（対応する '}}' がありません）")]
    UnterminatedVariableRef,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub files: Vec<String>,
    pub prefix: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    pub project: String,

    /// `store` がキャッシュするファイル/ディレクトリの glob パターン列
    ///
    /// - 基準ディレクトリはカレントディレクトリ（`key.files` と同じ基準）
    /// - ファイル・ディレクトリ・ワイルドカード（`*`, `**`, `?`, `[...]`）を指定できる
    /// - ディレクトリは再帰的に配下全体が対象になる
    /// - 絶対パス・基準ディレクトリ外への脱出はエラー
    /// - `${VAR}` 展開は行わない（ファイルシステムパスは展開対象外）
    /// - `store` では空配列・0 件マッチをエラーにする。`restore` は参照しない
    ///
    /// 実際の解決とバリデーションは `crate::path_matcher::resolve_paths` が担う。
    #[serde(default)]
    pub paths: Vec<String>,
    pub key: serde_either::StringOrStruct<Key>,
    #[serde(default)]
    pub fallback_keys: Vec<String>,
}

fn expand_env_vars(s: &str) -> Result<String, SettingError> {
    let mut result = String::with_capacity(s.len());
    let mut remaining = s;
    while let Some(dollar_pos) = remaining.find("${") {
        result.push_str(&remaining[..dollar_pos]);
        let after_brace = &remaining[dollar_pos + 2..];
        match after_brace.find('}') {
            Some(close_pos) => {
                let var_name = &after_brace[..close_pos];
                let value =
                    std::env::var(var_name).map_err(|_| SettingError::UndefinedVariable {
                        name: var_name.to_string(),
                    })?;
                result.push_str(&value);
                remaining = &after_brace[close_pos + 1..];
            }
            None => return Err(SettingError::UnterminatedVariableRef),
        }
    }
    result.push_str(remaining);
    Ok(result)
}

impl Setting {
    pub fn new_from_file(path: &std::path::Path) -> Result<Self, SettingError> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Self::new_from_str(&contents)
    }

    pub fn new_from_str(s: &str) -> Result<Self, SettingError> {
        let mut setting: Self = toml::from_str(s)?;

        setting.project = expand_env_vars(&setting.project)?;
        if setting.project.is_empty() {
            return Err(SettingError::EmptyProject);
        }

        setting.key = match setting.key {
            serde_either::StringOrStruct::String(literal) => {
                serde_either::StringOrStruct::String(expand_env_vars(&literal)?)
            }
            serde_either::StringOrStruct::Struct(k) => {
                let expanded_prefix = k.prefix.map(|p| expand_env_vars(&p)).transpose()?;
                serde_either::StringOrStruct::Struct(Key {
                    files: k.files,
                    prefix: expanded_prefix,
                })
            }
        };

        setting.fallback_keys = setting
            .fallback_keys
            .into_iter()
            .map(|k| expand_env_vars(&k))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(setting)
    }

    pub fn init_to_file(path: &std::path::Path) -> Result<(), SettingError> {
        use std::io::Write;
        let setting = Setting {
            project: "my-project".to_string(),
            paths: vec!["foo.txt".to_string()],
            key: serde_either::StringOrStruct::Struct(Key {
                files: vec!["bar.txt".to_string()],
                prefix: None,
            }),
            fallback_keys: Vec::default(),
        };
        let mut file = std::fs::File::create(path)?;
        let toml = toml::to_string(&setting).expect("init setting serialization should never fail");
        write!(file, "{toml}")?;
        file.flush()?;
        Ok(())
    }

    pub fn resolve_primary_key(&self, base_path: &std::path::Path) -> anyhow::Result<String> {
        match &self.key {
            serde_either::StringOrStruct::String(s) => Ok(s.clone()),
            serde_either::StringOrStruct::Struct(k) => {
                let generator =
                    crate::cache_key::CacheKeyGenerator::new(50, base_path.to_path_buf());
                generator.generate_key(k)
            }
        }
    }

    /// primary キーと `fallback_keys` を順に並べたキー候補列を返す
    ///
    /// `probe` と `restore` が同じ順序・同じ解決経路を通ることで、
    /// 「`probe` が `true` を返す状況では `restore` も必ずヒットする」という
    /// 不変条件を保つ。`store` は書き込み先が常に primary キーなのでこれを使わない。
    pub fn resolve_key_candidates(
        &self,
        base_path: &std::path::Path,
    ) -> anyhow::Result<std::vec::Vec<String>> {
        use anyhow::Context as _;

        let primary_key = self
            .resolve_primary_key(base_path)
            .context("primary キーの計算に失敗しました")?;

        let mut candidates = std::vec::Vec::with_capacity(1 + self.fallback_keys.len());
        candidates.push(primary_key);
        candidates.extend(self.fallback_keys.iter().cloned());
        Ok(candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod expand_env_vars_tests {
        use super::*;

        #[test]
        fn test_no_placeholders() {
            // Arrange
            let input = "static-key";

            // Act
            let result = expand_env_vars(input);

            // Assert
            assert_eq!(result.unwrap(), "static-key");
        }

        #[test]
        fn test_single_var() {
            // Arrange
            std::env::set_var("CAFCE_TEST_VAR_EXPAND", "hello");

            // Act
            let result = expand_env_vars("prefix-${CAFCE_TEST_VAR_EXPAND}-suffix");

            // Assert
            assert_eq!(result.unwrap(), "prefix-hello-suffix");
        }

        #[test]
        fn test_multiple_vars() {
            // Arrange
            std::env::set_var("CAFCE_TEST_A", "foo");
            std::env::set_var("CAFCE_TEST_B", "bar");

            // Act
            let result = expand_env_vars("${CAFCE_TEST_A}-${CAFCE_TEST_B}");

            // Assert
            assert_eq!(result.unwrap(), "foo-bar");
        }

        #[test]
        fn test_undefined_var_is_error() {
            // Arrange
            let input = "${CAFCE_DEFINITELY_NOT_SET_XYZ123}";

            // Act
            let result = expand_env_vars(input);

            // Assert
            assert!(matches!(
                result,
                Err(SettingError::UndefinedVariable { .. })
            ));
        }

        #[test]
        fn test_defined_but_empty_var_passes() {
            // Arrange
            std::env::set_var("CAFCE_TEST_EMPTY", "");

            // Act
            let result = expand_env_vars("prefix-${CAFCE_TEST_EMPTY}");

            // Assert: 空文字展開は通す（downstream validation が担う）
            assert_eq!(result.unwrap(), "prefix-");
        }

        #[test]
        fn test_unterminated_ref_is_error() {
            // Arrange
            let input = "cache-${UNCLOSED";

            // Act
            let result = expand_env_vars(input);

            // Assert
            assert!(matches!(result, Err(SettingError::UnterminatedVariableRef)));
        }

        #[test]
        fn test_no_bare_dollar_expansion() {
            // Arrange
            std::env::set_var("CAFCE_TEST_BARE", "should-not-expand");

            // Act: $VAR（波括弧なし）は展開しない
            let result = expand_env_vars("$CAFCE_TEST_BARE");

            // Assert
            assert_eq!(result.unwrap(), "$CAFCE_TEST_BARE");
        }

        #[test]
        fn test_no_recursive_expansion() {
            // Arrange
            std::env::set_var("CAFCE_TEST_RECURSE", "${CAFCE_TEST_BARE}");

            // Act: 展開結果に ${...} が含まれていても再展開しない
            let result = expand_env_vars("${CAFCE_TEST_RECURSE}");

            // Assert
            assert_eq!(result.unwrap(), "${CAFCE_TEST_BARE}");
        }
    }

    mod new_from_str_tests {
        use super::*;

        #[test]
        fn test_literal_key_form() {
            // Arrange
            let toml = r#"
project = "my-app"
key = "cache-v1"
fallback_keys = []
paths = []
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            assert_eq!(setting.project, "my-app");
            assert!(
                matches!(&setting.key, serde_either::StringOrStruct::String(s) if s == "cache-v1")
            );
            assert!(setting.fallback_keys.is_empty());
        }

        #[test]
        fn test_files_based_table_form() {
            // Arrange
            let toml = r#"
project = "my-app"
fallback_keys = []
paths = []

[key]
files = ["Cargo.lock", "package.json"]
prefix = "deps-v1"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            assert_eq!(setting.project, "my-app");
            match &setting.key {
                serde_either::StringOrStruct::Struct(k) => {
                    assert_eq!(k.files, vec!["Cargo.lock", "package.json"]);
                    assert_eq!(k.prefix.as_deref(), Some("deps-v1"));
                }
                _ => panic!("expected Struct form"),
            }
        }

        #[test]
        fn test_files_based_inline_table_form() {
            // Arrange
            let toml = r#"
project = "my-app"
key = { files = ["Cargo.lock"], prefix = "deps-v1" }
fallback_keys = []
paths = []
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            match &setting.key {
                serde_either::StringOrStruct::Struct(k) => {
                    assert_eq!(k.files, vec!["Cargo.lock"]);
                    assert_eq!(k.prefix.as_deref(), Some("deps-v1"));
                }
                _ => panic!("expected Struct form"),
            }
        }

        #[test]
        fn test_defaults_for_optional_fields() {
            // Arrange: paths と fallback_keys を省略
            let toml = r#"
project = "my-app"
key = "cache-v1"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            assert!(setting.paths.is_empty());
            assert!(setting.fallback_keys.is_empty());
        }

        #[test]
        fn test_fallback_keys_non_empty() {
            // Arrange
            let toml = r#"
project = "my-app"
key = "cache-v1"
fallback_keys = ["cache-main", "cache-default"]
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            assert_eq!(setting.fallback_keys, vec!["cache-main", "cache-default"]);
        }

        #[test]
        fn test_prefix_without_files_in_key_struct() {
            // Arrange: prefix あり・files あり
            let toml = r#"
project = "my-app"
key = { files = ["lock.txt"], prefix = "v2" }
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            match &setting.key {
                serde_either::StringOrStruct::Struct(k) => {
                    assert_eq!(k.prefix.as_deref(), Some("v2"));
                }
                _ => panic!("expected Struct form"),
            }
        }

        #[test]
        fn test_var_expansion_in_literal_key() {
            // Arrange
            std::env::set_var("CAFCE_TEST_BRANCH", "main");
            let toml = r#"
project = "my-app"
key = "cache-${CAFCE_TEST_BRANCH}"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            assert!(
                matches!(&setting.key, serde_either::StringOrStruct::String(s) if s == "cache-main")
            );
        }

        #[test]
        fn test_var_expansion_in_project() {
            // Arrange
            std::env::set_var("CAFCE_TEST_PROJECT_ID", "42");
            let toml = r#"
project = "proj-${CAFCE_TEST_PROJECT_ID}"
key = "cache-v1"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            assert_eq!(setting.project, "proj-42");
        }

        #[test]
        fn test_var_expansion_in_key_prefix() {
            // Arrange
            std::env::set_var("CAFCE_TEST_PREFIX_VER", "v3");
            let toml = r#"
project = "my-app"
key = { files = ["lock.txt"], prefix = "deps-${CAFCE_TEST_PREFIX_VER}" }
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            match &setting.key {
                serde_either::StringOrStruct::Struct(k) => {
                    assert_eq!(k.prefix.as_deref(), Some("deps-v3"));
                }
                _ => panic!("expected Struct form"),
            }
        }

        #[test]
        fn test_var_expansion_in_fallback_keys() {
            // Arrange
            std::env::set_var("CAFCE_TEST_DEFAULT_BRANCH", "main");
            let toml = r#"
project = "my-app"
key = "cache-v1"
fallback_keys = ["cache-${CAFCE_TEST_DEFAULT_BRANCH}", "cache-default"]
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            assert_eq!(setting.fallback_keys, vec!["cache-main", "cache-default"]);
        }

        #[test]
        fn test_files_patterns_not_expanded() {
            // Arrange: files 内の ${...} は展開しない（glob パターンとして扱う）
            std::env::set_var("CAFCE_TEST_GLOB_VAR", "should-not-expand");
            let toml = r#"
project = "my-app"
key = { files = ["${CAFCE_TEST_GLOB_VAR}/*.lock"] }
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            let setting = result.unwrap();
            match &setting.key {
                serde_either::StringOrStruct::Struct(k) => {
                    assert_eq!(k.files, vec!["${CAFCE_TEST_GLOB_VAR}/*.lock"]);
                }
                _ => panic!("expected Struct form"),
            }
        }
    }

    mod paths_tests {
        use super::*;

        #[test]
        fn test_paths_accepts_multiple_glob_patterns() {
            // Arrange: ディレクトリ名・ワイルドカード・別ディレクトリを混ぜる。
            // key.files と違い件数上限が無いので、複数書けること自体を固定する
            let toml = r#"
project = "my-app"
key = "cache-v1"
paths = ["target", "**/*.lock", "node_modules"]
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert: 順序も含めて書いたままパースされる（解決とバリデーションは
            // path_matcher の責務なので、ここでは値の受け渡しだけを見る）
            let setting = result.unwrap();
            assert_eq!(setting.paths, vec!["target", "**/*.lock", "node_modules"]);
        }

        #[test]
        fn test_paths_are_not_expanded() {
            // Arrange: project や key とは扱いが違い、paths は ${VAR} 展開しない。
            // 展開してしまうと、キャッシュ対象が実行環境によって変わり
            // store と restore で食い違う余地が生まれる（#6 で確定済みの方針）
            std::env::set_var("CAFCE_TEST_PATHS_VAR", "should-not-expand");
            let toml = r#"
project = "my-app"
key = "cache-v1"
paths = ["${CAFCE_TEST_PATHS_VAR}/target"]
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert: 変数が定義済みでも展開されず、リテラルのまま残る
            let setting = result.unwrap();
            assert_eq!(setting.paths, vec!["${CAFCE_TEST_PATHS_VAR}/target"]);
        }

        #[test]
        fn test_paths_omitted_defaults_to_empty() {
            // Arrange: paths を書かない config（key / probe だけを使う運用では正当）
            let toml = r#"
project = "my-app"
key = "cache-v1"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert: パース時点ではエラーにしない。store を実行したときに
            // path_matcher が空を弾く、という分担にしている
            assert!(result.unwrap().paths.is_empty());
        }
    }

    mod resolve_key_candidates_tests {
        use super::*;

        #[test]
        fn test_primary_only_when_no_fallback() {
            // Arrange: fallback_keys を書かない config
            let setting = Setting::new_from_str(
                r#"
project = "my-app"
key = "cache-v1"
"#,
            )
            .unwrap();

            // Act
            let candidates = setting
                .resolve_key_candidates(std::path::Path::new("."))
                .unwrap();

            // Assert: primary だけの 1 要素になる。空や 0 要素になると
            // probe / restore が何も試さず必ず miss する
            assert_eq!(candidates, vec!["cache-v1"]);
        }

        #[test]
        fn test_primary_comes_first_then_fallbacks_in_order() {
            // Arrange: feature ブランチ → main → 既定、という典型的な優先順位
            let setting = Setting::new_from_str(
                r#"
project = "my-app"
key = "cache-feature"
fallback_keys = ["cache-main", "cache-default"]
"#,
            )
            .unwrap();

            // Act
            let candidates = setting
                .resolve_key_candidates(std::path::Path::new("."))
                .unwrap();

            // Assert: primary が先頭で、fallback_keys は config に書いた順のまま。
            // この並びが probe と restore の共通の試行順になるため、
            // 入れ替わると「probe は true なのに restore が別のキャッシュを引く」が起きる
            assert_eq!(
                candidates,
                vec!["cache-feature", "cache-main", "cache-default"]
            );
        }

        #[test]
        fn test_files_based_key_is_computed_as_primary() {
            // Arrange: key が files 形態の場合。literal String と違い primary は
            // 計算結果になる。ここでは files が 0 件マッチなので、
            // GitLab CI 互換の default フォールバックが効く
            let temp_dir = tempfile::tempdir().unwrap();
            let setting = Setting::new_from_str(
                r#"
project = "my-app"
key = { files = ["does-not-exist.lock"], prefix = "deps" }
fallback_keys = ["cache-main"]
"#,
            )
            .unwrap();

            // Act
            let candidates = setting.resolve_key_candidates(temp_dir.path()).unwrap();

            // Assert: 先頭は計算済みの文字列（prefix 付き）で、fallback_keys は
            // そのまま後ろに続く。計算経路が resolve_primary_key と共通であることの確認
            assert_eq!(candidates, vec!["deps-default", "cache-main"]);
        }
    }

    mod parse_error_tests {
        use super::*;

        #[test]
        fn test_missing_project_is_error() {
            // Arrange
            let toml = r#"
key = "cache-v1"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            assert!(result.is_err());
        }

        #[test]
        fn test_empty_project_is_error() {
            // Arrange
            let toml = r#"
project = ""
key = "cache-v1"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            assert!(matches!(result, Err(SettingError::EmptyProject)));
        }

        #[test]
        fn test_project_expanding_to_empty_is_error() {
            // Arrange
            std::env::set_var("CAFCE_TEST_EMPTY_PROJ", "");
            let toml = r#"
project = "${CAFCE_TEST_EMPTY_PROJ}"
key = "cache-v1"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            assert!(matches!(result, Err(SettingError::EmptyProject)));
        }

        #[test]
        fn test_missing_key_is_error() {
            // Arrange
            let toml = r#"
project = "my-app"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            assert!(result.is_err());
        }

        #[test]
        fn test_wrong_type_for_project_is_error() {
            // Arrange
            let toml = r#"
project = 123
key = "cache-v1"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            assert!(result.is_err());
        }

        #[test]
        fn test_unknown_field_is_error() {
            // Arrange
            let toml = r#"
project = "my-app"
key = "cache-v1"
unknown_field = "oops"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            assert!(result.is_err());
        }

        #[test]
        fn test_undefined_var_in_key_is_error() {
            // Arrange
            let toml = r#"
project = "my-app"
key = "cache-${CAFCE_UNDEFINED_VAR_FOR_TEST_XYZ}"
"#;

            // Act
            let result = Setting::new_from_str(toml);

            // Assert
            assert!(matches!(
                result,
                Err(SettingError::UndefinedVariable { .. })
            ));
        }
    }

    mod sample_file_regression_test {
        use super::*;

        #[test]
        fn test_sample_setting_toml_parses() {
            // Arrange
            let path = std::path::Path::new("test/sample/setting.toml");

            // Act
            let result = Setting::new_from_file(path);

            // Assert
            let setting = result.unwrap();
            assert_eq!(setting.project, "sample-project");
            assert!(matches!(
                &setting.key,
                serde_either::StringOrStruct::Struct(_)
            ));
        }
    }
}
