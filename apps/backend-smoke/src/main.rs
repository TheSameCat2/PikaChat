use std::{env, path::PathBuf};

use backend_matrix::{MatrixBackend, MatrixBackendConfig};

#[tokio::main]
async fn main() {
    let homeserver =
        env::var("PIKACHAT_HOMESERVER").unwrap_or_else(|_| "https://matrix.example.org".to_owned());
    let data_dir = env::var("PIKACHAT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./.pikachat-smoke-store"));

    let config = MatrixBackendConfig::new(homeserver, data_dir, None);
    match MatrixBackend::new(config).await {
        Ok(_) => {
            println!("Matrix backend initialized. Set credentials env vars to run live smoke.");
            println!("Required for live auth: PIKACHAT_USER and PIKACHAT_PASSWORD");
        }
        Err(err) => {
            eprintln!("Failed to initialize backend: {err}");
            std::process::exit(1);
        }
    }
}
