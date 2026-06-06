mod client;
mod config;

use crate::client::{LabelRequest, OpenAiLabelClient};
use crate::config::Config;
use anyhow::{Context, Result};
use clap::Parser;
use richclip::ipc::client as ipc_client;
use richclip::ipc::{Request, Response, UpdateItemParams, WatchEvent, WatchEventsParams};
use richclip::store::Store;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

const IMAGE_MIME_PRIORITY: [&str; 4] = ["image/png", "image/webp", "image/jpeg", "image/bmp"];
const LABEL_MIME: &str = "application/x-richclip-label";
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Parser)]
#[command(name = "richclip-labeld")]
#[command(about = "Contrib image labeller for richclip items")]
struct Cli {
    /// Override the default config path.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Replace existing application/x-richclip-label values.
    #[arg(long)]
    overwrite: bool,

    /// Override the daemon socket path.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Override the local richclip data directory.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

struct Runtime {
    client: OpenAiLabelClient,
    prompt: String,
    overwrite: bool,
    socket_path: PathBuf,
    store: Store,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    config.validate()?;

    let config_path = cli.config.unwrap_or_else(config::default_config_path);
    let socket_path = socket_path(cli.socket)?;
    let data_dir = data_dir(cli.data_dir)?;
    let store = Store::open(&data_dir)
        .with_context(|| format!("failed to open richclip store at {:?}", data_dir))?;
    let client = OpenAiLabelClient::new(&config.model)?;

    let runtime = Runtime {
        client,
        prompt: config.prompt.label,
        overwrite: cli.overwrite,
        socket_path,
        store,
    };

    eprintln!(
        "richclip-labeld watching {:?} using config {:?}",
        runtime.socket_path, config_path
    );

    run(runtime)
}

fn run(runtime: Runtime) -> Result<()> {
    let watch_request = Request::WatchEvents(WatchEventsParams {
        event_filter: Some("item-added".to_string()),
        mime_filters: IMAGE_MIME_PRIORITY
            .iter()
            .map(|mime| mime.to_string())
            .collect(),
    });
    let mut backoff = INITIAL_RECONNECT_BACKOFF;

    loop {
        let Some(stream) = ipc_client::try_connect(&runtime.socket_path) else {
            eprintln!(
                "richclip-labeld: daemon unavailable at {:?}; retrying in {}s",
                runtime.socket_path,
                backoff.as_secs()
            );
            thread::sleep(backoff);
            backoff = next_backoff(backoff);
            continue;
        };

        let events = match ipc_client::watch_events(stream, &watch_request) {
            Ok(events) => events,
            Err(err) => {
                eprintln!(
                    "richclip-labeld: failed to start watch stream: {err}; retrying in {}s",
                    backoff.as_secs()
                );
                thread::sleep(backoff);
                backoff = next_backoff(backoff);
                continue;
            }
        };

        backoff = INITIAL_RECONNECT_BACKOFF;

        for event in events {
            if let WatchEvent::ItemAdded { id, formats, .. } = event {
                let result: Result<()> = (|| {
                    let Some(source_mime) = choose_image_mime(&formats) else {
                        eprintln!("richclip-labeld: item {id} skipped; no supported image MIME");
                        return Ok(());
                    };

                    if !runtime.overwrite && formats.iter().any(|mime| mime == LABEL_MIME) {
                        eprintln!("richclip-labeld: item {id} skipped; label already present");
                        return Ok(());
                    }

                    let image_bytes = runtime
                        .store
                        .decode(id, source_mime)
                        .with_context(|| format!("failed to decode {source_mime} for item {id}"))?;
                    let response = runtime.client.label_image(LabelRequest {
                        mime_type: source_mime.to_string(),
                        image_bytes,
                        prompt: runtime.prompt.clone(),
                    })?;
                    let label = response.text.trim();
                    if label.is_empty() {
                        anyhow::bail!("model returned empty label text");
                    }

                    let mut stream =
                        ipc_client::try_connect(&runtime.socket_path).with_context(|| {
                            format!("daemon unavailable at {:?}", runtime.socket_path)
                        })?;
                    let response = ipc_client::send_request(
                        &mut stream,
                        &Request::UpdateItem(UpdateItemParams {
                            id,
                            set_formats: vec![(LABEL_MIME.to_string(), label.as_bytes().to_vec())],
                            remove_mimes: Vec::new(),
                        }),
                    )
                    .context("IPC update failed")?;
                    ensure_response_ok(response)
                        .with_context(|| format!("failed to write {LABEL_MIME} for item {id}"))?;

                    eprintln!("richclip-labeld: labelled item {id} from {source_mime}");
                    Ok(())
                })();

                if let Err(err) = result {
                    eprintln!("richclip-labeld: item {id} failed: {err:#}");
                }
            }
        }

        eprintln!(
            "richclip-labeld: watch stream disconnected; retrying in {}s",
            backoff.as_secs()
        );
        thread::sleep(backoff);
        backoff = next_backoff(backoff);
    }
}

fn ensure_response_ok(response: Response) -> Result<()> {
    if response.ok {
        Ok(())
    } else {
        let code = response.code.unwrap_or_else(|| "error".to_string());
        let error = response
            .error
            .unwrap_or_else(|| "unknown daemon error".to_string());
        anyhow::bail!("daemon rejected update ({code}): {error}");
    }
}

fn choose_image_mime(formats: &[String]) -> Option<&str> {
    IMAGE_MIME_PRIORITY
        .iter()
        .find(|candidate| formats.iter().any(|mime| mime == **candidate))
        .copied()
}

fn socket_path(cli_socket: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = cli_socket {
        Ok(path)
    } else if let Ok(path) = std::env::var("RICHCLIP_SOCKET") {
        Ok(PathBuf::from(path))
    } else {
        richclip::paths::default_socket_path().context("failed to resolve default socket path")
    }
}

fn data_dir(cli_data_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = cli_data_dir {
        Ok(path)
    } else if let Ok(path) = std::env::var("RICHCLIP_DATA_DIR") {
        Ok(PathBuf::from(path))
    } else {
        richclip::paths::default_data_dir().context("failed to resolve default data dir")
    }
}

fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RECONNECT_BACKOFF)
}
