//! 浏览器可用的局域网文件共享服务。
//!
//! HTTP 服务与内置 SFTP 共用共享目录、用户名、密码和传输活动监视器；页面本身
//! 嵌入二进制，不依赖外部静态资源。所有路径都先映射到共享根目录内，上传时逐级
//! 检查父目录，拒绝借助 `..`、绝对路径或越界符号链接逃逸沙箱。

use std::collections::HashSet;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::UNIX_EPOCH;

use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, REFERRER_POLICY,
    SET_COOKIE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures::{StreamExt as _, stream};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::session::store::DEFAULT_LAN_HTTP_PORT;
use crate::sftp::lan_server::{
    DEFAULT_PASSWORD, TransferActivity, canonical_root, local_ipv4, selected_password,
    selected_username,
};

pub const DEFAULT_PORT: u16 = DEFAULT_LAN_HTTP_PORT;
const SESSION_COOKIE: &str = "blip_http_session";
const INDEX_HTML: &str = include_str!("http_share.html");

#[derive(Clone)]
struct ShareState {
    root: Arc<RwLock<PathBuf>>,
    username: Arc<RwLock<String>>,
    password: Arc<RwLock<String>>,
    sessions: Arc<Mutex<HashSet<String>>>,
    transfer_activity: TransferActivity,
}

/// HTTP 服务的唯一所有者。替换或释放时会通知后台线程停止监听。
pub struct HttpServer {
    host: String,
    port: u16,
    state: ShareState,
    shutdown: Option<oneshot::Sender<()>>,
}

impl HttpServer {
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn username(&self) -> String {
        self.state.username()
    }

    pub fn password(&self) -> String {
        self.state.password()
    }

    pub fn root(&self) -> PathBuf {
        self.state.root()
    }

    pub fn set_username(&self, username: &str) -> Result<(), String> {
        *write_lock(&self.state.username) = selected_username(username)?;
        self.clear_sessions();
        Ok(())
    }

    pub fn set_password(&self, manual_password: Option<String>) -> Result<(), String> {
        *write_lock(&self.state.password) = selected_password(manual_password)?;
        self.clear_sessions();
        Ok(())
    }

    pub fn set_root(&self, root: PathBuf) -> Result<(), String> {
        *write_lock(&self.state.root) = canonical_root(Some(root))?;
        Ok(())
    }

    fn clear_sessions(&self) {
        lock(&self.state.sessions).clear();
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl ShareState {
    fn root(&self) -> PathBuf {
        read_lock(&self.root).clone()
    }

    fn username(&self) -> String {
        read_lock(&self.username).clone()
    }

    fn password(&self) -> String {
        read_lock(&self.password).clone()
    }

    fn accepts(&self, username: &str, password: &str) -> bool {
        username == read_lock(&self.username).as_str()
            && password.as_bytes() == read_lock(&self.password).as_bytes()
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 启动 HTTP 服务。配置端口必须精确可用；失败时由设置界面保留旧服务并提示用户。
pub fn start(
    configured_root: Option<PathBuf>,
    configured_username: &str,
    manual_password: Option<String>,
    port: u16,
    transfer_activity: TransferActivity,
) -> Result<HttpServer, String> {
    start_on(
        Ipv4Addr::UNSPECIFIED,
        configured_root,
        configured_username,
        manual_password,
        port,
        transfer_activity,
    )
}

fn start_on(
    host: Ipv4Addr,
    configured_root: Option<PathBuf>,
    configured_username: &str,
    manual_password: Option<String>,
    port: u16,
    transfer_activity: TransferActivity,
) -> Result<HttpServer, String> {
    let root = canonical_root(configured_root)?;
    let username = selected_username(configured_username)?;
    let password = selected_password(manual_password)?;
    let listener = TcpListener::bind((host, port))
        .map_err(|err| format!("无法监听 HTTP 端口 {port}：{err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("无法设置 HTTP 监听端口：{err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("无法读取 HTTP 监听端口：{err}"))?
        .port();
    let state = ShareState {
        root: Arc::new(RwLock::new(root)),
        username: Arc::new(RwLock::new(username)),
        password: Arc::new(RwLock::new(password)),
        sessions: Arc::new(Mutex::new(HashSet::new())),
        transfer_activity,
    };
    let app = router(state.clone());
    let (shutdown, shutdown_receiver) = oneshot::channel();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("blip-http-share-rt")
        .enable_all()
        .build()
        .map_err(|err| format!("创建 HTTP 共享 runtime 失败：{err}"))?;
    std::thread::Builder::new()
        .name("blip-http-share".to_string())
        .spawn(move || {
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(err) => {
                        log::error!("接管 HTTP 监听端口失败：{err}");
                        return;
                    }
                };
                let server = axum::serve(listener, app).with_graceful_shutdown(async {
                    let _ = shutdown_receiver.await;
                });
                if let Err(err) = server.await {
                    log::error!("HTTP 共享服务意外停止：{err}");
                }
            });
        })
        .map_err(|err| format!("无法创建 HTTP 共享线程：{err}"))?;

    let advertised_host = if host.is_unspecified() {
        local_ipv4().to_string()
    } else {
        host.to_string()
    };
    Ok(HttpServer {
        host: advertised_host,
        port,
        state,
        shutdown: Some(shutdown),
    })
}

fn router(state: ShareState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/info", get(info))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/files", get(list_files))
        .route("/api/upload", put(upload_file))
        .route("/api/download", get(download_file))
        .with_state(state)
}

async fn index() -> Response {
    let mut response = Html(INDEX_HTML).into_response();
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    response
}

#[derive(Serialize)]
struct PublicInfo {
    username: String,
    default_password: bool,
}

async fn info(State(state): State<ShareState>) -> Response {
    let mut response = Json(PublicInfo {
        username: state.username(),
        default_password: state.password() == DEFAULT_PASSWORD,
    })
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(
    State(state): State<ShareState>,
    Json(request): Json<LoginRequest>,
) -> ApiResult<Response> {
    if !state.accepts(request.username.trim(), &request.password) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "用户名或密码不正确",
        ));
    }
    let session = Uuid::new_v4().to_string();
    let mut sessions = lock(&state.sessions);
    if sessions.len() >= 256 {
        sessions.clear();
    }
    sessions.insert(session.clone());
    drop(sessions);

    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{SESSION_COOKIE}={session}; HttpOnly; SameSite=Strict; Path=/"
        ))
        .map_err(|_| ApiError::internal("无法创建登录会话"))?,
    );
    Ok(response)
}

async fn logout(State(state): State<ShareState>, headers: HeaderMap) -> Response {
    if let Some(session) = session_cookie(&headers) {
        lock(&state.sessions).remove(session);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_static(
            "blip_http_session=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/",
        ),
    );
    response
}

#[derive(Default, Deserialize)]
struct PathQuery {
    #[serde(default)]
    path: String,
}

#[derive(Serialize)]
struct FileEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    modified: u64,
}

async fn list_files(
    State(state): State<ShareState>,
    headers: HeaderMap,
    Query(query): Query<PathQuery>,
) -> ApiResult<Json<Vec<FileEntry>>> {
    require_auth(&state, &headers)?;
    let normalized = normalize_relative(&query.path)?;
    let directory = resolve_existing(&state, &normalized).await?;
    let metadata = tokio::fs::metadata(&directory)
        .await
        .map_err(ApiError::from_io)?;
    if !metadata.is_dir() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "路径不是文件夹"));
    }
    let current = normalized_path(&normalized);
    let mut reader = tokio::fs::read_dir(directory)
        .await
        .map_err(ApiError::from_io)?;
    let mut entries = Vec::new();
    while let Some(entry) = reader.next_entry().await.map_err(ApiError::from_io)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(metadata) = tokio::fs::metadata(entry.path()).await else {
            continue;
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs());
        entries.push(FileEntry {
            path: join_web_path(&current, &name),
            name,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified,
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(Json(entries))
}

#[derive(Deserialize)]
struct UploadQuery {
    path: String,
    #[serde(default)]
    overwrite: bool,
}

async fn upload_file(
    State(state): State<ShareState>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    body: Body,
) -> ApiResult<StatusCode> {
    require_auth(&state, &headers)?;
    let target = prepare_upload_target(&state, &query.path).await?;
    if let Ok(metadata) = tokio::fs::symlink_metadata(&target).await {
        if metadata.file_type().is_symlink() {
            return Err(ApiError::new(StatusCode::FORBIDDEN, "不能覆盖符号链接"));
        }
        if metadata.is_dir() {
            return Err(ApiError::new(StatusCode::CONFLICT, "同名文件夹已存在"));
        }
        if !query.overwrite {
            return Err(ApiError::new(StatusCode::CONFLICT, "同名文件已存在"));
        }
    }

    let parent = target
        .parent()
        .ok_or_else(|| ApiError::bad_request("上传路径无效"))?;
    let temporary = parent.join(format!(".blip-upload-{}", Uuid::new_v4()));
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await
        .map_err(ApiError::from_io)?;
    let _activity = state.transfer_activity.begin();
    let mut stream = body.into_data_stream();
    let write_result: Result<(), ApiError> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| ApiError::bad_request(format!("上传中断：{err}")))?;
            file.write_all(&chunk).await.map_err(ApiError::from_io)?;
        }
        file.flush().await.map_err(ApiError::from_io)?;
        drop(file);
        commit_upload(&temporary, &target, query.overwrite).await
    }
    .await;
    if write_result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    write_result?;
    Ok(StatusCode::CREATED)
}

async fn download_file(
    State(state): State<ShareState>,
    headers: HeaderMap,
    Query(query): Query<PathQuery>,
) -> ApiResult<Response> {
    require_auth(&state, &headers)?;
    let normalized = normalize_relative(&query.path)?;
    let path = resolve_existing(&state, &normalized).await?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(ApiError::from_io)?;
    if !metadata.is_file() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "路径不是文件"));
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(ApiError::from_io)?;
    let _activity = state.transfer_activity.begin();
    let stream = stream::try_unfold((file, _activity), |(mut file, activity)| async move {
        let mut buffer = vec![0_u8; 64 * 1024];
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            Ok::<_, std::io::Error>(None)
        } else {
            buffer.truncate(read);
            Ok(Some((Bytes::from(buffer), (file, activity))))
        }
    });
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string())
            .map_err(|_| ApiError::internal("无法生成文件长度"))?,
    );
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&content_disposition(filename))
            .map_err(|_| ApiError::internal("文件名无法用于下载"))?,
    );
    Ok(response)
}

fn require_auth(state: &ShareState, headers: &HeaderMap) -> ApiResult<()> {
    let authenticated =
        session_cookie(headers).is_some_and(|session| lock(&state.sessions).contains(session));
    if authenticated {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::UNAUTHORIZED, "请先登录"))
    }
}

fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix(&format!("{SESSION_COOKIE}=")))
}

fn normalize_relative(path: &str) -> ApiResult<PathBuf> {
    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => relative.push(name),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ApiError::new(StatusCode::FORBIDDEN, "路径超出共享目录"));
            }
        }
    }
    Ok(relative)
}

async fn resolve_existing(state: &ShareState, relative: &Path) -> ApiResult<PathBuf> {
    let root = state.root();
    let path = tokio::fs::canonicalize(root.join(relative))
        .await
        .map_err(ApiError::from_io)?;
    if path.starts_with(&root) {
        Ok(path)
    } else {
        Err(ApiError::new(StatusCode::FORBIDDEN, "路径超出共享目录"))
    }
}

async fn prepare_upload_target(state: &ShareState, path: &str) -> ApiResult<PathBuf> {
    let relative = normalize_relative(path)?;
    let filename = relative
        .file_name()
        .ok_or_else(|| ApiError::bad_request("请选择要上传的文件"))?;
    let root = state.root();
    let mut parent = root.clone();
    if let Some(relative_parent) = relative.parent() {
        for component in relative_parent.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            let candidate = parent.join(name);
            match tokio::fs::symlink_metadata(&candidate).await {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let canonical = tokio::fs::canonicalize(&candidate)
                        .await
                        .map_err(ApiError::from_io)?;
                    if !canonical.starts_with(&root)
                        || !tokio::fs::metadata(&canonical)
                            .await
                            .map_err(ApiError::from_io)?
                            .is_dir()
                    {
                        return Err(ApiError::new(StatusCode::FORBIDDEN, "上传目录超出共享目录"));
                    }
                    parent = canonical;
                }
                Ok(metadata) if metadata.is_dir() => parent = candidate,
                Ok(_) => {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "上传路径中存在同名文件",
                    ));
                }
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    tokio::fs::create_dir(&candidate)
                        .await
                        .map_err(ApiError::from_io)?;
                    parent = candidate;
                }
                Err(err) => return Err(ApiError::from_io(err)),
            }
        }
    }
    Ok(parent.join(filename))
}

async fn commit_upload(temporary: &Path, target: &Path, overwrite: bool) -> ApiResult<()> {
    if !overwrite {
        if let Err(error) = tokio::fs::hard_link(temporary, target).await {
            if error.kind() == ErrorKind::AlreadyExists {
                return Err(ApiError::new(StatusCode::CONFLICT, "同名文件已存在"));
            }
            copy_without_overwrite(temporary, target).await?;
        }
        tokio::fs::remove_file(temporary)
            .await
            .map_err(ApiError::from_io)?;
        return Ok(());
    }

    if tokio::fs::symlink_metadata(target).await.is_err() {
        tokio::fs::rename(temporary, target)
            .await
            .map_err(ApiError::from_io)?;
        return Ok(());
    }

    let backup = target.with_file_name(format!(".blip-backup-{}", Uuid::new_v4()));
    tokio::fs::rename(target, &backup)
        .await
        .map_err(ApiError::from_io)?;
    if let Err(error) = tokio::fs::rename(temporary, target).await {
        let _ = tokio::fs::rename(&backup, target).await;
        return Err(ApiError::from_io(error));
    }
    if let Err(error) = tokio::fs::remove_file(&backup).await {
        log::warn!("无法删除 HTTP 上传备份 {}：{error}", backup.display());
    }
    Ok(())
}

async fn copy_without_overwrite(temporary: &Path, target: &Path) -> ApiResult<()> {
    let mut source = tokio::fs::File::open(temporary)
        .await
        .map_err(ApiError::from_io)?;
    let mut destination = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .await
        .map_err(|error| {
            if error.kind() == ErrorKind::AlreadyExists {
                ApiError::new(StatusCode::CONFLICT, "同名文件已存在")
            } else {
                ApiError::from_io(error)
            }
        })?;
    if let Err(error) = tokio::io::copy(&mut source, &mut destination).await {
        drop(destination);
        let _ = tokio::fs::remove_file(target).await;
        return Err(ApiError::from_io(error));
    }
    destination.flush().await.map_err(ApiError::from_io)
}

fn normalized_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn join_web_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn content_disposition(filename: &str) -> String {
    let fallback: String = filename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let encoded = filename
        .as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'-' | b'_') {
                (*byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    fn from_io(error: std::io::Error) -> Self {
        let status = match error.kind() {
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            ErrorKind::AlreadyExists => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::time::Duration;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "blip-http-share-{}-{tag}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("创建测试目录");
        root.canonicalize().expect("规范化测试目录")
    }

    fn request(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("连接 HTTP 服务");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("设置读取超时");
        stream.write_all(request.as_bytes()).expect("发送请求");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("读取响应");
        response
    }

    #[test]
    fn path_normalization_rejects_escape_attempts() {
        assert_eq!(
            normalize_relative("folder/file.txt").unwrap(),
            PathBuf::from("folder/file.txt")
        );
        assert!(normalize_relative("../outside").is_err());
        assert!(normalize_relative("/absolute").is_err());
    }

    #[test]
    fn content_disposition_keeps_utf8_filename() {
        let header = content_disposition("报告 1.txt");
        assert!(header.contains("filename=\"___1.txt\""));
        assert!(header.contains("filename*=UTF-8''%E6%8A%A5%E5%91%8A%201.txt"));
    }

    #[test]
    fn configured_port_is_used_and_conflicts_are_reported() {
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("占用测试端口");
        let port = occupied.local_addr().expect("读取测试端口").port();
        let root = temp_root("configured-port");
        let (activity, _) = crate::sftp::lan_server::transfer_activity();
        let error = start_on(
            Ipv4Addr::LOCALHOST,
            Some(root.clone()),
            "zy",
            None,
            port,
            activity,
        )
        .err()
        .expect("端口冲突应启动失败");
        assert!(error.contains(&port.to_string()));
        std::fs::remove_dir_all(root).expect("清理测试目录");
    }

    #[test]
    fn authenticates_and_uploads_a_folder_tree() {
        let root = temp_root("integration");
        let (activity, _) = crate::sftp::lan_server::transfer_activity();
        let server = start_on(
            Ipv4Addr::LOCALHOST,
            Some(root.clone()),
            "zy",
            None,
            0,
            activity,
        )
        .expect("启动 HTTP 服务");
        let port = server.port();

        let public_info = request(
            port,
            "GET /api/info HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(
            public_info.contains("\"default_password\":true"),
            "{public_info}"
        );

        let unauthorized = request(
            port,
            "GET /api/files HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(unauthorized.starts_with("HTTP/1.1 401"));

        let login_body = r#"{"username":"zy","password":"88888888"}"#;
        let login = request(
            port,
            &format!(
                "POST /api/login HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                login_body.len(),
                login_body
            ),
        );
        assert!(login.starts_with("HTTP/1.1 204"));
        let cookie = login
            .lines()
            .find_map(|line| line.strip_prefix("set-cookie: "))
            .and_then(|value| value.split(';').next())
            .expect("登录 Cookie");

        let body = "folder upload";
        let upload = request(
            port,
            &format!(
                "PUT /api/upload?path=photos%2Ftrip%2Fnote.txt HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(upload.starts_with("HTTP/1.1 201"), "{upload}");
        assert_eq!(
            std::fs::read_to_string(root.join("photos/trip/note.txt")).expect("读取上传文件"),
            body
        );

        let listing = request(
            port,
            &format!(
                "GET /api/files?path=photos%2Ftrip HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(listing.contains("\"name\":\"note.txt\""), "{listing}");

        let download = request(
            port,
            &format!(
                "GET /api/download?path=photos%2Ftrip%2Fnote.txt HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(download.starts_with("HTTP/1.1 200"));
        assert!(download.ends_with(body));

        let conflict = request(
            port,
            &format!(
                "PUT /api/upload?path=photos%2Ftrip%2Fnote.txt HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(conflict.starts_with("HTTP/1.1 409"), "{conflict}");

        let replacement = "replacement";
        let overwrite = request(
            port,
            &format!(
                "PUT /api/upload?path=photos%2Ftrip%2Fnote.txt&overwrite=true HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{replacement}",
                replacement.len()
            ),
        );
        assert!(overwrite.starts_with("HTTP/1.1 201"), "{overwrite}");
        assert_eq!(
            std::fs::read_to_string(root.join("photos/trip/note.txt")).expect("读取覆盖后的文件"),
            replacement
        );

        drop(server);
        std::fs::remove_dir_all(root).expect("清理测试目录");
    }
}
