use axum::Extension;
use axum::Json;
use axum::body::Body;
use axum::extract::{Multipart, Query, State};
use axum::response::Response;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use crate::auth::KeyKind;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::workspace::{
    self, FileMetadata, FileRead, PathIntent, WorkspaceListing, resolve_workspace_path,
};

#[derive(Serialize)]
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

#[derive(Deserialize)]
pub(crate) struct FilesPathParams {
    path: String,
}

#[derive(Serialize)]
pub(crate) struct FilesListResponse {
    path: String,
    entries: Vec<FilesListEntry>,
}

#[derive(Serialize)]
pub(crate) struct FilesListEntry {
    name: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    modified: String,
}

pub(crate) async fn files_list_handler(
    State(state): State<AppState>,
    Query(params): Query<FilesPathParams>,
) -> std::result::Result<ApiSuccess<FilesListResponse>, StackError> {
    let root = state.config.workspace.root.clone();
    let requested = params.path.clone();
    let listing: WorkspaceListing = tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&root),
            &requested,
            PathIntent::ReadExisting,
        )?;
        workspace::list_directory(&absolute)
    })
    .await
    .map_err(spawn_blocking_to_io)??;
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

#[derive(Serialize)]
pub(crate) struct FilesContentResponse {
    path: String,
    encoding: String,
    content: String,
    size: u64,
    modified: String,
}

pub(crate) async fn files_content_get_handler(
    State(state): State<AppState>,
    Query(params): Query<FilesPathParams>,
) -> std::result::Result<ApiSuccess<FilesContentResponse>, StackError> {
    let root = state.config.workspace.root.clone();
    let requested = params.path.clone();
    let max_bytes = state.config.workspace.max_file_bytes;
    let read: FileRead = tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&root),
            &requested,
            PathIntent::ReadExisting,
        )?;
        workspace::read_file(&absolute, max_bytes)
    })
    .await
    .map_err(spawn_blocking_to_io)??;
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
    let root = state.config.workspace.root.clone();
    let requested = params.path.clone();
    let max_bytes = state.config.workspace.max_file_bytes;
    let read: FileRead = tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&root),
            &requested,
            PathIntent::ReadExisting,
        )?;
        workspace::read_file(&absolute, max_bytes)
    })
    .await
    .map_err(spawn_blocking_to_io)??;
    let filename = std::path::Path::new(&params.path)
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
        .header(http::header::CONTENT_LENGTH, read.size)
        .header(http::header::CONTENT_DISPOSITION, disposition)
        .body(Body::from(read.content))
        .map_err(|_| StackError::WorkspaceIo {
            requested: params.path.clone(),
            source: std::io::Error::other("failed to build download response"),
        })?;
    Ok(response)
}

#[derive(Deserialize)]
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
    let root = state.config.workspace.root.clone();
    let requested = body.path.clone();
    let metadata: FileMetadata = tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&root),
            &requested,
            PathIntent::WriteOrCreate,
        )?;
        workspace::write_file_atomic(&absolute, &bytes)
    })
    .await
    .map_err(spawn_blocking_to_io)??;

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
                // Stream chunks instead of buffering the whole part: the HTTP
                // body cap (api.max_request_bytes) may be larger than
                // workspace.max_file_bytes, and we want to stop accumulating
                // bytes the moment we cross the per-file limit instead of
                // letting an authenticated client push the bigger cap of
                // memory through this handler.
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

    // Resolution against `workspace.root` (not against `workspace.uploads`)
    // even though the request path is uploads-relative. This means a symlink
    // at `workspace.uploads` that points outside the root gets caught by the
    // resolver's canonicalize-and-starts_with check; resolving directly under
    // `workspace.uploads` would treat it as its own containment root and let
    // an escape slip through. The config validator already rejects an
    // `uploads` path that is not lexically under `root`.
    if std::path::Path::new(&path).is_absolute() {
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
    let workspace_root = state.config.workspace.root.clone();
    let target_relative = workspace_relative_path.clone();
    let bytes = content;
    let metadata: FileMetadata = tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&workspace_root),
            &target_relative,
            PathIntent::WriteOrCreate,
        )?;
        workspace::write_file_atomic(&absolute, &bytes)
    })
    .await
    .map_err(spawn_blocking_to_io)??;

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
    let root = state.config.workspace.root.clone();
    let requested = params.path.clone();
    tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&root),
            &requested,
            PathIntent::WriteOrCreate,
        )?;
        workspace::delete_file(&absolute)
    })
    .await
    .map_err(spawn_blocking_to_io)??;

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
        // `message` is empty: the kind + payload carry the structured detail,
        // and we want sanitized logs that do not echo user paths into the
        // text column (`logs/events` is session-tier-readable).
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

#[derive(Serialize)]
pub(crate) struct FileMutationResponse {
    path: String,
    size: u64,
    modified: String,
}

#[derive(Serialize)]
pub(crate) struct FileUploadResponse {
    path: String,
    filename: String,
    size: u64,
    modified: String,
}

#[derive(Serialize)]
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

/// A panic in `spawn_blocking` should propagate as a 500; the join failure is
/// strictly an internal fault, so we surface a generic `WorkspaceIo` rather
/// than a path-specific code.
fn spawn_blocking_to_io(error: tokio::task::JoinError) -> StackError {
    StackError::WorkspaceIo {
        requested: "<background task>".to_owned(),
        source: std::io::Error::other(error.to_string()),
    }
}
