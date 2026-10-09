use std::path::{Path, PathBuf};

use axum::Extension;
use axum::Json;
use axum::body::Body;
use axum::extract::{Multipart, Query, State};
use axum::response::Response;
use futures::Stream;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;

use super::super::core::AppState;
use crate::auth::KeyKind;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::runtime::sandbox::SandboxProfile;
use crate::workload_fs::{Anchor, DEFAULT_JOB_TIMEOUT};
use crate::workspace::{self, FileMetadata, FileOpen, FileRead, PathIntent, WorkspaceListing};

// === CONSTANTS ===

/// Largest read a download body makes per chunk, which bounds the file data one download holds.
const DOWNLOAD_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct WorkspaceMetadataResponse {
    root: String,
    uploads_path: String,
    default_shell: String,
    max_file_bytes: u64,
}

pub(crate) async fn workspace_metadata_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<WorkspaceMetadataResponse>, StackError> {
    let workspace = &state.config.workspace;
    let uploads_path = workspace_relative_string(&workspace.root, &workspace.uploads);
    Ok(ApiSuccess::new(WorkspaceMetadataResponse {
        root: workspace.root.clone(),
        uploads_path,
        default_shell: workspace.default_shell.clone(),
        max_file_bytes: workspace.max_file_bytes,
    }))
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct FilesPathParams {
    path: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct FilesListResponse {
    path: String,
    entries: Vec<FilesListEntry>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct FilesListEntry {
    name: String,
    #[schemars(extend("enum" = ["file", "directory", "symlink", "other"]))]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    modified: String,
}

pub(crate) async fn files_list_handler(
    State(state): State<AppState>,
    Query(params): Query<FilesPathParams>,
) -> std::result::Result<ApiSuccess<FilesListResponse>, StackError> {
    let profile = SandboxProfile::resolve(&state.config.workspace.sandbox)?;
    let listing: WorkspaceListing = run_in_workspace(
        &profile,
        &state.config.workspace.root,
        &params.path,
        PathIntent::ReadExisting,
        workspace::list_directory,
    )
    .await?;
    Ok(ApiSuccess::new(FilesListResponse {
        path: params.path,
        entries: listing
            .entries
            .into_iter()
            .map(|entry| FilesListEntry {
                name: entry.name,
                kind: entry_kind_to_str(entry.kind).to_owned(),
                size: entry.size,
                modified: entry.modified.to_rfc3339(),
            })
            .collect(),
    }))
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct FilesContentResponse {
    path: String,
    #[schemars(extend("enum" = ["utf8", "base64"]))]
    encoding: String,
    content: String,
    size: u64,
    modified: String,
}

pub(crate) async fn files_content_get_handler(
    State(state): State<AppState>,
    Query(params): Query<FilesPathParams>,
) -> std::result::Result<ApiSuccess<FilesContentResponse>, StackError> {
    let read = read_workspace_file(&state, &params.path).await?;
    let (encoding, content) = encode_file_content(&read.content);
    Ok(ApiSuccess::new(FilesContentResponse {
        path: params.path,
        encoding: encoding.to_owned(),
        content,
        size: read.size,
        modified: read.modified.to_rfc3339(),
    }))
}

pub(crate) async fn files_download_handler(
    State(state): State<AppState>,
    Query(params): Query<FilesPathParams>,
) -> std::result::Result<Response, StackError> {
    let opened = open_workspace_file(&state, &params.path).await?;
    let filename = Path::new(&params.path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_owned());
    let disposition = format!(
        "attachment; filename=\"{}\"",
        sanitize_disposition_filename(&filename)
    );
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/octet-stream")
        .header(http::header::CONTENT_LENGTH, opened.size)
        .header(http::header::CONTENT_DISPOSITION, disposition)
        .body(Body::from_stream(download_stream(
            opened,
            state.shutdown.clone(),
            params.path.clone(),
        )))
        .map_err(|_| StackError::WorkspaceIo {
            requested: params.path.clone(),
            source: std::io::Error::other("failed to build download response"),
        })?;
    Ok(response)
}

/// Read cursor over an opened download, owned by the body stream.
struct DownloadCursor {
    file: tokio::fs::File,
    remaining: u64,
    shutdown: CancellationToken,
    requested: String,
}

/// Stream `opened` in chunks of at most [`DOWNLOAD_CHUNK_BYTES`], reading only when polled so a
/// slow reader slows the reads. A stream error aborts the connection, which keeps the body from
/// falling short of the declared Content-Length when the file shrinks, and ends the body when
/// shutdown starts.
fn download_stream(
    opened: FileOpen,
    shutdown: CancellationToken,
    requested: String,
) -> impl Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static {
    let cursor = DownloadCursor {
        file: tokio::fs::File::from_std(opened.file),
        remaining: opened.size,
        shutdown,
        requested,
    };
    futures::stream::try_unfold(cursor, |mut cursor| async move {
        match cursor.next_chunk().await {
            Ok(Some(chunk)) => Ok(Some((chunk, cursor))),
            Ok(None) => Ok(None),
            Err(error) => {
                tracing::warn!(path = %cursor.requested, %error, "workspace download ended early");
                Err(error)
            }
        }
    })
}

impl DownloadCursor {
    async fn next_chunk(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        // A file that grew is sent up to its declared length, which still matches Content-Length.
        if self.remaining == 0 {
            return Ok(None);
        }
        let length = usize::try_from(self.remaining).map_or(DOWNLOAD_CHUNK_BYTES, |remaining| {
            remaining.min(DOWNLOAD_CHUNK_BYTES)
        });
        let mut chunk = vec![0u8; length];
        let read = self.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "file shrank below its declared length during download",
            ));
        }
        chunk.truncate(read);
        self.remaining -= read as u64;
        Ok(Some(chunk))
    }

    async fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        tokio::select! {
            biased;
            () = self.shutdown.cancelled() => Err(std::io::Error::other("server is shutting down")),
            read = self.file.read(buffer) => read,
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct FilesContentPutBody {
    path: String,
    encoding: String,
    content: String,
}

pub(crate) async fn files_content_put_handler(
    State(state): State<AppState>,
    Extension(kind): Extension<KeyKind>,
    Json(body): Json<FilesContentPutBody>,
) -> std::result::Result<ApiSuccess<FileMutationResponse>, StackError> {
    let bytes = decode_request_content(&body.encoding, &body.content)?;
    let max_bytes = state.config.workspace.max_file_bytes;
    if bytes.len() as u64 > max_bytes {
        return Err(StackError::WorkspaceTooLarge { limit: max_bytes });
    }
    let metadata = write_workspace_file(&state, &body.path, bytes).await?;

    publish_workspace_mutation(
        &state,
        kind,
        "workspace.write",
        &body.path,
        Some(metadata.size),
    )
    .await?;

    Ok(ApiSuccess::new(FileMutationResponse {
        path: body.path,
        size: metadata.size,
        modified: metadata.modified.to_rfc3339(),
    }))
}

pub(crate) async fn files_upload_handler(
    State(state): State<AppState>,
    Extension(kind): Extension<KeyKind>,
    mut multipart: Multipart,
) -> std::result::Result<ApiSuccess<FileUploadResponse>, StackError> {
    let mut path: Option<String> = None;
    let mut filename: Option<String> = None;
    let mut content: Option<Vec<u8>> = None;

    while let Some(field) = multipart.next_field().await.map_err(|err| {
        tracing::debug!(error = %err, "rejecting malformed multipart upload");
        StackError::WorkspaceUploadInvalid {
            reason: "multipart body is malformed",
        }
    })? {
        match field.name() {
            Some("path") => {
                path =
                    Some(
                        field
                            .text()
                            .await
                            .map_err(|_| StackError::WorkspaceUploadInvalid {
                                reason: "multipart `path` field could not be read as text",
                            })?,
                    );
            }
            Some("file") => {
                filename = field.file_name().map(|s| s.to_owned());
                // Stream chunks rather than buffering the whole part:
                // `api.max_request_bytes` may exceed
                // `workspace.max_file_bytes`, and accumulation must stop the
                // moment the per-file limit is crossed.
                let max_bytes = state.config.workspace.max_file_bytes;
                let mut buffer: Vec<u8> = Vec::new();
                let mut field = field;
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if (buffer.len() as u64).saturating_add(chunk.len() as u64) > max_bytes
                            {
                                return Err(StackError::WorkspaceTooLarge { limit: max_bytes });
                            }
                            buffer.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(_) => {
                            return Err(StackError::WorkspaceUploadInvalid {
                                reason: "multipart `file` field could not be read",
                            });
                        }
                    }
                }
                content = Some(buffer);
            }
            _ => {}
        }
    }

    let path = path.ok_or(StackError::WorkspaceUploadInvalid {
        reason: "multipart upload is missing the required `path` field",
    })?;
    let content = content.ok_or(StackError::WorkspaceUploadInvalid {
        reason: "multipart upload is missing the required `file` field",
    })?;
    let filename = filename.unwrap_or_default();

    let max_bytes = state.config.workspace.max_file_bytes;
    if content.len() as u64 > max_bytes {
        return Err(StackError::WorkspaceTooLarge { limit: max_bytes });
    }

    // Walk from `workspace.root`, never `workspace.uploads`, even though the
    // request path is uploads-relative: anchoring at `uploads` would trust
    // whatever `uploads` itself resolves to, symlink included.
    if Path::new(&path).is_absolute() {
        return Err(StackError::WorkspacePathInvalid {
            reason: "upload `path` must be relative to workspace.uploads".to_owned(),
            requested: path,
        });
    }
    let workspace_relative_path = join_workspace_relative(
        &state.config.workspace.root,
        &state.config.workspace.uploads,
        &path,
    );
    let metadata = write_workspace_file(&state, &workspace_relative_path, content).await?;

    publish_workspace_mutation(
        &state,
        kind,
        "workspace.upload",
        &workspace_relative_path,
        Some(metadata.size),
    )
    .await?;

    Ok(ApiSuccess::new(FileUploadResponse {
        path: workspace_relative_path,
        filename,
        size: metadata.size,
        modified: metadata.modified.to_rfc3339(),
    }))
}

pub(crate) async fn files_delete_handler(
    State(state): State<AppState>,
    Extension(kind): Extension<KeyKind>,
    Query(params): Query<FilesPathParams>,
) -> std::result::Result<ApiSuccess<FileDeleteResponse>, StackError> {
    let profile = SandboxProfile::resolve(&state.config.workspace.sandbox)?;
    run_in_workspace(
        &profile,
        &state.config.workspace.root,
        &params.path,
        PathIntent::WriteOrCreate,
        workspace::delete_file,
    )
    .await?;

    publish_workspace_mutation(&state, kind, "workspace.delete", &params.path, None).await?;

    Ok(ApiSuccess::new(FileDeleteResponse {
        path: params.path,
        deleted: true,
    }))
}

fn decode_request_content(
    encoding: &str,
    content: &str,
) -> std::result::Result<Vec<u8>, StackError> {
    match encoding {
        "utf8" => Ok(content.as_bytes().to_vec()),
        "base64" => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(content)
                .map_err(|_| StackError::WorkspaceEncodingInvalid {
                    reason: "content is not valid base64",
                })
        }
        _ => Err(StackError::WorkspaceEncodingInvalid {
            reason: "encoding must be `utf8` or `base64`",
        }),
    }
}

/// Compose the workspace-relative path for an upload destination. The upload
/// request's `path` is interpreted relative to `workspace.uploads`; this helper
/// joins `uploads`'s workspace-relative form with the request path so callers
/// can read the file back via the read routes.
fn join_workspace_relative(workspace_root: &str, uploads_root: &str, request_path: &str) -> String {
    let uploads_rel = workspace_relative_string(workspace_root, uploads_root);
    let trimmed = request_path.trim_start_matches('/');
    if uploads_rel.is_empty() {
        trimmed.to_owned()
    } else if trimmed.is_empty() {
        uploads_rel
    } else {
        format!("{uploads_rel}/{trimmed}")
    }
}

async fn publish_workspace_mutation(
    state: &AppState,
    caller: KeyKind,
    event_kind: &str,
    path: &str,
    size: Option<u64>,
) -> std::result::Result<(), StackError> {
    let mut data = serde_json::json!({ "path": path });
    if let Some(size) = size
        && let Some(obj) = data.as_object_mut()
    {
        obj.insert(
            "size".to_owned(),
            serde_json::Value::Number(serde_json::Number::from(size)),
        );
    }
    let payload_json = serde_json::to_string(&data).map_err(|_| StackError::WorkspaceIo {
        requested: path.to_owned(),
        source: std::io::Error::other("failed to serialize workspace event payload"),
    })?;
    let event = {
        let store = state.state.lock().await;
        // `message` stays empty so user paths never reach the text column;
        // `logs/events` is session-tier-readable.
        store.append_event_with_source(
            "info",
            event_kind,
            AppState::event_source_for(Some(caller)),
            "",
            &payload_json,
        )?
    };
    state.event_hub.publish_workspace_event(&event, data);
    Ok(())
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct FileMutationResponse {
    path: String,
    size: u64,
    modified: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct FileUploadResponse {
    path: String,
    filename: String,
    size: u64,
    modified: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct FileDeleteResponse {
    path: String,
    deleted: bool,
}

fn entry_kind_to_str(kind: workspace::EntryKind) -> &'static str {
    match kind {
        workspace::EntryKind::File => "file",
        workspace::EntryKind::Directory => "directory",
        workspace::EntryKind::Symlink => "symlink",
        workspace::EntryKind::Other => "other",
    }
}

fn encode_file_content(bytes: &[u8]) -> (&'static str, String) {
    match std::str::from_utf8(bytes) {
        Ok(text) => ("utf8", text.to_owned()),
        Err(_) => {
            use base64::Engine as _;
            (
                "base64",
                base64::engine::general_purpose::STANDARD.encode(bytes),
            )
        }
    }
}

/// `workspace.root` and `workspace.uploads` are both absolute paths in
/// config. Most callers want the uploads path expressed as workspace-relative
/// so they can use it directly with `/v1/files*` routes.
fn workspace_relative_string(root: &str, absolute: &str) -> String {
    let root = std::path::Path::new(root);
    let absolute = std::path::Path::new(absolute);
    match absolute.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => absolute.display().to_string(),
    }
}

/// `Content-Disposition` filename values are quoted strings; backslash and
/// double-quote must be escaped, and bare control chars are not allowed.
/// Non-ASCII characters are dropped here to stay inside the simple
/// `filename="..."` form. Clients that need exact non-ASCII filenames should
/// rely on the response body, not the header.
fn sanitize_disposition_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_ascii() && !c.is_control() => out.push(c),
            _ => out.push('_'),
        }
    }
    out
}

/// Validate `requested` and run `job` on it under `profile`'s executor, walking from an anchor on
/// `root` opened inside the job so a workload executor opens it with the workload's own
/// credentials.
async fn run_in_workspace<T: Send + 'static>(
    profile: &SandboxProfile,
    root: &str,
    requested: &str,
    intent: PathIntent,
    job: impl FnOnce(&Anchor, &Path, &str) -> crate::error::Result<T> + Send + 'static,
) -> std::result::Result<T, StackError> {
    let relative = workspace::workspace_relative_path(requested, intent)?;
    let root = PathBuf::from(root);
    let requested = requested.to_owned();
    let links = profile.link_policy(true);
    profile
        .executor()
        .run_async(DEFAULT_JOB_TIMEOUT, move || {
            let anchor = workspace::open_root(&root, &requested, links)?;
            job(&anchor, &relative, &requested)
        })
        .await
}

/// Reads run with the workload identity's credentials when one is declared: the root's own path
/// may sit in a workload-writable directory, and a swapped root must expose nothing the workload
/// could not read itself.
async fn read_workspace_file(
    state: &AppState,
    requested: &str,
) -> std::result::Result<FileRead, StackError> {
    let max_bytes = state.config.workspace.max_file_bytes;
    let profile = SandboxProfile::resolve(&state.config.workspace.sandbox)?;
    run_in_workspace(
        &profile,
        &state.config.workspace.root,
        requested,
        PathIntent::ReadExisting,
        move |anchor, relative, requested| {
            workspace::read_file(anchor, relative, requested, max_bytes)
        },
    )
    .await
}

/// Only the open runs as a workspace job, under the same credentials and timeout as a read. Reads
/// through the opened handle need no credentials, so the body streams with no job deadline.
async fn open_workspace_file(
    state: &AppState,
    requested: &str,
) -> std::result::Result<FileOpen, StackError> {
    let profile = SandboxProfile::resolve(&state.config.workspace.sandbox)?;
    run_in_workspace(
        &profile,
        &state.config.workspace.root,
        requested,
        PathIntent::ReadExisting,
        workspace::open_file,
    )
    .await
}

/// Writes run with the workload identity's credentials when one is declared,
/// so the file lands owned by the workload.
async fn write_workspace_file(
    state: &AppState,
    requested: &str,
    content: Vec<u8>,
) -> std::result::Result<FileMetadata, StackError> {
    let profile = SandboxProfile::resolve(&state.config.workspace.sandbox)?;
    let options = workspace::workload_write_options(&profile);
    run_in_workspace(
        &profile,
        &state.config.workspace.root,
        requested,
        PathIntent::WriteOrCreate,
        move |anchor, relative, requested| {
            workspace::write_file(anchor, relative, requested, &content, &options)
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use std::io::Write as _;
    use std::pin::pin;

    const FILE_NAME: &str = "data.bin";
    const TRAILING_BYTES: usize = 17;

    fn patterned(length: usize) -> Vec<u8> {
        (0..length).map(|index| (index % 251) as u8).collect()
    }

    fn workspace_with(content: &[u8]) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(FILE_NAME), content).expect("write");
        root
    }

    async fn open(root: &tempfile::TempDir) -> FileOpen {
        run_in_workspace(
            &SandboxProfile::default(),
            &root.path().to_string_lossy(),
            FILE_NAME,
            PathIntent::ReadExisting,
            workspace::open_file,
        )
        .await
        .expect("open")
    }

    fn stream_of(
        opened: FileOpen,
        shutdown: CancellationToken,
    ) -> impl Stream<Item = std::io::Result<Vec<u8>>> {
        download_stream(opened, shutdown, FILE_NAME.to_owned())
    }

    #[tokio::test]
    async fn chunks_stay_bounded_and_reassemble_the_file() {
        let content = patterned(3 * DOWNLOAD_CHUNK_BYTES + TRAILING_BYTES);
        let root = workspace_with(&content);
        let opened = open(&root).await;
        assert_eq!(opened.size, content.len() as u64);

        let chunks: Vec<Vec<u8>> = stream_of(opened, CancellationToken::new())
            .map(|chunk| chunk.expect("chunk"))
            .collect()
            .await;
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= DOWNLOAD_CHUNK_BYTES)
        );
        assert_eq!(chunks.concat(), content);
    }

    #[tokio::test]
    async fn a_file_that_shrinks_mid_stream_ends_the_body_with_an_error() {
        let root = workspace_with(&patterned(2 * DOWNLOAD_CHUNK_BYTES));
        let mut stream = pin!(stream_of(open(&root).await, CancellationToken::new()));
        stream
            .next()
            .await
            .expect("first item")
            .expect("first chunk");

        std::fs::OpenOptions::new()
            .write(true)
            .open(root.path().join(FILE_NAME))
            .and_then(|file| file.set_len(DOWNLOAD_CHUNK_BYTES as u64))
            .expect("truncate");
        let error = stream
            .next()
            .await
            .expect("error item")
            .expect_err("shrunk file");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn a_file_that_grows_mid_stream_is_sent_up_to_its_declared_length() {
        let content = patterned(2 * DOWNLOAD_CHUNK_BYTES);
        let root = workspace_with(&content);
        let mut stream = pin!(stream_of(open(&root).await, CancellationToken::new()));
        let mut received = stream.next().await.expect("item").expect("chunk");

        std::fs::OpenOptions::new()
            .append(true)
            .open(root.path().join(FILE_NAME))
            .and_then(|mut file| file.write_all(b"more"))
            .expect("append");
        while let Some(chunk) = stream.next().await {
            received.extend(chunk.expect("chunk"));
        }
        assert_eq!(received, content);
    }

    #[tokio::test]
    async fn shutdown_ends_the_body_with_an_error() {
        let root = workspace_with(&patterned(2 * DOWNLOAD_CHUNK_BYTES));
        let shutdown = CancellationToken::new();
        let mut stream = pin!(stream_of(open(&root).await, shutdown.clone()));
        stream
            .next()
            .await
            .expect("first item")
            .expect("first chunk");

        shutdown.cancel();
        stream
            .next()
            .await
            .expect("error item")
            .expect_err("shutdown");
    }

    fn state_for(root: &tempfile::TempDir) -> AppState {
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-placebo-stack.toml"
        ))
        .expect("fixture parses");
        config.workspace.root = root.path().to_string_lossy().into_owned();
        let state_dir = tempfile::tempdir().expect("state tempdir");
        let store = crate::state::StateStore::open(state_dir.path().join("state.sqlite"))
            .expect("state open");
        store.migrate().expect("migrate");
        // Leaked because the returned AppState keeps the sqlite handle open.
        std::mem::forget(state_dir);
        AppState::new(config, store, String::new(), String::new())
    }

    #[tokio::test]
    async fn a_slow_reader_outlives_the_job_timeout() {
        let content = patterned(2 * DOWNLOAD_CHUNK_BYTES + TRAILING_BYTES);
        let root = workspace_with(&content);
        let response = files_download_handler(
            State(state_for(&root)),
            Query(FilesPathParams {
                path: FILE_NAME.to_owned(),
            }),
        )
        .await
        .expect("download response");
        let mut body = response.into_body().into_data_stream();
        let mut received = body
            .next()
            .await
            .expect("first frame")
            .expect("first chunk")
            .to_vec();

        // Paused only after the handler returns, so the open's job timeout runs on the real clock.
        tokio::time::pause();
        tokio::time::sleep(DEFAULT_JOB_TIMEOUT * 2).await;
        while let Some(chunk) = body.next().await {
            received.extend_from_slice(&chunk.expect("chunk"));
        }
        assert_eq!(received, content);
    }
}
