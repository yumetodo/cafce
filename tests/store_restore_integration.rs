/// store / restore サブコマンドの RustFS 統合テスト
///
/// このモジュールのテストは通常の `cargo test` 実行ではスキップされる（`#[ignore]` 指定）。
/// 実行するには、リポジトリルートの `docker-compose.yml` で RustFS を事前に起動しておく必要がある:
///
/// ```sh
/// docker compose up -d
/// cargo test store_restore_integration -- --ignored --nocapture
/// ```
///
/// 同じファイルの後半には実 AWS S3 宛の `aws_integration_tests` があるため、
/// `--test store_restore_integration -- --ignored` のようにファイル単位で指定すると
/// そちらまで走ってしまう。上のようにモジュール名でフィルタして実行すること。
///
/// RustFS は beta 段階で実運用実績が少ないため、user metadata のラウンドトリップや
/// フレキシブルチェックサムの挙動が仕様どおりとは限らない。設計doc 10 節のとおり、
/// ここで実際のラウンドトリップを確認することを前提としている。
#[cfg(test)]
mod store_restore_integration_tests {
    fn rustfs_env(bucket: &str, s3_checksum: cafce::env::S3ChecksumMode) -> cafce::env::Env {
        cafce::env::Env::new_for_test_with_bucket(cafce::env::TestEnvParams {
            server_address: Some("localhost:9000".to_string()),
            access_key: Some("cafce-dev-access-key".to_string()),
            secret_key: Some("cafce-dev-secret-key".to_string()),
            insecure: true,
            region: None,
            bucket: bucket.to_string(),
            s3_prefix: None,
            s3_checksum,
        })
    }

    fn unique_name(prefix: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before UNIX_EPOCH")
            .as_nanos();
        format!("{prefix}-{nanos}")
    }

    fn make_setting(
        project: &str,
        primary_key: &str,
        fallback_keys: std::vec::Vec<String>,
        paths: std::vec::Vec<String>,
    ) -> cafce::setting::Setting {
        cafce::setting::Setting {
            project: project.to_string(),
            paths,
            key: serde_either::StringOrStruct::String(primary_key.to_string()),
            fallback_keys,
        }
    }

    /// キャッシュ対象として使うファイル木を作る
    ///
    /// ファイル・ネストしたディレクトリ・空ディレクトリ・シンボリックリンクを含める。
    /// tar のエントリ型を一通り通すことで、S3 を経由しても情報が落ちないことを確認する。
    ///
    /// `marker` は内容へ埋め込む識別子で、復元されたファイルがどの世代のものかを
    /// 見分けるために使う（上書きテストで「古い内容が残っていない」ことを言うために要る）。
    fn create_source_tree(base_path: &std::path::Path, marker: &str) {
        std::fs::create_dir_all(base_path.join("target/debug")).unwrap();
        std::fs::create_dir_all(base_path.join("target/empty")).unwrap();
        std::fs::write(
            base_path.join("target/debug/app"),
            format!("binary-{marker}"),
        )
        .unwrap();
        std::fs::write(base_path.join("target/.fingerprint"), marker).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("debug/app", base_path.join("target/app-link")).unwrap();
    }

    fn assert_source_tree_restored(base_path: &std::path::Path, marker: &str) {
        assert_eq!(
            std::fs::read_to_string(base_path.join("target/debug/app")).unwrap(),
            format!("binary-{marker}")
        );
        assert_eq!(
            std::fs::read_to_string(base_path.join("target/.fingerprint")).unwrap(),
            marker
        );
        assert!(base_path.join("target/empty").is_dir());
        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(base_path.join("target/app-link"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// バケット内の全オブジェクトを削除する（delete_bucket の前処理）
    async fn delete_all_objects(client: &aws_sdk_s3::Client, bucket: &str) {
        let list_resp = client
            .list_objects_v2()
            .bucket(bucket)
            .send()
            .await
            .unwrap_or_else(|e| panic!("list_objects_v2({bucket}) failed: {e:?}"));

        for obj in list_resp.contents() {
            let key = obj.key().expect("S3 object key must not be None");
            client
                .delete_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .unwrap_or_else(|e| panic!("delete_object({bucket}/{key}) failed: {e:?}"));
        }
    }

    /// バケット作成→テスト実行→全オブジェクト削除→バケット削除のラッパー
    async fn with_bucket<F, Fut>(client: &aws_sdk_s3::Client, bucket: &str, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        client
            .create_bucket()
            .bucket(bucket)
            .send()
            .await
            .unwrap_or_else(|e| panic!("create_bucket({bucket}) failed: {e:?}"));

        f().await;

        // 非空バケットは削除できないため、先にオブジェクトを全削除する
        delete_all_objects(client, bucket).await;

        client
            .delete_bucket()
            .bucket(bucket)
            .send()
            .await
            .unwrap_or_else(|e| panic!("delete_bucket({bucket}) failed: {e:?}"));
    }

    async fn head_object(
        client: &aws_sdk_s3::Client,
        bucket: &str,
        key: &str,
    ) -> aws_sdk_s3::operation::head_object::HeadObjectOutput {
        client
            .head_object()
            .bucket(bucket)
            .key(key)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await
            .unwrap_or_else(|e| panic!("head_object({bucket}/{key}) failed: {e:?}"))
    }

    /// store → probe → restore のラウンドトリップで元のファイル木が復元される
    #[tokio::test]
    #[ignore]
    async fn test_store_probe_restore_roundtrip() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "roundtrip");

        with_bucket(&client, &bucket, || async {
            // Act: store した木とは別ディレクトリへ restore する。同じ場所だと
            // 元のファイルが残っているだけで通ってしまい、復元を確認できない
            let uploaded = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let probed = cafce::probe::probe(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let restored = cafce::restore::restore(&setting, &env, &client, restore_dir.path())
                .await
                .unwrap();

            // Assert: probe も挟むのは「probe が true を返す状況では restore も必ず
            // ヒットする」という不変条件（設計doc 6.9）を実機で確認するため
            assert!(uploaded, "初回なのでアップロードされるはず");
            assert!(probed, "store 済みなので probe は true のはず");
            assert!(restored, "store 済みなので restore は true のはず");
            assert_source_tree_restored(restore_dir.path(), "roundtrip");
        })
        .await;
    }

    /// 同一内容で store を 2 回実行すると 2 回目はアップロードが省略される
    #[tokio::test]
    #[ignore]
    async fn test_store_twice_with_same_content_skips_upload() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "same");
        let object_key = format!("{project}/cache-v1");

        with_bucket(&client, &bucket, || async {
            let first = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let first_modified = head_object(&client, &bucket, &object_key)
                .await
                .last_modified;

            // Act: ファイルを一切触らずに再実行する。アーカイブは決定論的なので
            // 内容ハッシュも同じ値になり、metadata と一致するはず
            let second = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let second_modified = head_object(&client, &bucket, &object_key)
                .await
                .last_modified;

            // Assert: 戻り値の false だけでは「PutObject を打たなかった」ことの証明に
            // ならないので、LastModified が変わっていないことをサーバ側の事実として見る。
            // これは同時に、RustFS が user metadata をラウンドトリップしていることの
            // 裏付けにもなっている（返らなければハッシュ照合ができず必ず再送になる）
            assert!(first, "初回はアップロードされるはず");
            assert!(!second, "内容が同一なのでアップロードは省略されるはず");
            assert_eq!(
                first_modified, second_modified,
                "アップロードが省略されたなら LastModified は変わらないはず"
            );
        })
        .await;
    }

    /// 内容を変えて store すると上書きされ、restore が新しい内容を返す
    #[tokio::test]
    #[ignore]
    async fn test_store_with_changed_content_overwrites() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "before");

        with_bucket(&client, &bucket, || async {
            cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();

            // Act: キーは変えずに中身だけ差し替える（同じブランチでコードが進んだ状況）。
            // marker を "before" から "after" へ変えることで、restore 後に
            // どちらの世代が入っているか判定できる
            std::fs::write(source_dir.path().join("target/debug/app"), "binary-after").unwrap();
            std::fs::write(source_dir.path().join("target/.fingerprint"), "after").unwrap();
            let second = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            cafce::restore::restore(&setting, &env, &client, restore_dir.path())
                .await
                .unwrap();

            // Assert: 上書きされ、restore が新しい世代を返すこと。
            // ハッシュ照合が雑だと古い内容のまま省略され、ここで "before" が出る
            assert!(second, "内容が変わったので上書きアップロードされるはず");
            assert_source_tree_restored(restore_dir.path(), "after");
        })
        .await;
    }

    /// primary が miss で fallback が hit のとき、fallback のアーカイブが展開される
    #[tokio::test]
    #[ignore]
    async fn test_restore_falls_back_to_fallback_key() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let store_setting =
            make_setting(&project, "cache-main", vec![], vec!["target".to_string()]);
        let restore_setting = make_setting(
            &project,
            "cache-feature-branch",
            vec!["cache-main".to_string()],
            vec![],
        );
        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "fallback");

        with_bucket(&client, &bucket, || async {
            // store 用と restore 用で別の Setting を使う。store は常に primary へ書くので、
            // 「fallback にだけ存在する」状態を作るには primary key を cache-main にした
            // Setting で store する必要がある
            cafce::store::store(&store_setting, &env, &client, source_dir.path())
                .await
                .unwrap();

            // Act
            let restored =
                cafce::restore::restore(&restore_setting, &env, &client, restore_dir.path())
                    .await
                    .unwrap();

            // Assert: primary が miss でも fallback のアーカイブが展開される。
            // restore は paths を参照しないので、restore_setting の paths が空でも問題ない
            // ——という設計もここで一緒に確認できている
            assert!(restored, "fallback key が存在するので true のはず");
            assert_source_tree_restored(restore_dir.path(), "fallback");
        })
        .await;
    }

    /// 全 miss のとき restore は false を返し、作業ディレクトリを触らない
    #[tokio::test]
    #[ignore]
    async fn test_restore_all_miss_returns_false() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(
            &project,
            "cache-v1",
            vec!["cache-main".to_string(), "cache-default".to_string()],
            vec![],
        );
        let restore_dir = tempfile::tempdir().unwrap();

        with_bucket(&client, &bucket, || async {
            // Act: バケットは作るが何も置かない。3 つのキー候補すべてが miss する
            let restored = cafce::restore::restore(&setting, &env, &client, restore_dir.path())
                .await
                .unwrap();

            // Assert: エラーではなく false + exit 0 相当（`unwrap()` が通ること自体が
            // それを示す）。CI スクリプトが `$(cafce restore ...)` で分岐できる前提を守る。
            // 併せて、miss の過程で一時ファイルや空ディレクトリを作業ディレクトリへ
            // 撒いていないことも確認する
            assert!(!restored, "何も存在しないので false のはず");
            assert_eq!(
                std::fs::read_dir(restore_dir.path()).unwrap().count(),
                0,
                "全 miss のときは作業ディレクトリを触らないはず"
            );
        })
        .await;
    }

    /// store が付けた user metadata が RustFS でラウンドトリップする
    #[tokio::test]
    #[ignore]
    async fn test_user_metadata_roundtrips() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "metadata");
        let object_key = format!("{project}/cache-v1");

        with_bucket(&client, &bucket, || async {
            cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();

            // Act: cafce 自身と同じ経路（parse_metadata）で読み返す
            let output = head_object(&client, &bucket, &object_key).await;
            let parsed = cafce::cache_metadata::parse_metadata(output.metadata()).unwrap();

            // Assert: 設計doc 8 節で「rustfs が user metadata を保持して返すかは
            // 実機確認が要る」と挙げていた項目。返らなければ Ok(None) になり、
            // 「メタデータ欠落 = 常に再アップロード」へ退化する（正しさは保たれるが
            // アップロード省略が効かなくなる）
            let parsed = parsed.expect("user metadata がラウンドトリップしていない");
            assert_eq!(parsed.schema_version, cafce::cache_metadata::SCHEMA_VERSION);
            assert_eq!(parsed.archive_format, cafce::cache_metadata::ARCHIVE_FORMAT);
            assert_eq!(parsed.content_sha256.len(), 64);
        })
        .await;
    }

    /// チェックサム付きの PutObject が受理され、HeadObject で値が返る
    ///
    /// 値が返らない場合でも store / restore が成立することを併せて確認する
    /// （フレキシブルチェックサムはあくまで上積みであり、内容の検証は metadata ハッシュが担う）。
    #[tokio::test]
    #[ignore]
    async fn test_flexible_checksum_is_accepted() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "checksum");
        let object_key = format!("{project}/cache-v1");

        with_bucket(&client, &bucket, || async {
            // Act: CAFCE_S3_CHECKSUM=auto なので事前計算した x-amz-checksum-sha256 を
            // 付けて PutObject する。非対応サーバならここでフォールバックが走る
            let uploaded = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let output = head_object(&client, &bucket, &object_key).await;
            let restored = cafce::restore::restore(&setting, &env, &client, restore_dir.path())
                .await
                .unwrap();

            // Assert: 受理されること自体（アーカイブ生成時に計算した値がサーバの
            // 再計算と一致すること）がまず確認したい点。ずれていれば BadDigest になる
            assert!(uploaded, "チェックサム付きの PutObject が受理されるはず");
            assert!(restored);
            assert_source_tree_restored(restore_dir.path(), "checksum");
            // 保管された値が HeadObject で返るかはサーバ実装依存なので、
            // 返らないこと自体はテストの失敗にしない。フレキシブルチェックサムは
            // あくまで上積みであり、内容の検証は metadata ハッシュが独立に担っている
            match output.checksum_sha256() {
                Some(checksum) => {
                    assert_eq!(checksum.len(), 44, "32 バイトの Base64 は 44 文字のはず")
                }
                None => eprintln!(
                    "注意: RustFS は HeadObject で x-amz-checksum-sha256 を返さなかった。\
                     store / restore は成立している"
                ),
            }
        })
        .await;
    }

    /// 意図的に壊した値をチェックサムとして渡した PutObject が拒否される
    ///
    /// サーバ側照合が実際に効いていることの確認。効いていない場合はテストを失敗させず、
    /// その旨を記録する（RustFS の対応状況は beta 段階で変わりうるため）。
    #[tokio::test]
    #[ignore]
    async fn test_corrupted_checksum_is_rejected() {
        // Arrange: cafce の store は正しい値しか送らないため、サーバ側照合が効いている
        // ことを確かめるには SDK を直接叩いて意図的に壊した値を渡す必要がある
        let bucket = unique_name("cafce-store-test");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let body: &[u8] = b"cafce-corrupted-checksum-test";
        // 本文とは無関係な値（空文字列の SHA-256）。形式としては妥当な 44 文字なので、
        // 「形式エラーで弾かれた」のではなく「照合して不一致だった」ことが分かる
        let wrong_checksum = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";

        with_bucket(&client, &bucket, || async {
            // Act
            let result = client
                .put_object()
                .bucket(&bucket)
                .key("corrupted")
                .body(aws_sdk_s3::primitives::ByteStream::from_static(body))
                .checksum_algorithm(aws_sdk_s3::types::ChecksumAlgorithm::Sha256)
                .checksum_sha256(wrong_checksum)
                .send()
                .await;

            // Assert: 拒否されれば BadDigest が返る想定。cafce 側はこのコードを
            // 「非対応」ではなく「本物の破損」と解釈して再試行しない（store の
            // should_retry_without_checksum と対応する実機側の裏付け）
            match result {
                Err(e) => {
                    use aws_sdk_s3::error::ProvideErrorMetadata as _;
                    eprintln!("サーバ側照合が効いている: code={:?}", e.code());
                }
                Ok(_) => eprintln!(
                    "注意: RustFS は壊れた x-amz-checksum-sha256 を拒否しなかった。\
                     内容の検証は metadata ハッシュが独立に担っている"
                ),
            }
        })
        .await;
    }

    /// CAFCE_S3_CHECKSUM=off でチェックサム無しの経路でも store → restore が成立する
    #[tokio::test]
    #[ignore]
    async fn test_checksum_off_roundtrip() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Off);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec!["target".to_string()]);
        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        create_source_tree(source_dir.path(), "checksum-off");

        with_bucket(&client, &bucket, || async {
            // Act: Off なので PutObject に checksum を付けず、GetObject にも
            // checksum_mode を指定しない
            let uploaded = cafce::store::store(&setting, &env, &client, source_dir.path())
                .await
                .unwrap();
            let restored = cafce::restore::restore(&setting, &env, &client, restore_dir.path())
                .await
                .unwrap();

            // Assert: チェックサム無しでも正しさは損なわれない（内容の検証は
            // metadata ハッシュが独立に担う）。フレキシブルチェックサムに問題のある
            // サーバに当たった利用者の逃げ道が実際に機能することの確認
            assert!(uploaded);
            assert!(restored);
            assert_source_tree_restored(restore_dir.path(), "checksum-off");
        })
        .await;
    }

    /// paths が空のとき store はエラーになる
    #[tokio::test]
    #[ignore]
    async fn test_store_with_empty_paths_is_error() {
        // Arrange
        let bucket = unique_name("cafce-store-test");
        let project = unique_name("proj");
        let env = rustfs_env(&bucket, cafce::env::S3ChecksumMode::Auto);
        let client = cafce::s3_client::build_s3_client(&env).await.unwrap();
        let setting = make_setting(&project, "cache-v1", vec![], vec![]);
        let source_dir = tempfile::tempdir().unwrap();

        with_bucket(&client, &bucket, || async {
            // Act: paths が空の Setting で store する
            let result = cafce::store::store(&setting, &env, &client, source_dir.path()).await;

            // Assert: path_matcher の単体テストと重複するが、store が実際に
            // その検証を通していること（S3 へ空アーカイブを置いてしまわないこと）を
            // 経路として確認する。context の文言で store 側の呼び出し箇所を特定できる
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("キャッシュ対象の解決に失敗しました"));
        })
        .await;
    }
}

/// 実際の AWS S3 に対する store / restore のラウンドトリップ確認。
///
/// このモジュールのテストは通常の `cargo test` 実行ではスキップされる（`#[ignore]` 指定）。
/// RustFS 向けの `store_restore_integration_tests` とは完全に独立しており、
/// 実際の AWS アカウント上に用意したテスト専用の S3 バケットを使用する。
/// 署名・metadata の正規化・リージョンといった、ここでのみ検出できる差異を拾うためのものである。
///
/// 必要な AWS リソースと権限は `src/s3_client.rs` の `aws_integration_tests` と同じで、
/// バケットの作成・削除は行わないため管理者権限は要らない
/// （`s3:ListBucket` / `s3:GetObject` / `s3:PutObject` / `s3:DeleteObject` のみで通る）。
///
/// 実行に必要な環境変数:
/// - `CAFCE_AWS_ACCESS_KEY` - IAM ユーザーのアクセスキー ID
/// - `CAFCE_AWS_SECRET_KEY` - IAM ユーザーのシークレットアクセスキー
/// - `CAFCE_AWS_REGION` - バケットのリージョン（例: "ap-northeast-1"）
/// - `CAFCE_TEST_BUCKET` - テスト対象の既存 S3 バケット名
///
/// 実行方法:
/// ```sh
/// export CAFCE_AWS_ACCESS_KEY=...
/// export CAFCE_AWS_SECRET_KEY=...
/// export CAFCE_AWS_REGION=ap-northeast-1
/// export CAFCE_TEST_BUCKET=...
/// cargo test aws_integration -- --ignored --nocapture
/// ```
#[cfg(test)]
mod aws_integration_tests {
    /// 実 AWS S3 に対して store → restore のラウンドトリップを確認する
    ///
    /// バケットは既存のものを使い回すため、テスト後に自分が作ったオブジェクトだけを削除する
    /// （`create_bucket` / `delete_bucket` は権限外のため呼ばない）。
    #[tokio::test]
    #[ignore]
    async fn aws_store_restore_roundtrip_smoke_test() {
        // Arrange
        let access_key = std::env::var("CAFCE_AWS_ACCESS_KEY").unwrap_or_else(|_| {
            panic!(
                "環境変数CAFCE_AWS_ACCESS_KEYが設定されていません。\
                 AWS疎通確認テストの実行にはIAMユーザーのアクセスキーIDが必要です。"
            )
        });
        let secret_key = std::env::var("CAFCE_AWS_SECRET_KEY").unwrap_or_else(|_| {
            panic!(
                "環境変数CAFCE_AWS_SECRET_KEYが設定されていません。\
                 AWS疎通確認テストの実行にはIAMユーザーのシークレットアクセスキーが必要です。"
            )
        });
        let region = std::env::var("CAFCE_AWS_REGION").unwrap_or_else(|_| {
            panic!(
                "環境変数CAFCE_AWS_REGIONが設定されていません。\
                 テスト対象バケットのリージョン（例: ap-northeast-1）を指定してください。"
            )
        });
        let bucket = std::env::var("CAFCE_TEST_BUCKET").unwrap_or_else(|_| {
            panic!(
                "環境変数CAFCE_TEST_BUCKETが設定されていません。\
                 テスト専用に用意した既存のS3バケット名を指定してください。"
            )
        });

        // バケットを共有するため、project 名の方をユニーク化してキーの衝突を避ける
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is before UNIX_EPOCH")
            .as_nanos();
        let project = format!("cafce-aws-store-test-{nanos}");
        let object_key = format!("{project}/cache-v1");

        let env = cafce::env::Env::new_for_test_with_bucket(cafce::env::TestEnvParams {
            // server_address: AWS S3 のデフォルトエンドポイントを使う
            server_address: None,
            access_key: Some(access_key),
            secret_key: Some(secret_key),
            insecure: false,
            region: Some(region),
            bucket: bucket.clone(),
            s3_prefix: None,
            s3_checksum: cafce::env::S3ChecksumMode::Auto,
        });
        let client = cafce::s3_client::build_s3_client(&env)
            .await
            .expect("failed to build S3 client for AWS S3");
        let setting = cafce::setting::Setting {
            project: project.clone(),
            paths: vec!["target".to_string()],
            key: serde_either::StringOrStruct::String("cache-v1".to_string()),
            fallback_keys: vec![],
        };

        let source_dir = tempfile::tempdir().unwrap();
        let restore_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source_dir.path().join("target/debug")).unwrap();
        std::fs::write(source_dir.path().join("target/debug/app"), "binary-aws").unwrap();

        // Act: 実 AWS でのみ検出できる差異（署名、metadata の正規化、リージョン）を
        // 1 本で拾いたいので、初回 store・省略される 2 回目・restore をまとめて通す
        let uploaded = cafce::store::store(&setting, &env, &client, source_dir.path())
            .await
            .expect("store failed against AWS S3");
        let skipped = cafce::store::store(&setting, &env, &client, source_dir.path())
            .await
            .expect("second store failed against AWS S3");
        let restored = cafce::restore::restore(&setting, &env, &client, restore_dir.path())
            .await
            .expect("restore failed against AWS S3");

        // 後始末（アサート前に行い、失敗してもオブジェクトを残さない）
        client
            .delete_object()
            .bucket(&bucket)
            .key(&object_key)
            .send()
            .await
            .unwrap_or_else(|e| panic!("delete_object({bucket}/{object_key}) failed: {e:?}"));

        // Assert: 2 回目が false になることが、AWS S3 でも user metadata が
        // ラウンドトリップしている（＝小文字化などの正規化で読めなくなっていない）
        // ことの裏付けになっている
        assert!(uploaded, "初回なのでアップロードされるはず");
        assert!(!skipped, "内容が同一なのでアップロードは省略されるはず");
        assert!(restored, "store 済みなので restore は true のはず");
        assert_eq!(
            std::fs::read_to_string(restore_dir.path().join("target/debug/app")).unwrap(),
            "binary-aws"
        );
    }
}
