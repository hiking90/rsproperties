use std::fs::{create_dir_all, remove_dir_all};
use std::{path::PathBuf, str::FromStr};

use tokio::sync::OnceCell;

use rsproperties::PropertyConfig;

use rsproperties_service::ServiceHandle;

pub static TEST_PROPERTIES_DIR: &str = "__properties__";

static SERVICE: OnceCell<()> = OnceCell::const_new();

async fn start_service() -> ServiceHandle {
    let _ = env_logger::builder().is_test(true).try_init();

    let properties_dir =
        PathBuf::from_str(TEST_PROPERTIES_DIR).expect("Failed to parse properties directory path");
    let socket_dir = properties_dir.join("sockets");

    remove_dir_all(&properties_dir).unwrap_or_default();
    remove_dir_all(&socket_dir).unwrap_or_default();
    create_dir_all(&properties_dir).expect("Failed to create properties directory");
    create_dir_all(&socket_dir).expect("Failed to create socket directory");

    let config = PropertyConfig::new()
        .properties_dir(properties_dir)
        .socket_dir(socket_dir);

    rsproperties_service::run(config, rsproperties_service::ServiceOptions::new())
        .await
        .expect("Failed to start services")
}

/// Starts the service once per test binary, on its own thread and
/// runtime, and returns when it is ready. The service runs until the
/// process exits.
pub async fn init_test() {
    SERVICE
        .get_or_init(|| async {
            let (sender, receiver) = tokio::sync::oneshot::channel::<()>();

            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create Tokio runtime");

                runtime.block_on(async {
                    let mut service = start_service().await;
                    let _ = sender.send(());

                    if let Err(e) = service.join().await {
                        eprintln!("Property service error: {e}");
                    }
                });
            });

            receiver.await.expect("Failed to receive services")
        })
        .await;
}
