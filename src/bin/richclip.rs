//! richclip — CLI frontend for the richclip clipboard history store.
//!
//! Commands: add, list, formats, decode, update, delete, inspect, watch, restore.
//! Global flag: `--data-dir <PATH>` (overrides RICHCLIP_DATA_DIR env var).
//!
//! ## Mutation routing
//!
//! When `richclipd` is running (detected by a successful connection to the IPC
//! socket), `add`, `update`, and `delete` are routed through the daemon so that
//! every mutation emits a `watch` event.  If no daemon is running they fall
//! back to direct DB access (Phase-1 behaviour).
//!
//! Reads (`list`, `formats`, `inspect`, `decode`) always go direct to the DB
//! (SQLite WAL mode allows concurrent readers).

use clap::{Args, Parser, Subcommand};
use richclip::ipc::{
    AddItemParams, Request, Response, UpdateItemParams, WatchEventsParams, client as ipc_client,
};
use richclip::{Error as LibError, Store};
use serde::Serialize;
use std::io::{Read as IoRead, Write as IoWrite};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use time::OffsetDateTime;
use uuid::Uuid;

// Thumbnail generation (store-aware).
use richclip::thumbnail::generate_item_thumbnail;

// ---------------------------------------------------------------------------
// CLI structure (clap derive)
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "richclip", about = "Richclip clipboard history manager CLI")]
struct Cli {
    /// Override data directory (else RICHCLIP_DATA_DIR env var, else XDG default).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add a new clipboard item.
    Add(AddArgs),
    /// List clipboard items.
    List(ListArgs),
    /// List formats (MIME types) for an item.
    Formats(FormatsArgs),
    /// Decode a format blob to stdout.
    Decode(DecodeArgs),
    /// Update an existing item's formats.
    Update(UpdateArgs),
    /// Delete an item, a single format, or items older than a duration.
    Delete(DeleteArgs),
    /// Inspect full item record.
    Inspect(InspectArgs),
    /// Watch live clipboard events (requires richclipd).
    Watch(WatchArgs),
    /// Restore an item as the active clipboard (requires richclipd).
    Restore(RestoreArgs),
    /// (Re)generate a thumbnail for an item; prints its path or nothing if not an image.
    Thumbnail(ThumbnailArgs),
}

// --- add ---

#[derive(Args)]
struct AddArgs {
    /// Output result as JSON.
    #[arg(long)]
    json: bool,

    /// Set a MIME format: MIME=SRC where SRC is @path or - (stdin).
    /// Repeatable; at least one required.
    #[arg(long = "set-mime", value_name = "MIME=SRC", required = true)]
    set_mime: Vec<String>,
}

// --- list ---

#[derive(Args)]
struct ListArgs {
    /// Output as JSON.
    #[arg(long)]
    json: bool,

    /// Maximum number of items to return.
    #[arg(long)]
    limit: Option<usize>,

    /// Filter to items that have this MIME type.
    #[arg(long)]
    mime: Option<String>,
}

// --- formats ---

#[derive(Args)]
struct FormatsArgs {
    /// Item ID.
    id: String,
}

// --- decode ---

#[derive(Args)]
struct DecodeArgs {
    /// Item ID.
    id: String,
    /// MIME type to decode.
    mime: String,
}

// --- update ---

#[derive(Args)]
struct UpdateArgs {
    /// Item ID.
    id: String,

    /// Output result as JSON.
    #[arg(long)]
    json: bool,

    /// Set (add or replace) a MIME format: MIME=SRC.
    #[arg(long = "set-mime", value_name = "MIME=SRC")]
    set_mime: Vec<String>,

    /// Remove a MIME format from the item.
    #[arg(long = "remove-mime", value_name = "MIME")]
    remove_mime: Vec<String>,
}

// --- delete ---

#[derive(Args)]
struct DeleteArgs {
    /// Item ID (omit when using --older-than).
    id: Option<String>,

    /// Delete only the given MIME format (requires id).
    #[arg(long)]
    mime: Option<String>,

    /// Delete all items older than this duration (e.g. "30d", "2h").
    #[arg(long, value_name = "DUR")]
    older_than: Option<String>,

    /// Output result as JSON.
    #[arg(long)]
    json: bool,
}

// --- inspect ---

#[derive(Args)]
struct InspectArgs {
    /// Item ID.
    id: String,

    /// Output as JSON (default; always JSON for this command).
    #[arg(long)]
    json: bool,
}

// --- watch ---

#[derive(Args)]
struct WatchArgs {
    /// Output raw JSON event lines (default: human-readable).
    #[arg(long)]
    json: bool,

    /// Only show events of this type: item-added | item-updated | item-deleted.
    #[arg(long, value_name = "EVENT")]
    event: Option<String>,

    /// Only show events for items that have this MIME type.
    #[arg(long, value_name = "MIME")]
    mime: Option<String>,
}

// --- restore ---

#[derive(Args)]
struct RestoreArgs {
    /// Item ID.
    id: String,
}

// --- thumbnail ---

#[derive(Args)]
struct ThumbnailArgs {
    /// Item ID.
    id: String,
}

// ---------------------------------------------------------------------------
// Output structs (owned, for JSON serialisation)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AddOutput {
    id: Uuid,
}

#[derive(Serialize)]
struct OkOutput {
    ok: bool,
}

#[derive(Serialize)]
struct DeletedOutput {
    deleted: u64,
}

#[derive(Serialize)]
struct ListFormatEntry {
    mime: String,
    size: u64,
}

#[derive(Serialize)]
struct ListItemEntry {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    label: Option<String>,
    thumbnail: Option<String>,
    formats: Vec<ListFormatEntry>,
}

#[derive(Serialize)]
struct InspectFormatEntry {
    mime: String,
    size: u64,
    blob_hash: String,
}

#[derive(Serialize)]
struct InspectOutput {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    formats: Vec<InspectFormatEntry>,
}

// ---------------------------------------------------------------------------
// Application error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum AppError {
    /// A library error (NotFound → code 2, others → code 1).
    Lib(LibError),
    /// A CLI-usage or argument error (always code 1).
    Usage(String),
}

impl AppError {
    fn exit_code(&self) -> i32 {
        match self {
            AppError::Lib(e) => e.code(),
            AppError::Usage(_) => 1,
        }
    }

    fn json_code(&self) -> &'static str {
        match self {
            AppError::Lib(e) => e.json_code(),
            AppError::Usage(_) => "error",
        }
    }

    fn message(&self) -> String {
        match self {
            AppError::Lib(e) => e.to_string(),
            AppError::Usage(s) => s.clone(),
        }
    }
}

impl From<LibError> for AppError {
    fn from(e: LibError) -> Self {
        AppError::Lib(e)
    }
}

// ---------------------------------------------------------------------------
// Store helper
// ---------------------------------------------------------------------------

fn open_store(data_dir: &Option<PathBuf>) -> Result<Store, AppError> {
    if let Some(p) = data_dir {
        Store::open(p).map_err(AppError::from)
    } else if let Ok(env_dir) = std::env::var("RICHCLIP_DATA_DIR") {
        Store::open(&PathBuf::from(env_dir)).map_err(AppError::from)
    } else {
        Store::open_default().map_err(AppError::from)
    }
}

// ---------------------------------------------------------------------------
// Cache dir helper
// ---------------------------------------------------------------------------

/// Resolve the thumbnail cache directory.
///
/// Resolution order: `RICHCLIP_CACHE_DIR` env var → `richclip::paths::default_cache_dir()`.
fn cache_dir() -> Result<PathBuf, AppError> {
    if let Ok(env_dir) = std::env::var("RICHCLIP_CACHE_DIR") {
        Ok(PathBuf::from(env_dir))
    } else {
        richclip::paths::default_cache_dir().map_err(AppError::from)
    }
}

// ---------------------------------------------------------------------------
// Daemon socket helper
// ---------------------------------------------------------------------------

/// Attempt to connect to the daemon. Returns `Some(stream)` if a daemon is up.
fn daemon_socket() -> Option<UnixStream> {
    let path = if let Ok(v) = std::env::var("RICHCLIP_SOCKET") {
        PathBuf::from(v)
    } else {
        richclip::paths::default_socket_path().ok()?
    };
    ipc_client::try_connect(&path)
}

/// Map an IPC `Response` to a CLI `Result`.  Non-ok responses become `AppError`.
fn response_to_result(resp: Response) -> Result<Response, AppError> {
    if resp.ok {
        Ok(resp)
    } else {
        let code = resp.code.as_deref().unwrap_or("error");
        // Map "not_found" to NotFound so the CLI exits 2.
        if code == "not_found" {
            Err(AppError::Lib(LibError::NotFound))
        } else {
            Err(AppError::Usage(
                resp.error.unwrap_or_else(|| "unknown error".into()),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// --set-mime parsing
// ---------------------------------------------------------------------------

/// A single parsed --set-mime entry.
enum MimeSrc {
    File(String, PathBuf), // mime, path
    Stdin(String),         // mime
}

/// Parse all --set-mime strings. Enforces at most one stdin source.
fn parse_set_mime(args: &[String]) -> Result<Vec<MimeSrc>, AppError> {
    let mut entries = Vec::with_capacity(args.len());
    let mut stdin_count = 0usize;

    for s in args {
        // Split on the FIRST '=' only.
        let eq = s.find('=').ok_or_else(|| {
            AppError::Usage(format!(
                "--set-mime value must be MIME=SRC (no '=' found in {:?})",
                s
            ))
        })?;
        let mime = &s[..eq];
        let src = &s[eq + 1..];

        if mime.is_empty() {
            return Err(AppError::Usage(format!(
                "--set-mime value has empty MIME type: {:?}",
                s
            )));
        }

        if src == "-" {
            stdin_count += 1;
            if stdin_count > 1 {
                return Err(AppError::Usage(
                    "at most one --set-mime source may be '-' (stdin can only be read once)".into(),
                ));
            }
            entries.push(MimeSrc::Stdin(mime.to_string()));
        } else if let Some(path) = src.strip_prefix('@') {
            entries.push(MimeSrc::File(mime.to_string(), PathBuf::from(path)));
        } else {
            return Err(AppError::Usage(format!(
                "--set-mime source must be '-' or '@path', got {:?}",
                src
            )));
        }
    }

    Ok(entries)
}

/// Resolve parsed MimeSrc entries into (mime, bytes) pairs.
fn resolve_mime_srcs(srcs: Vec<MimeSrc>) -> Result<Vec<(String, Vec<u8>)>, AppError> {
    // Read stdin once if needed.
    let stdin_bytes: Option<Vec<u8>> = if srcs.iter().any(|s| matches!(s, MimeSrc::Stdin(_))) {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| AppError::Usage(format!("failed to read stdin: {e}")))?;
        Some(buf)
    } else {
        None
    };

    let mut result = Vec::with_capacity(srcs.len());
    for src in srcs {
        match src {
            MimeSrc::File(mime, path) => {
                let bytes = std::fs::read(&path)
                    .map_err(|e| AppError::Usage(format!("failed to read {:?}: {e}", path)))?;
                result.push((mime, bytes));
            }
            MimeSrc::Stdin(mime) => {
                result.push((mime, stdin_bytes.clone().unwrap()));
            }
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Derived label helper
// ---------------------------------------------------------------------------

fn derive_label(store: &Store, id: Uuid) -> Option<String> {
    // Try application/x-richclip-label first.
    if let Ok(bytes) = store.decode(id, "application/x-richclip-label")
        && let Ok(s) = std::str::from_utf8(&bytes)
    {
        let trimmed = s.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    // Fall back to text/plain snippet.
    if let Ok(bytes) = store.decode(id, "text/plain")
        && let Ok(s) = std::str::from_utf8(&bytes)
    {
        let first_line = s.lines().next().unwrap_or("").trim();
        if !first_line.is_empty() {
            // Truncate by char count (not byte index) to avoid panicking on
            // multibyte UTF-8 boundaries.
            let owned_snippet;
            let snippet = if first_line.chars().count() > 60 {
                owned_snippet = first_line.chars().take(60).collect::<String>();
                owned_snippet.as_str()
            } else {
                first_line
            };
            return Some(snippet.to_string());
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Relative time helper
// ---------------------------------------------------------------------------

fn relative_time(ts: OffsetDateTime) -> String {
    let now = OffsetDateTime::now_utc();
    let delta = now - ts;
    // humantime works with std::time::Duration (non-negative).
    let secs = delta.whole_seconds().max(0) as u64;
    let dur = std::time::Duration::from_secs(secs);
    if dur.as_secs() < 60 {
        return format!("{}s ago", dur.as_secs());
    }
    let formatted = humantime::format_duration(dur).to_string();
    // Take just the first token (e.g. "2m" from "2m 30s").
    let first = formatted.split_whitespace().next().unwrap_or("?");
    format!("{first} ago")
}

// ---------------------------------------------------------------------------
// UUID parsing helper
// ---------------------------------------------------------------------------

fn parse_uuid(s: &str) -> Result<Uuid, AppError> {
    s.parse::<Uuid>()
        .map_err(|_| AppError::Usage(format!("invalid id: {:?}", s)))
}

// ---------------------------------------------------------------------------
// Error output helpers
// ---------------------------------------------------------------------------

fn print_error(err: &AppError, json: bool) {
    let msg = err.message();
    if json {
        let obj = serde_json::json!({
            "error": msg,
            "code": err.json_code(),
        });
        eprintln!("{}", obj);
    } else {
        eprintln!("error: {msg}");
    }
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

fn cmd_add(cli_data_dir: &Option<PathBuf>, args: AddArgs) -> Result<(), AppError> {
    let srcs = parse_set_mime(&args.set_mime)?;
    let formats = resolve_mime_srcs(srcs)?;

    // Try routing through the daemon first.
    if let Some(mut stream) = daemon_socket() {
        let req = Request::AddItem(AddItemParams {
            formats: formats.clone(),
        });
        let resp = ipc_client::send_request(&mut stream, &req)
            .map_err(|e| AppError::Usage(format!("IPC error: {e}")))?;
        let resp = response_to_result(resp)?;

        let id_val = resp
            .data
            .as_ref()
            .and_then(|d| d["id"].as_str())
            .unwrap_or("");
        if args.json {
            println!("{}", serde_json::json!({"id": id_val}));
        } else {
            println!("{id_val}");
        }
        return Ok(());
    }

    // No daemon: direct DB write (Phase-1 fallback).
    let mut store = open_store(cli_data_dir)?;
    let id = store.add_item(&formats)?;

    // Best-effort thumbnail generation.  Print the id first so output is
    // correct even on thumbnail failure; then attempt generation and warn on
    // error (never fail the command).
    if args.json {
        println!("{}", serde_json::to_string(&AddOutput { id }).unwrap());
    } else {
        println!("{id}");
    }

    if let Ok(cd) = cache_dir() {
        if let Err(e) = generate_item_thumbnail(&store, &cd, id) {
            eprintln!("warning: thumbnail generation failed: {e}");
        }
    } else {
        eprintln!("warning: could not resolve cache dir; thumbnail skipped");
    }

    Ok(())
}

fn cmd_list(cli_data_dir: &Option<PathBuf>, args: ListArgs) -> Result<(), AppError> {
    // Reads always go direct to the DB.
    let store = open_store(cli_data_dir)?;
    let items = store.list_items(args.limit, args.mime.as_deref())?;

    if args.json {
        // Resolve cache dir once; if unavailable, all thumbnails will be None.
        let cd = cache_dir().ok();

        let mut output = Vec::with_capacity(items.len());
        for iwf in &items {
            let label = derive_label(&store, iwf.item.id);
            let formats = iwf
                .formats
                .iter()
                .map(|f| ListFormatEntry {
                    mime: f.mime.clone(),
                    size: f.size,
                })
                .collect();
            // Report existing thumbnail file; never generate here.
            let thumbnail = cd.as_ref().and_then(|c| {
                let p = richclip::paths::thumb_path(c, iwf.item.id);
                if p.exists() {
                    Some(p.to_string_lossy().into_owned())
                } else {
                    None
                }
            });
            output.push(ListItemEntry {
                id: iwf.item.id,
                created_at: iwf.item.created_at,
                label,
                thumbnail,
                formats,
            });
        }
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        for iwf in &items {
            let label = derive_label(&store, iwf.item.id).unwrap_or_default();
            let rel = relative_time(iwf.item.created_at);
            let mimes: Vec<&str> = iwf.formats.iter().map(|f| f.mime.as_str()).collect();
            let mimes_str = mimes.join(", ");
            println!("{}\t{}\t{}\t{}", iwf.item.id, rel, mimes_str, label);
        }
    }
    Ok(())
}

fn cmd_formats(cli_data_dir: &Option<PathBuf>, args: FormatsArgs) -> Result<(), AppError> {
    let id = parse_uuid(&args.id)?;
    let store = open_store(cli_data_dir)?;
    let formats = store.formats(id)?;

    for f in &formats {
        println!("{:<40} {:>10}", f.mime, f.size);
    }
    Ok(())
}

fn cmd_decode(cli_data_dir: &Option<PathBuf>, args: DecodeArgs) -> Result<(), AppError> {
    let id = parse_uuid(&args.id)?;
    let store = open_store(cli_data_dir)?;
    let bytes = store.decode(id, &args.mime)?;

    std::io::stdout()
        .write_all(&bytes)
        .map_err(|e| AppError::Usage(format!("failed to write to stdout: {e}")))?;
    std::io::stdout()
        .flush()
        .map_err(|e| AppError::Usage(format!("failed to flush stdout: {e}")))?;
    Ok(())
}

fn cmd_update(cli_data_dir: &Option<PathBuf>, args: UpdateArgs) -> Result<(), AppError> {
    if args.set_mime.is_empty() && args.remove_mime.is_empty() {
        return Err(AppError::Usage(
            "update requires at least one of --set-mime or --remove-mime".into(),
        ));
    }

    let id = parse_uuid(&args.id)?;
    let srcs = parse_set_mime(&args.set_mime)?;
    let resolved = resolve_mime_srcs(srcs)?;

    // Try routing through the daemon first.
    if let Some(mut stream) = daemon_socket() {
        let req = Request::UpdateItem(UpdateItemParams {
            id,
            set_formats: resolved.clone(),
            remove_mimes: args.remove_mime.clone(),
        });
        let resp = ipc_client::send_request(&mut stream, &req)
            .map_err(|e| AppError::Usage(format!("IPC error: {e}")))?;
        response_to_result(resp)?;

        if args.json {
            println!("{}", serde_json::to_string(&OkOutput { ok: true }).unwrap());
        }
        return Ok(());
    }

    // No daemon: direct DB write.
    let mut store = open_store(cli_data_dir)?;

    // Apply removes first, then sets.
    for mime in &args.remove_mime {
        store.remove_format(id, mime)?;
    }
    for (mime, bytes) in &resolved {
        store.set_format(id, mime, bytes)?;
    }

    if args.json {
        println!("{}", serde_json::to_string(&OkOutput { ok: true }).unwrap());
    }
    Ok(())
}

fn cmd_delete(cli_data_dir: &Option<PathBuf>, args: DeleteArgs) -> Result<(), AppError> {
    // Validate argument combinations.
    match (&args.id, &args.older_than) {
        (Some(_), Some(_)) => {
            return Err(AppError::Usage(
                "cannot use both <id> and --older-than".into(),
            ));
        }
        (None, None) => {
            return Err(AppError::Usage(
                "must specify either <id> or --older-than".into(),
            ));
        }
        _ => {}
    }

    if args.mime.is_some() && args.id.is_none() {
        return Err(AppError::Usage("--mime requires an item <id>".into()));
    }

    if args.mime.is_some() && args.older_than.is_some() {
        return Err(AppError::Usage(
            "--mime cannot be used with --older-than".into(),
        ));
    }

    // --older-than: always goes direct to DB (bulk prune doesn't need IPC).
    if let Some(dur_str) = &args.older_than {
        let dur = humantime::parse_duration(dur_str)
            .map_err(|e| AppError::Usage(format!("invalid duration {:?}: {e}", dur_str)))?;
        let now = OffsetDateTime::now_utc();
        let cutoff = now - time::Duration::new(dur.as_secs() as i64, dur.subsec_nanos() as i32);
        let mut store = open_store(cli_data_dir)?;
        let count = store.delete_older_than(cutoff)?;

        if args.json {
            println!(
                "{}",
                serde_json::to_string(&DeletedOutput { deleted: count }).unwrap()
            );
        } else {
            println!("deleted {count} items");
        }
        return Ok(());
    }

    // id is Some at this point.
    let id = parse_uuid(args.id.as_deref().unwrap())?;

    // MIME removal: route through daemon (or direct) using UpdateItem/DeleteItem.
    if let Some(ref mime) = args.mime {
        // --mime: route via UpdateItem(remove_mimes=[mime]) or direct.
        if let Some(mut stream) = daemon_socket() {
            let req = Request::UpdateItem(UpdateItemParams {
                id,
                set_formats: vec![],
                remove_mimes: vec![mime.clone()],
            });
            let resp = ipc_client::send_request(&mut stream, &req)
                .map_err(|e| AppError::Usage(format!("IPC error: {e}")))?;
            response_to_result(resp)?;
            if args.json {
                println!("{}", serde_json::to_string(&OkOutput { ok: true }).unwrap());
            }
            return Ok(());
        }
        let mut store = open_store(cli_data_dir)?;
        store.remove_format(id, mime)?;
        if args.json {
            println!("{}", serde_json::to_string(&OkOutput { ok: true }).unwrap());
        }
        return Ok(());
    }

    // Full item delete: route through daemon.
    if let Some(mut stream) = daemon_socket() {
        let req = Request::DeleteItem { id };
        let resp = ipc_client::send_request(&mut stream, &req)
            .map_err(|e| AppError::Usage(format!("IPC error: {e}")))?;
        response_to_result(resp)?;
        // Best-effort thumbnail cleanup (daemon also cleans up, but this covers
        // the case where the cache dir is local to the CLI process).
        if let Ok(cd) = cache_dir() {
            let _ = std::fs::remove_file(richclip::paths::thumb_path(&cd, id));
        }
        if args.json {
            println!("{}", serde_json::to_string(&OkOutput { ok: true }).unwrap());
        }
        return Ok(());
    }

    let mut store = open_store(cli_data_dir)?;
    store.delete_item(id)?;
    // Best-effort thumbnail cleanup.
    if let Ok(cd) = cache_dir() {
        let _ = std::fs::remove_file(richclip::paths::thumb_path(&cd, id));
    }
    if args.json {
        println!("{}", serde_json::to_string(&OkOutput { ok: true }).unwrap());
    }
    Ok(())
}

fn cmd_inspect(cli_data_dir: &Option<PathBuf>, args: InspectArgs) -> Result<(), AppError> {
    let id = parse_uuid(&args.id)?;
    let store = open_store(cli_data_dir)?;
    let iwf = store.get_item(id)?;

    let out = InspectOutput {
        id: iwf.item.id,
        created_at: iwf.item.created_at,
        updated_at: iwf.item.updated_at,
        formats: iwf
            .formats
            .iter()
            .map(|f| InspectFormatEntry {
                mime: f.mime.clone(),
                size: f.size,
                blob_hash: f.blob_hash.clone(),
            })
            .collect(),
    };
    println!("{}", serde_json::to_string(&out).unwrap());
    Ok(())
}

fn cmd_watch(args: WatchArgs) -> Result<(), AppError> {
    let stream = daemon_socket().ok_or_else(|| {
        AppError::Usage("richclipd is not running; start the daemon first".into())
    })?;

    let req = Request::WatchEvents(WatchEventsParams {
        event_filter: args.event.clone(),
        mime_filter: args.mime.clone(),
    });

    let iter = ipc_client::watch_events(stream, &req)
        .map_err(|e| AppError::Usage(format!("IPC error: {e}")))?;

    for event in iter {
        if args.json {
            // Print the raw JSON line.
            let line = serde_json::to_string(&event)
                .unwrap_or_else(|_| String::from("{\"error\":\"serialize\"}"));
            println!("{line}");
        } else {
            // Human-readable short line.
            use richclip::ipc::WatchEvent;
            match &event {
                WatchEvent::ItemAdded { id, formats, .. } => {
                    println!("item-added {} {}", id, formats.join(", "));
                }
                WatchEvent::ItemUpdated { id, changed } => {
                    println!("item-updated {} changed: {}", id, changed.join(", "));
                }
                WatchEvent::ItemDeleted { id } => {
                    println!("item-deleted {}", id);
                }
            }
        }
        // Flush so piped consumers see events immediately.
        let _ = std::io::stdout().flush();
    }
    Ok(())
}

fn cmd_restore(args: RestoreArgs) -> Result<(), AppError> {
    let id = parse_uuid(&args.id)?;
    let mut stream = daemon_socket().ok_or_else(|| {
        AppError::Usage("richclipd is not running; start the daemon first".into())
    })?;

    let req = Request::RestoreItem { id };
    let resp = ipc_client::send_request(&mut stream, &req)
        .map_err(|e| AppError::Usage(format!("IPC error: {e}")))?;
    response_to_result(resp)?;
    Ok(())
}

fn cmd_thumbnail(cli_data_dir: &Option<PathBuf>, args: ThumbnailArgs) -> Result<(), AppError> {
    let id = parse_uuid(&args.id)?;
    let store = open_store(cli_data_dir)?;
    let cd = cache_dir()?;
    // not an image item — print nothing, exit 0
    if let Some(path) = generate_item_thumbnail(&store, &cd, id)? {
        println!("{}", path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn run() -> Result<(), AppError> {
    let cli = Cli::parse();

    match cli.command {
        Command::Add(args) => cmd_add(&cli.data_dir, args),
        Command::List(args) => cmd_list(&cli.data_dir, args),
        Command::Formats(args) => cmd_formats(&cli.data_dir, args),
        Command::Decode(args) => cmd_decode(&cli.data_dir, args),
        Command::Update(args) => cmd_update(&cli.data_dir, args),
        Command::Delete(args) => cmd_delete(&cli.data_dir, args),
        Command::Inspect(args) => cmd_inspect(&cli.data_dir, args),
        Command::Watch(args) => cmd_watch(args),
        Command::Restore(args) => cmd_restore(args),
        Command::Thumbnail(args) => cmd_thumbnail(&cli.data_dir, args),
    }
}

fn main() {
    // Determine whether --json is in effect for error reporting.
    // We do a lightweight scan of argv rather than re-parsing the full CLI.
    let json_mode = std::env::args().any(|a| a == "--json");

    match run() {
        Ok(()) => {}
        Err(err) => {
            print_error(&err, json_mode);
            std::process::exit(err.exit_code());
        }
    }
}
