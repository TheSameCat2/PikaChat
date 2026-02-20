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
        Ok(backend) => {
            println!("Matrix backend initialized.");

            let maybe_user = env::var("PIKACHAT_USER").ok();
            let maybe_password = env::var("PIKACHAT_PASSWORD").ok();

            if let (Some(user), Some(password)) = (maybe_user, maybe_password) {
                backend
                    .login_password(&user, &password, "PikaChat Smoke")
                    .await
                    .expect("live login failed");
                println!("Login successful for {user}");

                backend.sync_once().await.expect("sync_once failed");
                println!("sync_once completed");

                if let Some(dm_target) = env::var("PIKACHAT_DM_TARGET").ok() {
                    let dm_body = env::var("PIKACHAT_DM_BODY")
                        .unwrap_or_else(|_| "PikaChat backend smoke test".to_owned());
                    let event_id = backend
                        .send_dm_text(&dm_target, &dm_body)
                        .await
                        .expect("DM send failed");
                    println!("Sent DM to {dm_target} as event {event_id}");
                }
            } else {
                println!("Set PIKACHAT_USER and PIKACHAT_PASSWORD to run live auth smoke.");
                println!("Optional: set PIKACHAT_DM_TARGET to send a DM smoke message.");
            }
        }
        Err(err) => {
            eprintln!("Failed to initialize backend: {err}");
            std::process::exit(1);
        }
    }
}
