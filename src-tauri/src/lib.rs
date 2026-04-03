use anyhow::{anyhow, Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{overlay, FilterType};
use image::{DynamicImage, ImageBuffer, Rgba, RgbaImage};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Cursor, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};
use tauri::async_runtime::Mutex as AsyncMutex;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_opener::OpenerExt;
use tempfile::{tempdir, TempDir};
use v4l::buffer::Type;
use v4l::format::FourCC;
use v4l::framesize::FrameSizeEnum;
use v4l::io::traits::CaptureStream;
use v4l::prelude::MmapStream;
use v4l::video::Capture;
use v4l::{Device, Format};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const APP_VERSION: u32 = 1;
const PROJECT_FPS: u32 = 15;
const STATUS_EVENT: &str = "jind://status";
const DEFAULT_PROJECT_NAME: &str = "untitled.jind";
const MAX_CAMERA_WIDTH: u32 = 1920;
const MAX_CAMERA_HEIGHT: u32 = 1080;
const DEFAULT_CAMERA_WIDTH: u32 = 1920;
const DEFAULT_CAMERA_HEIGHT: u32 = 1080;
const PREVIEW_INTERVAL_MS: u64 = 400;
const PREVIEW_WIDTH: u32 = 640;
const PREVIEW_HEIGHT: u32 = 360;
const PREVIEW_JPEG_QUALITY: u8 = 55;

#[derive(Clone)]
struct SharedState {
    app: Arc<SharedApp>,
}

struct SharedApp {
    inner: Mutex<AppInner>,
    operation_lock: AsyncMutex<()>,
    media_server: MediaServer,
}

struct AppInner {
    session: Option<ProjectSession>,
    selected_camera: Option<SelectedCamera>,
    preview: Option<PreviewWorker>,
    busy_detail: Option<String>,
    last_result: Option<LastOperationResult>,
    close_requested: bool,
    shutdown_started: bool,
}

struct ProjectSession {
    project_path: PathBuf,
    workspace: TempDir,
    manifest: ProjectManifest,
    next_frame_id: u64,
}

struct PreviewWorker {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    latest_frame: Arc<Mutex<Option<LatestFrame>>>,
}

struct MediaServer {
    base_url: Option<String>,
    preview_state: Arc<PreviewBroadcast>,
}

struct PreviewBroadcast {
    state: Mutex<PreviewStreamState>,
    ready: Condvar,
}

struct PreviewStreamState {
    jpeg: Option<Vec<u8>>,
    version: u64,
    freeze_depth: u32,
}

#[derive(Clone)]
struct LatestFrame {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    fourcc: FourCC,
}

#[derive(Debug, Clone)]
struct SelectedCamera {
    device_id: String,
    label: String,
    mode: CameraMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectManifest {
    version: u32,
    fps: u32,
    resolution: Option<ProjectResolution>,
    frames: Vec<FrameEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectResolution {
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrameEntry {
    id: u64,
    image_path: String,
    thumb_path: String,
}

#[derive(Debug, Clone, Serialize)]
struct AppSnapshot {
    project: Option<ProjectView>,
    status: OperationStatusView,
    selected_camera: Option<SelectedCameraView>,
    ffmpeg_available: bool,
    preview_url: Option<String>,
    preview_still_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ProjectView {
    project_path: String,
    fps: u32,
    resolution: Option<ProjectResolution>,
    frames: Vec<FrameView>,
}

#[derive(Debug, Clone, Serialize)]
struct FrameView {
    id: u64,
    image_url: String,
    thumb_url: String,
}

#[derive(Debug, Clone, Serialize)]
struct OperationStatusView {
    busy: bool,
    phase: String,
    detail: Option<String>,
    last_result: Option<LastOperationResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LastOperationResult {
    ok: bool,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CameraDeviceView {
    device_id: String,
    label: String,
    modes: Vec<CameraMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CameraMode {
    id: String,
    label: String,
    width: u32,
    height: u32,
    pixel_format: String,
}

#[derive(Debug, Clone, Serialize)]
struct SelectedCameraView {
    device_id: String,
    label: String,
    mode: CameraMode,
}

impl SharedState {
    fn new() -> Self {
        let app = Arc::new_cyclic(|weak| SharedApp {
            inner: Mutex::new(AppInner {
                session: None,
                selected_camera: None,
                preview: None,
                busy_detail: None,
                last_result: None,
                close_requested: false,
                shutdown_started: false,
            }),
            operation_lock: AsyncMutex::new(()),
            media_server: MediaServer::new(weak.clone()),
        });

        Self { app }
    }
}

impl SharedApp {
    fn snapshot(&self) -> Result<AppSnapshot> {
        let inner = self.inner.lock().unwrap();
        build_snapshot(&inner, self.media_server.base_url.as_deref())
    }

    fn emit_status(&self, app: &AppHandle) {
        let payload = {
            let inner = self.inner.lock().unwrap();
            build_status(&inner)
        };
        let _ = app.emit(STATUS_EVENT, payload);
    }

    fn set_busy(&self, app: &AppHandle, detail: impl Into<String>) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.busy_detail = Some(detail.into());
        }
        self.emit_status(app);
    }

    fn finish_operation(&self, app: &AppHandle, ok: bool, message: impl Into<String>) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.busy_detail = None;
            inner.last_result = Some(LastOperationResult {
                ok,
                message: message.into(),
            });
        }
        self.emit_status(app);
    }

    fn mark_close_requested(&self, app: &AppHandle) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.close_requested = true;
        }
        self.emit_status(app);
    }

    fn is_busy(&self) -> bool {
        self.inner.lock().unwrap().busy_detail.is_some()
    }

    fn should_start_shutdown(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if !inner.close_requested || inner.busy_detail.is_some() || inner.shutdown_started {
            return false;
        }
        inner.shutdown_started = true;
        inner.busy_detail = Some("Cleaning up workspace.".to_string());
        true
    }

    fn set_last_result(&self, app: &AppHandle, ok: bool, message: impl Into<String>) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.last_result = Some(LastOperationResult {
                ok,
                message: message.into(),
            });
        }
        self.emit_status(app);
    }
}

impl MediaServer {
    fn new(shared: Weak<SharedApp>) -> Self {
        let preview_state = Arc::new(PreviewBroadcast {
            state: Mutex::new(PreviewStreamState {
                jpeg: None,
                version: 0,
                freeze_depth: 0,
            }),
            ready: Condvar::new(),
        });

        let listener = TcpListener::bind(("127.0.0.1", 0)).ok();
        let base_url = listener
            .as_ref()
            .and_then(|listener| listener.local_addr().ok())
            .map(|address| format!("http://{}", address));

        if let Some(listener) = listener {
            let preview_state_for_thread = preview_state.clone();
            thread::spawn(move || {
                let _ = listener.set_nonblocking(true);
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let preview_state = preview_state_for_thread.clone();
                            let shared = shared.clone();
                            thread::spawn(move || {
                                let _ = serve_media_client(stream, shared, preview_state);
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(25));
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        Self {
            base_url,
            preview_state,
        }
    }

    fn set_preview_jpeg(&self, jpeg: Vec<u8>) {
        let mut state = self.preview_state.state.lock().unwrap();
        if state.freeze_depth > 0 {
            return;
        }
        state.jpeg = Some(jpeg);
        state.version = state.version.wrapping_add(1);
        self.preview_state.ready.notify_all();
    }

    fn freeze_preview(&self) {
        let mut state = self.preview_state.state.lock().unwrap();
        state.freeze_depth = state.freeze_depth.saturating_add(1);
    }

    fn unfreeze_preview(&self) {
        let mut state = self.preview_state.state.lock().unwrap();
        state.freeze_depth = state.freeze_depth.saturating_sub(1);
    }

    fn clear_preview(&self) {
        let mut state = self.preview_state.state.lock().unwrap();
        state.jpeg = None;
        state.freeze_depth = 0;
        state.version = state.version.wrapping_add(1);
        self.preview_state.ready.notify_all();
    }
}

#[tauri::command]
fn pick_new_project_path() -> Option<String> {
    let path = FileDialog::new()
        .add_filter("Jind project", &["jind"])
        .set_file_name(DEFAULT_PROJECT_NAME)
        .save_file()?;
    Some(normalize_project_path(&path).to_string_lossy().into_owned())
}

#[tauri::command]
fn pick_open_project_path() -> Option<String> {
    FileDialog::new()
        .add_filter("Jind project", &["jind"])
        .pick_file()
        .map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
fn pick_export_path() -> Option<String> {
    let path = FileDialog::new()
        .add_filter("MP4 video", &["mp4"])
        .set_file_name("animation.mp4")
        .save_file()?;
    Some(normalize_export_path(&path).to_string_lossy().into_owned())
}

#[tauri::command]
fn get_project_state(state: State<'_, SharedState>) -> Result<AppSnapshot, String> {
    state.app.snapshot().map_err(error_to_string)
}

#[tauri::command]
fn list_cameras() -> Result<Vec<CameraDeviceView>, String> {
    enumerate_cameras().map_err(error_to_string)
}

#[tauri::command]
async fn create_project(
    path: String,
    state: State<'_, SharedState>,
    app: AppHandle,
) -> Result<AppSnapshot, String> {
    let app_for_task = app.clone();
    run_locked_mutation(state, app, "Creating project...", move |shared| {
        create_project_impl(shared.clone(), PathBuf::from(path))?;
        autoselect_first_camera(shared, &app_for_task);
        Ok("Project created.".to_string())
    })
    .await
}

#[tauri::command]
async fn open_project(
    path: String,
    state: State<'_, SharedState>,
    app: AppHandle,
) -> Result<AppSnapshot, String> {
    let app_for_task = app.clone();
    run_locked_mutation(state, app, "Opening project...", move |shared| {
        open_project_impl(shared.clone(), PathBuf::from(path))?;
        autoselect_first_camera(shared, &app_for_task);
        Ok("Project opened.".to_string())
    })
    .await
}

#[tauri::command]
async fn select_camera(
    device_id: String,
    mode_id: String,
    state: State<'_, SharedState>,
    app: AppHandle,
) -> Result<AppSnapshot, String> {
    let app_for_task = app.clone();
    run_locked_mutation(state, app, "Switching camera...", move |shared| {
        select_camera_impl(shared, &app_for_task, device_id, mode_id)?;
        Ok("Camera ready.".to_string())
    })
    .await
}

#[tauri::command]
async fn capture_frame(
    state: State<'_, SharedState>,
    app: AppHandle,
) -> Result<AppSnapshot, String> {
    let app_for_task = app.clone();
    run_locked_mutation(state, app, "Capturing frame...", move |shared| {
        shared.media_server.freeze_preview();
        let result = capture_frame_impl(shared.clone(), &app_for_task);
        shared.media_server.unfreeze_preview();
        result?;
        Ok("Frame captured and autosaved.".to_string())
    })
    .await
}

#[tauri::command]
async fn delete_frame(
    frame_id: u64,
    state: State<'_, SharedState>,
    app: AppHandle,
) -> Result<AppSnapshot, String> {
    run_locked_mutation(state, app, "Deleting frame...", move |shared| {
        delete_frame_impl(shared, frame_id)?;
        Ok("Frame deleted and autosaved.".to_string())
    })
    .await
}

#[tauri::command]
async fn reorder_frames(
    frame_ids_in_order: Vec<u64>,
    state: State<'_, SharedState>,
    app: AppHandle,
) -> Result<AppSnapshot, String> {
    run_locked_mutation(state, app, "Reordering timeline...", move |shared| {
        reorder_frames_impl(shared, frame_ids_in_order)?;
        Ok("Timeline updated and autosaved.".to_string())
    })
    .await
}

#[tauri::command]
async fn export_mp4(
    path: String,
    state: State<'_, SharedState>,
    app: AppHandle,
) -> Result<AppSnapshot, String> {
    run_locked_mutation(state, app, "Exporting movie...", move |shared| {
        export_mp4_impl(shared, PathBuf::from(path))?;
        Ok("Export finished.".to_string())
    })
    .await
}

#[tauri::command]
fn reveal_export_in_folder(path: String, app: AppHandle) -> Result<(), String> {
    app.opener()
        .reveal_item_in_dir(normalize_export_path(Path::new(&path)))
        .map_err(error_to_string)
}

async fn run_locked_mutation<F>(
    state: State<'_, SharedState>,
    app: AppHandle,
    busy_label: &'static str,
    task: F,
) -> Result<AppSnapshot, String>
where
    F: FnOnce(Arc<SharedApp>) -> Result<String> + Send + 'static,
{
    let shared = state.app.clone();
    let _guard = shared.operation_lock.lock().await;
    shared.set_busy(&app, busy_label);

    let result = tauri::async_runtime::spawn_blocking({
        let shared = shared.clone();
        move || task(shared)
    })
    .await
    .map_err(|error| error.to_string())
    .and_then(|result| result.map_err(error_to_string));

    match result {
        Ok(message) => {
            shared.finish_operation(&app, true, message);
            let snapshot = shared.snapshot().map_err(error_to_string)?;
            spawn_shutdown_if_requested(shared.clone(), app.clone());
            Ok(snapshot)
        }
        Err(error) => {
            shared.finish_operation(&app, false, error.clone());
            spawn_shutdown_if_requested(shared.clone(), app.clone());
            Err(error)
        }
    }
}

fn create_project_impl(shared: Arc<SharedApp>, project_path: PathBuf) -> Result<()> {
    let normalized_path = normalize_project_path(&project_path);
    let session = ProjectSession {
        project_path: normalized_path,
        workspace: tempdir().context("failed to create temporary workspace")?,
        manifest: ProjectManifest {
            version: APP_VERSION,
            fps: PROJECT_FPS,
            resolution: None,
            frames: Vec::new(),
        },
        next_frame_id: 1,
    };
    ensure_workspace_dirs(session.workspace.path())?;
    write_manifest_to_workspace(&session)?;
    write_archive(&session)?;

    {
        let mut inner = shared.inner.lock().unwrap();
        stop_preview_locked(shared.as_ref(), &mut inner);
        inner.session = Some(session);
    }

    Ok(())
}

fn open_project_impl(shared: Arc<SharedApp>, project_path: PathBuf) -> Result<()> {
    let project_path = normalize_project_path(&project_path);
    let mut session = unpack_project(&project_path)?;
    session.project_path = project_path;

    {
        let mut inner = shared.inner.lock().unwrap();
        stop_preview_locked(shared.as_ref(), &mut inner);
        inner.session = Some(session);
    }

    Ok(())
}

fn select_camera_impl(
    shared: Arc<SharedApp>,
    app: &AppHandle,
    device_id: String,
    mode_id: String,
) -> Result<()> {
    let selected = enumerate_cameras()?
        .into_iter()
        .find(|device| device.device_id == device_id)
        .and_then(|device| {
            device
                .modes
                .into_iter()
                .find(|mode| mode.id == mode_id)
                .map(|mode| SelectedCamera {
                    device_id: device.device_id,
                    label: device.label,
                    mode,
                })
        })
        .ok_or_else(|| anyhow!("Requested camera mode no longer exists."))?;

    {
        let mut inner = shared.inner.lock().unwrap();
        stop_preview_locked(shared.as_ref(), &mut inner);
        inner.selected_camera = Some(selected.clone());
        inner.preview = Some(start_preview_worker(shared.clone(), app.clone(), selected));
    }

    Ok(())
}

fn autoselect_first_camera(shared: Arc<SharedApp>, app: &AppHandle) {
    let Ok(cameras) = enumerate_cameras() else {
        return;
    };
    let Some(device) = cameras.first() else {
        return;
    };
    let Some(mode) = device.modes.first() else {
        return;
    };

    let _ = select_camera_impl(shared, app, device.device_id.clone(), mode.id.clone());
}

fn capture_frame_impl(shared: Arc<SharedApp>, app: &AppHandle) -> Result<()> {
    let (selected, latest_frame) = {
        let inner = shared.inner.lock().unwrap();
        let selected = inner
            .selected_camera
            .clone()
            .ok_or_else(|| anyhow!("Select a camera mode before capturing."))?;
        let latest_frame = inner
            .preview
            .as_ref()
            .and_then(|worker| worker.latest_frame.lock().unwrap().clone())
            .ok_or_else(|| anyhow!("Camera preview is not ready yet."))?;
        (selected, latest_frame)
    };

    let captured = decode_frame(
        &latest_frame.bytes,
        latest_frame.width,
        latest_frame.height,
        &latest_frame.fourcc,
    )?;

    {
        let mut inner = shared.inner.lock().unwrap();
        let session = inner
            .session
            .as_mut()
            .ok_or_else(|| anyhow!("Open or create a project before capturing."))?;

        let resolution = session
            .manifest
            .resolution
            .clone()
            .unwrap_or(ProjectResolution {
                width: latest_frame.width,
                height: latest_frame.height,
            });

        if session.manifest.resolution.is_none() {
            session.manifest.resolution = Some(resolution.clone());
        }

        let normalized = normalize_to_resolution(&captured, &resolution);
        let frame_name = format!("frame-{id:06}.jpg", id = session.next_frame_id);
        let frame_rel = PathBuf::from("frames").join(&frame_name);
        let thumb_rel = PathBuf::from("thumbs").join(&frame_name);
        let frame_abs = session.workspace.path().join(&frame_rel);
        let thumb_abs = session.workspace.path().join(&thumb_rel);

        write_jpeg(&frame_abs, &normalized, 92)?;
        let thumb = normalized.resize(240, 135, FilterType::Triangle);
        write_jpeg(&thumb_abs, &thumb, 80)?;

        session.manifest.frames.push(FrameEntry {
            id: session.next_frame_id,
            image_path: rel_path_string(&frame_rel),
            thumb_path: rel_path_string(&thumb_rel),
        });
        session.next_frame_id += 1;

        write_manifest_to_workspace(session)?;
        write_archive(session)?;
    }

    let _ = (app, selected);
    Ok(())
}

fn delete_frame_impl(shared: Arc<SharedApp>, frame_id: u64) -> Result<()> {
    {
        let mut inner = shared.inner.lock().unwrap();
        let session = inner
            .session
            .as_mut()
            .ok_or_else(|| anyhow!("Open or create a project before deleting frames."))?;

        let index = session
            .manifest
            .frames
            .iter()
            .position(|frame| frame.id == frame_id)
            .ok_or_else(|| anyhow!("Frame not found."))?;

        let removed = session.manifest.frames.remove(index);
        write_manifest_to_workspace(session)?;
        write_archive(session)?;
        remove_if_exists(session.workspace.path().join(removed.image_path))?;
        remove_if_exists(session.workspace.path().join(removed.thumb_path))?;
    }

    Ok(())
}

fn reorder_frames_impl(shared: Arc<SharedApp>, frame_ids_in_order: Vec<u64>) -> Result<()> {
    {
        let mut inner = shared.inner.lock().unwrap();
        let session = inner
            .session
            .as_mut()
            .ok_or_else(|| anyhow!("Open or create a project before reordering."))?;

        if frame_ids_in_order.len() != session.manifest.frames.len() {
            return Err(anyhow!("Timeline reorder payload is incomplete."));
        }

        let mut frame_map: HashMap<u64, FrameEntry> = session
            .manifest
            .frames
            .drain(..)
            .map(|frame| (frame.id, frame))
            .collect();

        let mut reordered = Vec::with_capacity(frame_ids_in_order.len());
        for frame_id in frame_ids_in_order {
            let frame = frame_map
                .remove(&frame_id)
                .ok_or_else(|| anyhow!("Timeline reorder payload contains an unknown frame."))?;
            reordered.push(frame);
        }

        if !frame_map.is_empty() {
            return Err(anyhow!(
                "Timeline reorder payload is missing existing frames."
            ));
        }

        session.manifest.frames = reordered;
        write_manifest_to_workspace(session)?;
        write_archive(session)?;
    }

    Ok(())
}

fn export_mp4_impl(shared: Arc<SharedApp>, export_path: PathBuf) -> Result<()> {
    ensure_ffmpeg_available()?;

    let export_path = normalize_export_path(&export_path);
    let session = {
        let inner = shared.inner.lock().unwrap();
        inner
            .session
            .as_ref()
            .ok_or_else(|| anyhow!("Open or create a project before exporting."))?
            .project_path
            .clone()
    };

    let workspace_session = {
        let inner = shared.inner.lock().unwrap();
        inner
            .session
            .as_ref()
            .unwrap()
            .workspace
            .path()
            .to_path_buf()
    };
    let manifest = {
        let inner = shared.inner.lock().unwrap();
        inner.session.as_ref().unwrap().manifest.clone()
    };

    if manifest.frames.is_empty() {
        return Err(anyhow!("Capture at least one frame before exporting."));
    }

    let sequence_dir = tempdir().context("failed to create temporary export sequence")?;
    for (index, frame) in manifest.frames.iter().enumerate() {
        let source = workspace_session.join(&frame.image_path);
        let destination = sequence_dir
            .path()
            .join(format!("frame-{index:06}.jpg", index = index + 1));
        fs::copy(&source, &destination)
            .with_context(|| format!("failed to stage {}", source.display()))?;
    }

    if let Some(parent) = export_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let status = Command::new("ffmpeg")
        .arg("-y")
        .arg("-framerate")
        .arg(PROJECT_FPS.to_string())
        .arg("-i")
        .arg(sequence_dir.path().join("frame-%06d.jpg"))
        .arg("-vf")
        .arg("scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2")
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg(export_path.as_os_str())
        .status()
        .context("failed to spawn ffmpeg")?;

    if !status.success() {
        return Err(anyhow!("ffmpeg exited with a non-zero status."));
    }

    let _ = session;
    Ok(())
}

fn unpack_project(project_path: &Path) -> Result<ProjectSession> {
    let file = File::open(project_path)
        .with_context(|| format!("failed to open {}", project_path.display()))?;
    let mut archive = ZipArchive::new(file).context("project archive is not a valid ZIP file")?;
    let workspace = tempdir().context("failed to create temporary workspace")?;
    ensure_workspace_dirs(workspace.path())?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("project archive contains an invalid path"))?
            .to_path_buf();
        let out_path = workspace.path().join(name);
        if entry.name().ends_with('/') {
            fs::create_dir_all(&out_path)?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&out_path)
            .with_context(|| format!("failed to extract {}", out_path.display()))?;
        std::io::copy(&mut entry, &mut output)?;
    }

    let manifest_path = workspace.path().join("project.json");
    let manifest_text =
        fs::read_to_string(&manifest_path).context("project archive is missing project.json")?;
    let manifest: ProjectManifest =
        serde_json::from_str(&manifest_text).context("project.json is invalid")?;

    for frame in &manifest.frames {
        validate_relative_path(&frame.image_path)?;
        validate_relative_path(&frame.thumb_path)?;
        let frame_path = workspace.path().join(&frame.image_path);
        let thumb_path = workspace.path().join(&frame.thumb_path);
        if !frame_path.exists() || !thumb_path.exists() {
            return Err(anyhow!(
                "project archive is missing one or more frame assets"
            ));
        }
    }

    let next_frame_id = manifest
        .frames
        .iter()
        .map(|frame| frame.id)
        .max()
        .unwrap_or(0)
        + 1;
    Ok(ProjectSession {
        project_path: project_path.to_path_buf(),
        workspace,
        manifest,
        next_frame_id,
    })
}

fn write_archive(session: &ProjectSession) -> Result<()> {
    if let Some(parent) = session.project_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let temporary_path = session.project_path.with_extension("jind.tmp");
    let file = File::create(&temporary_path)
        .with_context(|| format!("failed to create {}", temporary_path.display()))?;
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    writer.add_directory("frames/", options)?;
    writer.add_directory("thumbs/", options)?;
    writer.start_file("project.json", options)?;
    writer.write_all(serde_json::to_string_pretty(&session.manifest)?.as_bytes())?;

    for frame in &session.manifest.frames {
        add_file_to_archive(
            &mut writer,
            session.workspace.path(),
            &frame.image_path,
            options,
        )?;
        add_file_to_archive(
            &mut writer,
            session.workspace.path(),
            &frame.thumb_path,
            options,
        )?;
    }

    writer.finish()?;
    fs::rename(&temporary_path, &session.project_path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            session.project_path.display(),
            temporary_path.display()
        )
    })?;
    Ok(())
}

fn add_file_to_archive(
    writer: &mut ZipWriter<File>,
    workspace_root: &Path,
    relative_path: &str,
    options: SimpleFileOptions,
) -> Result<()> {
    validate_relative_path(relative_path)?;
    let absolute_path = workspace_root.join(relative_path);
    let bytes = fs::read(&absolute_path)
        .with_context(|| format!("failed to read {}", absolute_path.display()))?;
    writer.start_file(relative_path.replace('\\', "/"), options)?;
    writer.write_all(&bytes)?;
    Ok(())
}

fn write_manifest_to_workspace(session: &ProjectSession) -> Result<()> {
    let manifest_path = session.workspace.path().join("project.json");
    let content = serde_json::to_string_pretty(&session.manifest)?;
    fs::write(&manifest_path, content)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    Ok(())
}

fn enumerate_cameras() -> Result<Vec<CameraDeviceView>> {
    let mut devices = Vec::new();
    let mut entries = fs::read_dir("/dev").context("failed to read /dev for cameras")?;
    while let Some(entry) = entries.next() {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !file_name.starts_with("video") {
            continue;
        }

        let path = entry.path();
        let Ok(device) = Device::with_path(&path) else {
            continue;
        };
        let Ok(caps) = device.query_caps() else {
            continue;
        };

        let mut modes = Vec::new();
        let mut uncapped_modes = Vec::new();
        for format in device.enum_formats().unwrap_or_default() {
            let pixel_format = format.fourcc.str().unwrap_or("").to_string();
            if !matches!(pixel_format.as_str(), "MJPG" | "YUYV" | "YUY2") {
                continue;
            }

            for frame_size in device.enum_framesizes(format.fourcc).unwrap_or_default() {
                match frame_size.size {
                    FrameSizeEnum::Discrete(size) => {
                        let mode = CameraMode {
                            id: camera_mode_id(&path, format.fourcc, size.width, size.height),
                            label: format!(
                                "{width}x{height} {pixel_format}",
                                width = size.width,
                                height = size.height
                            ),
                            width: size.width,
                            height: size.height,
                            pixel_format: pixel_format.clone(),
                        };
                        if is_within_camera_cap(mode.width, mode.height) {
                            modes.push(mode.clone());
                        }
                        uncapped_modes.push(mode);
                    }
                    FrameSizeEnum::Stepwise(stepwise) => {
                        let candidates = [
                            (1280, 720),
                            (1920, 1080),
                            (1024, 768),
                            (800, 600),
                            (640, 480),
                            (320, 240),
                        ];
                        for (width, height) in candidates {
                            if width < stepwise.min_width
                                || width > stepwise.max_width
                                || height < stepwise.min_height
                                || height > stepwise.max_height
                            {
                                continue;
                            }
                            let mode = CameraMode {
                                id: camera_mode_id(&path, format.fourcc, width, height),
                                label: format!("{width}x{height} {pixel_format}"),
                                width,
                                height,
                                pixel_format: pixel_format.clone(),
                            };
                            if is_within_camera_cap(mode.width, mode.height) {
                                modes.push(mode.clone());
                            }
                            uncapped_modes.push(mode);
                        }
                    }
                }
            }
        }

        if modes.is_empty() {
            modes = uncapped_modes;
        }

        modes.sort_by(camera_mode_sort_key);
        modes.dedup_by(|left, right| left.id == right.id);

        if !modes.is_empty() {
            devices.push(CameraDeviceView {
                device_id: path.to_string_lossy().into_owned(),
                label: format!("{} ({})", caps.card, path.display()),
                modes,
            });
        }
    }

    devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));
    Ok(devices)
}

fn is_within_camera_cap(width: u32, height: u32) -> bool {
    width <= MAX_CAMERA_WIDTH && height <= MAX_CAMERA_HEIGHT
}

fn camera_mode_sort_key(left: &CameraMode, right: &CameraMode) -> std::cmp::Ordering {
    mode_rank(right)
        .cmp(&mode_rank(left))
        .then((right.width * right.height).cmp(&(left.width * left.height)))
        .then(pixel_format_rank(&right.pixel_format).cmp(&pixel_format_rank(&left.pixel_format)))
        .then(right.pixel_format.cmp(&left.pixel_format))
}

fn mode_rank(mode: &CameraMode) -> u8 {
    match (mode.width, mode.height) {
        (DEFAULT_CAMERA_WIDTH, DEFAULT_CAMERA_HEIGHT) => 5,
        (1920, 1080) => 4,
        (1024, 768) => 3,
        (800, 600) => 2,
        _ => 1,
    }
}

fn pixel_format_rank(pixel_format: &str) -> u8 {
    match pixel_format {
        "MJPG" => 3,
        "YUYV" | "YUY2" => 2,
        _ => 1,
    }
}

fn start_preview_worker(
    shared: Arc<SharedApp>,
    app: AppHandle,
    selected: SelectedCamera,
) -> PreviewWorker {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_signal = stop.clone();
    let latest_frame = Arc::new(Mutex::new(None));
    let latest_frame_for_thread = latest_frame.clone();
    let handle = thread::spawn(move || {
        let result = || -> Result<()> {
            let fourcc = parse_fourcc(&selected.mode.pixel_format)?;
            let device = Device::with_path(&selected.device_id)
                .with_context(|| format!("failed to open {}", selected.device_id))?;
            let requested = Format::new(selected.mode.width, selected.mode.height, fourcc);
            let actual = device
                .set_format(&requested)
                .context("failed to set camera format")?;
            let mut stream = MmapStream::with_buffers(&device, Type::VideoCapture, 4)
                .context("failed to start camera stream")?;
            let preview_is_passthrough =
                matches!(actual.fourcc.str().unwrap_or_default(), "MJPG" | "JPEG");
            let mut last_emit = Instant::now() - Duration::from_millis(PREVIEW_INTERVAL_MS);

            while !stop_signal.load(Ordering::Relaxed) {
                let (buffer, meta) = stream.next().context("failed to read preview frame")?;
                let used = meta.bytesused as usize;
                if used == 0 || used > buffer.len() {
                    continue;
                }

                let bytes = buffer[..used].to_vec();
                {
                    let mut latest = latest_frame_for_thread.lock().unwrap();
                    *latest = Some(LatestFrame {
                        bytes: bytes.clone(),
                        width: actual.width,
                        height: actual.height,
                        fourcc: actual.fourcc,
                    });
                }

                if preview_is_passthrough {
                    shared.media_server.set_preview_jpeg(bytes);
                    continue;
                }

                if last_emit.elapsed() >= Duration::from_millis(PREVIEW_INTERVAL_MS) {
                    last_emit = Instant::now();
                    let image = decode_frame(&bytes, actual.width, actual.height, &actual.fourcc)?;
                    let preview = image.resize(PREVIEW_WIDTH, PREVIEW_HEIGHT, FilterType::Triangle);
                    let jpeg = encode_jpeg(&preview, PREVIEW_JPEG_QUALITY)?;
                    shared.media_server.set_preview_jpeg(jpeg);
                }
            }

            Ok(())
        }();

        if let Err(error) = result {
            shared.media_server.clear_preview();
            shared.set_last_result(&app, false, error_to_string(error));
        }
    });
    PreviewWorker {
        stop,
        handle: Some(handle),
        latest_frame,
    }
}

fn stop_preview_locked(shared: &SharedApp, inner: &mut AppInner) {
    if let Some(mut worker) = inner.preview.take() {
        worker.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = worker.handle.take() {
            let _ = handle.join();
        }
    }
    shared.media_server.clear_preview();
}

fn normalize_to_resolution(image: &DynamicImage, resolution: &ProjectResolution) -> DynamicImage {
    let resized = image.resize(resolution.width, resolution.height, FilterType::Lanczos3);
    let mut canvas = RgbaImage::from_pixel(
        resolution.width,
        resolution.height,
        Rgba([0_u8, 0_u8, 0_u8, 255_u8]),
    );

    let x = ((resolution.width - resized.width()) / 2) as i64;
    let y = ((resolution.height - resized.height()) / 2) as i64;
    overlay(&mut canvas, &resized.to_rgba8(), x, y);
    DynamicImage::ImageRgba8(canvas)
}

fn decode_frame(buffer: &[u8], width: u32, height: u32, fourcc: &FourCC) -> Result<DynamicImage> {
    match fourcc.str().unwrap_or_default() {
        "MJPG" | "JPEG" => image::load_from_memory(buffer).context("failed to decode MJPEG frame"),
        "YUYV" | "YUY2" => decode_yuyv(buffer, width, height),
        other => Err(anyhow!("Unsupported camera pixel format: {other}")),
    }
}

fn decode_yuyv(buffer: &[u8], width: u32, height: u32) -> Result<DynamicImage> {
    let expected = (width * height * 2) as usize;
    if buffer.len() < expected {
        return Err(anyhow!("YUYV frame is shorter than expected."));
    }

    let mut image = ImageBuffer::new(width, height);
    let mut index = 0;
    for y in 0..height {
        for x in (0..width).step_by(2) {
            let y0 = buffer[index] as f32;
            let u = buffer[index + 1] as f32 - 128.0;
            let y1 = buffer[index + 2] as f32;
            let v = buffer[index + 3] as f32 - 128.0;

            image.put_pixel(x, y, yuv_to_rgba(y0, u, v));
            if x + 1 < width {
                image.put_pixel(x + 1, y, yuv_to_rgba(y1, u, v));
            }
            index += 4;
        }
    }

    Ok(DynamicImage::ImageRgba8(image))
}

fn yuv_to_rgba(y: f32, u: f32, v: f32) -> Rgba<u8> {
    let r = clamp_color(y + 1.402 * v);
    let g = clamp_color(y - 0.344_136 * u - 0.714_136 * v);
    let b = clamp_color(y + 1.772 * u);
    Rgba([r, g, b, 255])
}

fn clamp_color(value: f32) -> u8 {
    value.clamp(0.0, 255.0) as u8
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let rgb = image.to_rgb8();
    let mut cursor = Cursor::new(Vec::new());
    let mut encoder = JpegEncoder::new_with_quality(&mut cursor, quality);
    encoder.encode(
        &rgb,
        rgb.width(),
        rgb.height(),
        image::ColorType::Rgb8.into(),
    )?;
    Ok(cursor.into_inner())
}

fn write_jpeg(path: &Path, image: &DynamicImage, quality: u8) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = encode_jpeg(image, quality)?;
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn build_snapshot(inner: &AppInner, media_base_url: Option<&str>) -> Result<AppSnapshot> {
    let project = if let Some(session) = inner.session.as_ref() {
        Some(ProjectView {
            project_path: session.project_path.to_string_lossy().into_owned(),
            fps: session.manifest.fps,
            resolution: session.manifest.resolution.clone(),
            frames: session
                .manifest
                .frames
                .iter()
                .map(|frame| FrameView {
                    id: frame.id,
                    image_url: media_frame_url(media_base_url, frame.id, false),
                    thumb_url: media_frame_url(media_base_url, frame.id, true),
                })
                .collect(),
        })
    } else {
        None
    };

    Ok(AppSnapshot {
        project,
        status: build_status(inner),
        selected_camera: inner.selected_camera.as_ref().map(selected_camera_view),
        ffmpeg_available: ffmpeg_is_available(),
        preview_url: inner
            .selected_camera
            .as_ref()
            .and_then(|_| media_base_url.map(|base_url| format!("{base_url}/preview.mjpg"))),
        preview_still_url: inner
            .selected_camera
            .as_ref()
            .and_then(|_| media_base_url.map(|base_url| format!("{base_url}/preview.jpg"))),
    })
}

fn build_status(inner: &AppInner) -> OperationStatusView {
    let phase = if inner.close_requested {
        "cleaning-up"
    } else if inner.busy_detail.is_some() {
        "busy"
    } else {
        "idle"
    };

    let detail = if inner.close_requested {
        Some("Cleaning up...".to_string())
    } else {
        inner.busy_detail.clone()
    };

    OperationStatusView {
        busy: inner.busy_detail.is_some() || inner.close_requested,
        phase: phase.to_string(),
        detail,
        last_result: inner.last_result.clone(),
    }
}

fn selected_camera_view(selected: &SelectedCamera) -> SelectedCameraView {
    SelectedCameraView {
        device_id: selected.device_id.clone(),
        label: selected.label.clone(),
        mode: selected.mode.clone(),
    }
}

fn media_frame_url(media_base_url: Option<&str>, frame_id: u64, thumb: bool) -> String {
    let Some(base_url) = media_base_url else {
        return String::new();
    };
    let kind = if thumb { "thumb" } else { "image" };
    format!("{base_url}/frames/{frame_id}/{kind}.jpg")
}

fn serve_media_client(
    mut stream: TcpStream,
    shared: Weak<SharedApp>,
    preview_state: Arc<PreviewBroadcast>,
) -> Result<()> {
    let mut request_line = String::new();
    {
        let mut reader = BufReader::new(stream.try_clone()?);
        reader.read_line(&mut request_line)?;
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/");

    if path == "/preview.mjpg" {
        return serve_preview_stream(stream, preview_state);
    }

    if path == "/preview.jpg" {
        return serve_preview_still(&mut stream, preview_state);
    }

    if let Some((frame_id, thumb)) = parse_frame_request(path) {
        let Some(shared) = shared.upgrade() else {
            return write_http_error(&mut stream, 503, "Service unavailable");
        };
        return serve_frame_asset(&mut stream, &shared, frame_id, thumb);
    }

    write_http_error(&mut stream, 404, "Not found")
}

fn parse_frame_request(path: &str) -> Option<(u64, bool)> {
    let mut parts = path.trim_matches('/').split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("frames"), Some(frame_id), Some("thumb.jpg"), None) => {
            frame_id.parse().ok().map(|frame_id| (frame_id, true))
        }
        (Some("frames"), Some(frame_id), Some("image.jpg"), None) => {
            frame_id.parse().ok().map(|frame_id| (frame_id, false))
        }
        _ => None,
    }
}

fn serve_frame_asset(
    stream: &mut TcpStream,
    shared: &SharedApp,
    frame_id: u64,
    thumb: bool,
) -> Result<()> {
    let asset_path = {
        let inner = shared.inner.lock().unwrap();
        let session = inner
            .session
            .as_ref()
            .ok_or_else(|| anyhow!("No project open."))?;
        let frame = session
            .manifest
            .frames
            .iter()
            .find(|frame| frame.id == frame_id)
            .ok_or_else(|| anyhow!("Frame not found."))?;
        let relative = if thumb {
            &frame.thumb_path
        } else {
            &frame.image_path
        };
        session.workspace.path().join(relative)
    };

    let bytes = fs::read(&asset_path)
        .with_context(|| format!("failed to read {}", asset_path.display()))?;
    write_http_bytes(stream, "200 OK", "image/jpeg", &bytes)
}

fn serve_preview_stream(mut stream: TcpStream, preview_state: Arc<PreviewBroadcast>) -> Result<()> {
    write!(
        stream,
        concat!(
            "HTTP/1.1 200 OK\r\n",
            "Cache-Control: no-store, no-cache, must-revalidate, max-age=0\r\n",
            "Pragma: no-cache\r\n",
            "Connection: close\r\n",
            "Content-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n"
        )
    )?;
    stream.flush()?;

    let mut last_version = 0_u64;
    loop {
        let jpeg = {
            let mut state = preview_state.state.lock().unwrap();
            while state.jpeg.is_none() || state.version == last_version {
                state = preview_state.ready.wait(state).unwrap();
            }
            last_version = state.version;
            state.jpeg.clone().unwrap_or_default()
        };

        write!(
            stream,
            concat!(
                "--frame\r\n",
                "Content-Type: image/jpeg\r\n",
                "Content-Length: {}\r\n\r\n"
            ),
            jpeg.len()
        )?;
        stream.write_all(&jpeg)?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
    }
}

fn serve_preview_still(stream: &mut TcpStream, preview_state: Arc<PreviewBroadcast>) -> Result<()> {
    let jpeg = {
        let state = preview_state.state.lock().unwrap();
        state.jpeg.clone()
    };

    let Some(jpeg) = jpeg else {
        return write_http_error(stream, 503, "Preview not ready");
    };

    write_http_bytes(stream, "200 OK", "image/jpeg", &jpeg)
}

fn write_http_bytes(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<()> {
    write!(
        stream,
        concat!(
            "HTTP/1.1 {status}\r\n",
            "Cache-Control: no-store, no-cache, must-revalidate, max-age=0\r\n",
            "Pragma: no-cache\r\n",
            "Connection: close\r\n",
            "Content-Type: {content_type}\r\n",
            "Content-Length: {content_length}\r\n\r\n"
        ),
        status = status,
        content_type = content_type,
        content_length = bytes.len()
    )?;
    stream.write_all(bytes)?;
    stream.flush()?;
    Ok(())
}

fn write_http_error(stream: &mut TcpStream, code: u16, message: &str) -> Result<()> {
    let body = format!("{message}\n");
    write_http_bytes(
        stream,
        &format!("{code} {message}"),
        "text/plain; charset=utf-8",
        body.as_bytes(),
    )
}

fn ensure_workspace_dirs(root: &Path) -> Result<()> {
    fs::create_dir_all(root.join("frames"))?;
    fs::create_dir_all(root.join("thumbs"))?;
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(anyhow!("absolute paths are not allowed in project assets"));
    }

    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(anyhow!("project asset path contains invalid traversal"));
        }
    }

    Ok(())
}

fn remove_if_exists(path: PathBuf) -> Result<()> {
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn normalize_project_path(path: &Path) -> PathBuf {
    if path.extension().and_then(|ext| ext.to_str()) == Some("jind") {
        path.to_path_buf()
    } else {
        path.with_extension("jind")
    }
}

fn normalize_export_path(path: &Path) -> PathBuf {
    if path.extension().and_then(|ext| ext.to_str()) == Some("mp4") {
        path.to_path_buf()
    } else {
        path.with_extension("mp4")
    }
}

fn rel_path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn camera_mode_id(path: &Path, fourcc: FourCC, width: u32, height: u32) -> String {
    format!(
        "{}|{}|{}x{}",
        path.display(),
        fourcc.str().unwrap_or("UNKN"),
        width,
        height
    )
}

fn parse_fourcc(pixel_format: &str) -> Result<FourCC> {
    let bytes = pixel_format.as_bytes();
    if bytes.len() != 4 {
        return Err(anyhow!("Invalid camera pixel format: {pixel_format}"));
    }
    let repr: [u8; 4] = bytes.try_into().map_err(|_| anyhow!("Invalid fourcc"))?;
    Ok(FourCC::new(&repr))
}

fn ensure_ffmpeg_available() -> Result<()> {
    if ffmpeg_is_available() {
        Ok(())
    } else {
        Err(anyhow!(
            "`ffmpeg` was not found on PATH. Export is unavailable."
        ))
    }
}

fn ffmpeg_is_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn spawn_shutdown_if_requested(shared: Arc<SharedApp>, app: AppHandle) {
    if !shared.should_start_shutdown() {
        return;
    }

    shared.emit_status(&app);
    tauri::async_runtime::spawn(async move {
        let _guard = shared.operation_lock.lock().await;
        let _ = tauri::async_runtime::spawn_blocking({
            let shared = shared.clone();
            move || cleanup_for_shutdown(shared)
        })
        .await;
        app.exit(0);
    });
}

fn cleanup_for_shutdown(shared: Arc<SharedApp>) -> Result<()> {
    let mut inner = shared.inner.lock().unwrap();
    stop_preview_locked(shared.as_ref(), &mut inner);
    inner.selected_camera = None;
    inner.session = None;
    inner.busy_detail = None;
    Ok(())
}

fn error_to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(SharedState::new())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            pick_new_project_path,
            pick_open_project_path,
            pick_export_path,
            get_project_state,
            list_cameras,
            create_project,
            open_project,
            select_camera,
            capture_frame,
            delete_frame,
            reorder_frames,
            export_mp4,
            reveal_export_in_folder
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let state = window.state::<SharedState>().app.clone();
                state.mark_close_requested(&window.app_handle());
                if !state.is_busy() {
                    spawn_shutdown_if_requested(state, window.app_handle().clone());
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_letterboxes_to_target_resolution() {
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(1600, 900, Rgba([255, 0, 0, 255])));
        let result = normalize_to_resolution(
            &image,
            &ProjectResolution {
                width: 800,
                height: 800,
            },
        );

        assert_eq!((result.width(), result.height()), (800, 800));
    }

    #[test]
    fn archive_round_trip_preserves_manifest() {
        let workspace = tempdir().unwrap();
        ensure_workspace_dirs(workspace.path()).unwrap();
        let frame_path = workspace.path().join("frames/frame-000001.jpg");
        let thumb_path = workspace.path().join("thumbs/frame-000001.jpg");
        let pixel =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(32, 32, Rgba([0, 128, 255, 255])));
        write_jpeg(&frame_path, &pixel, 90).unwrap();
        write_jpeg(&thumb_path, &pixel, 80).unwrap();

        let archive_dir = tempdir().unwrap();
        let project_path = archive_dir.path().join("sample.jind");
        let session = ProjectSession {
            project_path: project_path.clone(),
            workspace,
            manifest: ProjectManifest {
                version: APP_VERSION,
                fps: PROJECT_FPS,
                resolution: Some(ProjectResolution {
                    width: 32,
                    height: 32,
                }),
                frames: vec![FrameEntry {
                    id: 1,
                    image_path: "frames/frame-000001.jpg".to_string(),
                    thumb_path: "thumbs/frame-000001.jpg".to_string(),
                }],
            },
            next_frame_id: 2,
        };

        write_manifest_to_workspace(&session).unwrap();
        write_archive(&session).unwrap();
        let reopened = unpack_project(&project_path).unwrap();

        assert_eq!(reopened.manifest.frames.len(), 1);
        assert_eq!(reopened.manifest.fps, PROJECT_FPS);
    }
}
