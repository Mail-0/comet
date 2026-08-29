use std::path::PathBuf;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    keiki_ui::run_app(keiki_ui::UiConfig {
        data_dir: data_dir(),
        api_base_url: std::env::var("KEIKI_API_URL")
            .unwrap_or_else(|_| "https://onkeiki.com".into()),
    });
    Ok(())
}

fn data_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("KEIKI_DATA_DIR") {
        return path.into();
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Keiki")
    } else {
        home.join(".keiki")
    }
}
