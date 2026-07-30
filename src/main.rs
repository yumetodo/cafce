/// Cache management tool for CI pipelines
#[derive(Debug, Clone, bpaf::Bpaf)]
#[bpaf(options, version)]
struct Opts {
    #[bpaf(external)]
    action: Action,
}

#[derive(Debug, Clone, bpaf::Bpaf)]
enum Action {
    /// Store the configured paths into the cache (outputs "true" when uploaded)
    #[bpaf(command)]
    Store {
        /// Path to the configuration file
        #[bpaf(positional("CONFIG"))]
        config: std::path::PathBuf,
    },

    /// Restore files from the cache (outputs "true" when extracted)
    #[bpaf(command)]
    Restore {
        /// Path to the configuration file
        #[bpaf(positional("CONFIG"))]
        config: std::path::PathBuf,
    },

    /// Initialize a new configuration file
    #[bpaf(command)]
    Init {
        /// Path to write the configuration file
        #[bpaf(positional("CONFIG"))]
        config: std::path::PathBuf,
    },

    /// Print the primary cache key computed from the configuration
    #[bpaf(command)]
    Key {
        /// Path to the configuration file
        #[bpaf(positional("CONFIG"))]
        config: std::path::PathBuf,
    },

    /// Check whether the cache exists in S3 (outputs "true" or "false")
    #[bpaf(command)]
    Probe {
        /// Path to the configuration file
        #[bpaf(positional("CONFIG"))]
        config: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    match run(opts().run()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(opts: Opts) -> anyhow::Result<()> {
    use anyhow::Context as _;

    match opts.action {
        Action::Key { config } => {
            let cwd =
                std::env::current_dir().context("カレントディレクトリの取得に失敗しました")?;
            let setting = cafce::setting::Setting::new_from_file(&config).with_context(|| {
                format!("設定ファイルの読み込みに失敗しました: {}", config.display())
            })?;
            let key = setting
                .resolve_primary_key(&cwd)
                .context("primary キーの計算に失敗しました")?;
            println!("{key}");
        }

        Action::Probe { config } => {
            let cwd =
                std::env::current_dir().context("カレントディレクトリの取得に失敗しました")?;
            let setting = cafce::setting::Setting::new_from_file(&config).with_context(|| {
                format!("設定ファイルの読み込みに失敗しました: {}", config.display())
            })?;
            let env = cafce::env::Env::new().context("環境変数の読み込みに失敗しました")?;
            let client = cafce::s3_client::build_s3_client(&env)
                .await
                .context("S3 クライアントの構築に失敗しました")?;
            let hit = cafce::probe::probe(&setting, &env, &client, &cwd).await?;
            println!("{}", if hit { "true" } else { "false" });
        }

        Action::Init { config } => {
            cafce::setting::Setting::init_to_file(&config).with_context(|| {
                format!("設定ファイルの初期化に失敗しました: {}", config.display())
            })?;
        }

        Action::Store { config } => {
            let cwd =
                std::env::current_dir().context("カレントディレクトリの取得に失敗しました")?;
            let setting = cafce::setting::Setting::new_from_file(&config).with_context(|| {
                format!("設定ファイルの読み込みに失敗しました: {}", config.display())
            })?;
            let env = cafce::env::Env::new().context("環境変数の読み込みに失敗しました")?;
            let client = cafce::s3_client::build_s3_client(&env)
                .await
                .context("S3 クライアントの構築に失敗しました")?;
            let uploaded = cafce::store::store(&setting, &env, &client, &cwd).await?;
            println!("{}", if uploaded { "true" } else { "false" });
        }

        Action::Restore { config } => {
            let cwd =
                std::env::current_dir().context("カレントディレクトリの取得に失敗しました")?;
            let setting = cafce::setting::Setting::new_from_file(&config).with_context(|| {
                format!("設定ファイルの読み込みに失敗しました: {}", config.display())
            })?;
            let env = cafce::env::Env::new().context("環境変数の読み込みに失敗しました")?;
            let client = cafce::s3_client::build_s3_client(&env)
                .await
                .context("S3 クライアントの構築に失敗しました")?;
            let restored = cafce::restore::restore(&setting, &env, &client, &cwd).await?;
            println!("{}", if restored { "true" } else { "false" });
        }
    }

    Ok(())
}
