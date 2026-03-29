use gloo::timers::callback::Interval;
use js_sys::Reflect;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{window, DragEvent, HtmlElement, HtmlSelectElement};
use yew::prelude::*;

const STATUS_EVENT: &str = "jind://status";

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    async fn invoke_js(command: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "event"], js_name = listen)]
    async fn listen_js(event: &str, handler: &js_sys::Function) -> Result<JsValue, JsValue>;
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct AppSnapshot {
    project: Option<ProjectView>,
    status: OperationStatusView,
    selected_camera: Option<SelectedCameraView>,
    ffmpeg_available: bool,
    preview_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct ProjectView {
    project_path: String,
    fps: u32,
    resolution: Option<ProjectResolution>,
    frames: Vec<FrameView>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct ProjectResolution {
    width: u32,
    height: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct FrameView {
    id: u64,
    image_url: String,
    thumb_url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct OperationStatusView {
    busy: bool,
    phase: String,
    detail: Option<String>,
    last_result: Option<LastOperationResult>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct LastOperationResult {
    ok: bool,
    message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct CameraDeviceView {
    device_id: String,
    label: String,
    modes: Vec<CameraMode>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct CameraMode {
    id: String,
    label: String,
    width: u32,
    height: u32,
    pixel_format: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct SelectedCameraView {
    device_id: String,
    label: String,
    mode: CameraMode,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PathArg {
    path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SelectCameraArg {
    device_id: String,
    mode_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteFrameArg {
    frame_id: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReorderFramesArg {
    frame_ids_in_order: Vec<u64>,
}

#[derive(Serialize, Default)]
struct EmptyArgs {}

#[function_component(App)]
pub fn app() -> Html {
    let snapshot = use_state(AppSnapshot::default);
    let project_cache = use_state(|| None::<ProjectView>);
    let cameras = use_state(Vec::<CameraDeviceView>::new);
    let selected_frame_id = use_state(|| None::<u64>);
    let dragged_frame_id = use_state(|| None::<u64>);
    let live_preview_enabled = use_state(|| true);
    let onion_skin_enabled = use_state(|| true);
    let playing = use_state(|| false);
    let playback_cursor = use_mut_ref(|| 0_usize);
    let capture_feedback_frame_id = use_state(|| None::<u64>);

    {
        let snapshot = snapshot.clone();
        let project_cache = project_cache.clone();
        let cameras = cameras.clone();
        let selected_frame_id = selected_frame_id.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                if let Ok(current_snapshot) =
                    invoke_command::<AppSnapshot, _>("get_project_state", &EmptyArgs {}).await
                {
                    sync_snapshot(
                        snapshot.clone(),
                        project_cache.clone(),
                        selected_frame_id.clone(),
                        current_snapshot,
                    );
                }

                if let Ok(available_cameras) =
                    invoke_command::<Vec<CameraDeviceView>, _>("list_cameras", &EmptyArgs {}).await
                {
                    let should_autoselect =
                        snapshot.project.is_some() && snapshot.selected_camera.is_none();
                    cameras.set(available_cameras);
                    if should_autoselect {
                        maybe_autoselect_camera(
                            snapshot.clone(),
                            project_cache.clone(),
                            selected_frame_id.clone(),
                            &cameras,
                        )
                        .await;
                    }
                }

                attach_listener::<OperationStatusView, _>(STATUS_EVENT, {
                    let snapshot = snapshot.clone();
                    move |status| {
                        let mut next = (*snapshot).clone();
                        next.status = status;
                        snapshot.set(next);
                    }
                })
                .await;
            });

            || {}
        });
    }

    {
        let snapshot = snapshot.clone();
        let project_cache = project_cache.clone();
        let selected_frame_id = selected_frame_id.clone();
        let cameras = cameras.clone();
        use_effect_with(
            ((*snapshot).project.is_some(), (*snapshot).selected_camera.clone(), (*cameras).clone()),
            move |(has_project, selected_camera, available_cameras)| {
                if *has_project && selected_camera.is_none() {
                    if let Some(first_device) = available_cameras.first() {
                        if let Some(first_mode) = first_device.modes.first() {
                            let device_id = first_device.device_id.clone();
                            let mode_id = first_mode.id.clone();
                            let snapshot = snapshot.clone();
                            let project_cache = project_cache.clone();
                            let selected_frame_id = selected_frame_id.clone();
                            spawn_local(async move {
                                if let Ok(next_snapshot) = invoke_command::<AppSnapshot, _>(
                                    "select_camera",
                                    &SelectCameraArg { device_id, mode_id },
                                )
                                .await
                                {
                                    sync_snapshot(snapshot, project_cache, selected_frame_id, next_snapshot);
                                }
                            });
                        }
                    }
                }
                || {}
            },
        );
    }

    {
        let playing = playing.clone();
        let selected_frame_id = selected_frame_id.clone();
        let playback_cursor = playback_cursor.clone();
        let frame_ids = snapshot
            .project
            .as_ref()
            .or_else(|| (*project_cache).as_ref())
            .map(|project| project.frames.iter().map(|frame| frame.id).collect::<Vec<_>>())
            .unwrap_or_default();
        use_effect_with(((*playing), frame_ids.clone()), move |(is_playing, frame_ids)| {
            let total_frames = frame_ids.len();
            let interval = if !*is_playing || total_frames == 0 {
                None
            } else {
                let selected_frame_id = selected_frame_id.clone();
                let frame_ids = frame_ids.clone();
                let playback_cursor = playback_cursor.clone();
                Some(Interval::new(1000 / 15, move || {
                    let next_index = {
                        let mut cursor = playback_cursor.borrow_mut();
                        *cursor = (*cursor + 1) % total_frames.max(1);
                        *cursor
                    };
                    if let Some(frame_id) = frame_ids.get(next_index) {
                        selected_frame_id.set(Some(*frame_id));
                    }
                }))
            };

            move || drop(interval)
        });
    }

    let busy = snapshot.status.busy;
    let current_project = snapshot
        .project
        .clone()
        .or_else(|| (*project_cache).clone());
    let has_project = current_project.is_some();
    let current_device_id = snapshot
        .selected_camera
        .as_ref()
        .map(|selected| selected.device_id.clone())
        .or_else(|| cameras.first().map(|device| device.device_id.clone()))
        .unwrap_or_default();
    let current_mode_id = snapshot
        .selected_camera
        .as_ref()
        .map(|selected| selected.mode.id.clone())
        .or_else(|| {
            cameras
                .iter()
                .find(|device| device.device_id == current_device_id)
                .and_then(|device| device.modes.first().map(|mode| mode.id.clone()))
        })
        .unwrap_or_default();
    let current_modes = cameras
        .iter()
        .find(|device| device.device_id == current_device_id)
        .map(|device| device.modes.clone())
        .unwrap_or_default();

    let selected_frame = current_project.as_ref().and_then(|project| {
        selected_frame_id
            .as_ref()
            .and_then(|selected| project.frames.iter().find(|frame| frame.id == *selected))
            .or_else(|| project.frames.first())
    });
    let active_selected_frame = selected_frame;
    let active_selected_frame_id = selected_frame.map(|frame| frame.id);
    let delete_target_frame_id = if *live_preview_enabled || *playing {
        None
    } else {
        active_selected_frame_id
    };
    let capture_feedback_frame = current_project.as_ref().and_then(|project| {
        capture_feedback_frame_id
            .as_ref()
            .and_then(|selected| project.frames.iter().find(|frame| frame.id == *selected))
    });
    let live_onion_frame = current_project
        .as_ref()
        .and_then(|project| project.frames.last());
    let preview_stream_url = snapshot
        .preview_url
        .clone()
        .filter(|_| snapshot.selected_camera.is_some() && *live_preview_enabled && !*playing)
        .map(|url| format!("{url}?device={current_device_id}&mode={current_mode_id}"));

    {
        use_effect_with((active_selected_frame_id, *live_preview_enabled, *playing), move |(frame_id, live, playing)| {
            let target_id = if *live && !*playing {
                Some("timeline-live-proxy".to_string())
            } else {
                frame_id.map(|frame_id| format!("timeline-frame-{frame_id}"))
            };

            if let Some(target_id) = target_id {
                scroll_timeline_item_into_view(&target_id);
            }
            || {}
        });
    }

    let status_line = snapshot
        .status
        .detail
        .clone()
        .or_else(|| snapshot.status.last_result.as_ref().map(|last| last.message.clone()))
        .unwrap_or_else(|| {
            if has_project {
                "Ready.".to_string()
            } else {
                "Create or open a `.jind` project to begin.".to_string()
            }
        });

    let on_create_project = {
        let snapshot = snapshot.clone();
        let project_cache = project_cache.clone();
        let cameras = cameras.clone();
        let live_preview_enabled = live_preview_enabled.clone();
        let selected_frame_id = selected_frame_id.clone();
        Callback::from(move |_| {
            let snapshot = snapshot.clone();
            let project_cache = project_cache.clone();
            let cameras = cameras.clone();
            let live_preview_enabled = live_preview_enabled.clone();
            let selected_frame_id = selected_frame_id.clone();
            spawn_local(async move {
                if let Ok(Some(path)) =
                    invoke_command::<Option<String>, _>("pick_new_project_path", &EmptyArgs {}).await
                {
                    let mut next_snapshot = if let Ok(next_snapshot) =
                        invoke_command::<AppSnapshot, _>("create_project", &PathArg { path }).await
                    {
                        next_snapshot
                    } else {
                        return;
                    };

                    if let Ok(available_cameras) =
                        invoke_command::<Vec<CameraDeviceView>, _>("list_cameras", &EmptyArgs {}).await
                    {
                        cameras.set(available_cameras.clone());
                        if next_snapshot.selected_camera.is_none() {
                            if let Some(selection) = first_camera_selection(&available_cameras) {
                                if let Ok(camera_snapshot) =
                                    invoke_command::<AppSnapshot, _>("select_camera", &selection).await
                                {
                                    next_snapshot = camera_snapshot;
                                }
                            }
                        }
                    }

                    live_preview_enabled.set(true);
                    sync_snapshot(snapshot, project_cache, selected_frame_id, next_snapshot);
                }
            });
        })
    };

    let on_open_project = {
        let snapshot = snapshot.clone();
        let project_cache = project_cache.clone();
        let cameras = cameras.clone();
        let live_preview_enabled = live_preview_enabled.clone();
        let selected_frame_id = selected_frame_id.clone();
        Callback::from(move |_| {
            let snapshot = snapshot.clone();
            let project_cache = project_cache.clone();
            let cameras = cameras.clone();
            let live_preview_enabled = live_preview_enabled.clone();
            let selected_frame_id = selected_frame_id.clone();
            spawn_local(async move {
                if let Ok(Some(path)) =
                    invoke_command::<Option<String>, _>("pick_open_project_path", &EmptyArgs {}).await
                {
                    let mut next_snapshot = if let Ok(next_snapshot) =
                        invoke_command::<AppSnapshot, _>("open_project", &PathArg { path }).await
                    {
                        next_snapshot
                    } else {
                        return;
                    };

                    if let Ok(available_cameras) =
                        invoke_command::<Vec<CameraDeviceView>, _>("list_cameras", &EmptyArgs {}).await
                    {
                        cameras.set(available_cameras.clone());
                        if next_snapshot.selected_camera.is_none() {
                            if let Some(selection) = first_camera_selection(&available_cameras) {
                                if let Ok(camera_snapshot) =
                                    invoke_command::<AppSnapshot, _>("select_camera", &selection).await
                                {
                                    next_snapshot = camera_snapshot;
                                }
                            }
                        }
                    }

                    live_preview_enabled.set(true);
                    sync_snapshot(snapshot, project_cache, selected_frame_id, next_snapshot);
                }
            });
        })
    };

    let on_capture = {
        let current_project_for_capture = current_project.clone();
        let snapshot = snapshot.clone();
        let project_cache = project_cache.clone();
        let live_preview_enabled = live_preview_enabled.clone();
        let playing = playing.clone();
        let selected_frame_id = selected_frame_id.clone();
        let playback_cursor = playback_cursor.clone();
        let capture_feedback_frame_id = capture_feedback_frame_id.clone();
        Callback::from(move |_| {
            let snapshot = snapshot.clone();
            let project_cache = project_cache.clone();
            let live_preview_enabled = live_preview_enabled.clone();
            let playing = playing.clone();
            let selected_frame_id = selected_frame_id.clone();
            let playback_cursor = playback_cursor.clone();
            let capture_feedback_frame_id = capture_feedback_frame_id.clone();
            playing.set(false);
            capture_feedback_frame_id.set(
                current_project_for_capture
                    .as_ref()
                    .and_then(|project| project.frames.last().map(|frame| frame.id)),
            );
            live_preview_enabled.set(false);
            *playback_cursor.borrow_mut() = 0;
            spawn_local(async move {
                if let Ok(next_snapshot) =
                    invoke_command::<AppSnapshot, _>("capture_frame", &EmptyArgs {}).await
                {
                    playing.set(false);
                    live_preview_enabled.set(true);
                    capture_feedback_frame_id.set(None);
                    sync_snapshot(snapshot, project_cache, selected_frame_id, next_snapshot);
                } else {
                    capture_feedback_frame_id.set(None);
                }
            });
        })
    };

    let on_delete = {
        let current_project = current_project.clone();
        let live_preview_enabled = live_preview_enabled.clone();
        let playing = playing.clone();
        let snapshot = snapshot.clone();
        let project_cache = project_cache.clone();
        let selected_frame_id = selected_frame_id.clone();
        Callback::from(move |_| {
            if *live_preview_enabled || *playing {
                return;
            }

            let frame_id = current_project
                .as_ref()
                .and_then(|project| {
                    selected_frame_id
                        .as_ref()
                        .and_then(|selected| project.frames.iter().find(|frame| frame.id == *selected))
                        .or_else(|| project.frames.first())
                })
                .map(|frame| frame.id);
            let next_selected_after_delete = current_project.as_ref().and_then(|project| {
                let index = project.frames.iter().position(|frame| Some(frame.id) == frame_id)?;
                project
                    .frames
                    .get(index + 1)
                    .or_else(|| index.checked_sub(1).and_then(|left| project.frames.get(left)))
                    .map(|frame| frame.id)
            });

            let snapshot = snapshot.clone();
            let project_cache = project_cache.clone();
            let selected_frame_id = selected_frame_id.clone();
            let playing = playing.clone();
            if let Some(frame_id) = frame_id {
                spawn_local(async move {
                    match invoke_command::<AppSnapshot, _>("delete_frame", &DeleteFrameArg { frame_id }).await {
                        Ok(next_snapshot) => {
                            playing.set(false);
                            sync_snapshot(
                                snapshot,
                                project_cache,
                                selected_frame_id.clone(),
                                next_snapshot,
                            );
                            selected_frame_id.set(next_selected_after_delete);
                        }
                        Err(error) => {
                            set_command_error(snapshot, error);
                        }
                    }
                });
            }
        })
    };

    let on_export = {
        let snapshot = snapshot.clone();
        let project_cache = project_cache.clone();
        let selected_frame_id = selected_frame_id.clone();
        Callback::from(move |_| {
            let snapshot = snapshot.clone();
            let project_cache = project_cache.clone();
            let selected_frame_id = selected_frame_id.clone();
            spawn_local(async move {
                if let Ok(Some(path)) =
                    invoke_command::<Option<String>, _>("pick_export_path", &EmptyArgs {}).await
                {
                    if let Ok(next_snapshot) =
                        invoke_command::<AppSnapshot, _>("export_mp4", &PathArg { path }).await
                    {
                        sync_snapshot(snapshot, project_cache, selected_frame_id, next_snapshot);
                    }
                }
            });
        })
    };

    let on_toggle_playback = {
        let playing = playing.clone();
        let live_preview_enabled = live_preview_enabled.clone();
        let current_project = current_project.clone();
        let selected_frame_id = selected_frame_id.clone();
        let playback_cursor = playback_cursor.clone();
        Callback::from(move |_| {
            if *playing {
                playing.set(false);
            } else {
                let start_index = current_project
                    .as_ref()
                    .and_then(|project| {
                        selected_frame_id
                            .as_ref()
                            .and_then(|selected| project.frames.iter().position(|frame| frame.id == *selected))
                            .or(Some(0))
                    })
                    .unwrap_or(0);
                *playback_cursor.borrow_mut() = start_index;
                if let Some(frame_id) = current_project
                    .as_ref()
                    .and_then(|project| project.frames.get(start_index))
                    .map(|frame| frame.id)
                {
                    selected_frame_id.set(Some(frame_id));
                }
                live_preview_enabled.set(false);
                playing.set(true);
            }
        })
    };

    let on_device_change = {
        let snapshot = snapshot.clone();
        let cameras = cameras.clone();
        let project_cache = project_cache.clone();
        let live_preview_enabled = live_preview_enabled.clone();
        let playing = playing.clone();
        let selected_frame_id = selected_frame_id.clone();
        Callback::from(move |event: Event| {
            let Some(select) = event.target_dyn_into::<HtmlSelectElement>() else {
                return;
            };
            let device_id = select.value();
            let snapshot = snapshot.clone();
            let project_cache = project_cache.clone();
            let live_preview_enabled = live_preview_enabled.clone();
            let playing = playing.clone();
            let selected_frame_id = selected_frame_id.clone();
            let first_mode = cameras
                .iter()
                .find(|device| device.device_id == device_id)
                .and_then(|device| device.modes.first().cloned());
            if let Some(mode) = first_mode {
                spawn_local(async move {
                    playing.set(false);
                    live_preview_enabled.set(true);
                    if let Ok(next_snapshot) = invoke_command::<AppSnapshot, _>(
                        "select_camera",
                        &SelectCameraArg {
                            device_id,
                            mode_id: mode.id,
                        },
                    )
                    .await
                    {
                        sync_snapshot(snapshot, project_cache, selected_frame_id, next_snapshot);
                    }
                });
            }
        })
    };

    let on_mode_change = {
        let snapshot = snapshot.clone();
        let project_cache = project_cache.clone();
        let live_preview_enabled = live_preview_enabled.clone();
        let playing = playing.clone();
        let selected_frame_id = selected_frame_id.clone();
        let current_device_id = current_device_id.clone();
        Callback::from(move |event: Event| {
            let Some(select) = event.target_dyn_into::<HtmlSelectElement>() else {
                return;
            };
            let mode_id = select.value();
            let snapshot = snapshot.clone();
            let project_cache = project_cache.clone();
            let live_preview_enabled = live_preview_enabled.clone();
            let playing = playing.clone();
            let selected_frame_id = selected_frame_id.clone();
            let device_id = current_device_id.clone();
            spawn_local(async move {
                playing.set(false);
                live_preview_enabled.set(true);
                if let Ok(next_snapshot) = invoke_command::<AppSnapshot, _>(
                    "select_camera",
                    &SelectCameraArg { device_id, mode_id },
                )
                .await
                {
                    sync_snapshot(snapshot, project_cache, selected_frame_id, next_snapshot);
                }
            });
        })
    };

    html! {
        <main class="app-shell">
            <section class="status-strip">
                <div class="status-pill">
                    <span class="status-dot"></span>
                    <strong>{status_phase_label(&snapshot.status)}</strong>
                    <span>{status_line}</span>
                </div>
                {
                    if let Some(project) = current_project.as_ref() {
                        html! {
                            <div class="project-meta">
                                <span>{project.project_path.clone()}</span>
                                <span>{format!("{} FPS", project.fps)}</span>
                                <span>{project.resolution.as_ref().map(|resolution| format!("{}x{}", resolution.width, resolution.height)).unwrap_or_else(|| "Resolution unlocks on first capture".to_string())}</span>
                            </div>
                        }
                    } else {
                        html! { <div class="project-meta"><span>{"No project open"}</span></div> }
                    }
                }
            </section>

            {
                if has_project {
                    html! {
                        <section class="editor-shell">
                            <aside class="control-panel">
                                <div class="panel-card">
                                    <h2>{"Camera"}</h2>
                                    {
                                        if cameras.is_empty() {
                                            html! { <p class="muted">{"No usable `/dev/video*` camera was found."}</p> }
                                        } else {
                                            html! {
                                                <>
                                                    <label class="field">
                                                        <span>{"Device"}</span>
                                                        <select value={current_device_id.clone()} onchange={on_device_change} disabled={busy}>
                                                            { for cameras.iter().map(|camera| html! {
                                                                <option value={camera.device_id.clone()}>{camera.label.clone()}</option>
                                                            }) }
                                                        </select>
                                                    </label>
                                                    <label class="field">
                                                        <span>{"Mode"}</span>
                                                        <select value={current_mode_id.clone()} onchange={on_mode_change} disabled={busy}>
                                                            { for current_modes.iter().map(|mode| html! {
                                                                <option value={mode.id.clone()}>{mode.label.clone()}</option>
                                                            }) }
                                                        </select>
                                                    </label>
                                                </>
                                            }
                                        }
                                    }
                                </div>

                                <div class="panel-card">
                                    <h2>{"Actions"}</h2>
                                    <div class="action-grid">
                                        <button onclick={on_capture} disabled={busy || snapshot.selected_camera.is_none()}>{"Capture"}</button>
                                        <button onclick={on_delete} disabled={busy || delete_target_frame_id.is_none()}>{"Delete Selected"}</button>
                                        <button onclick={on_toggle_playback} disabled={current_project.as_ref().map(|project| project.frames.is_empty()).unwrap_or(true)}>
                                            { if *playing { "Stop Playback" } else { "Play" } }
                                        </button>
                                        <button onclick={on_export} disabled={busy || !snapshot.ffmpeg_available || current_project.as_ref().map(|project| project.frames.is_empty()).unwrap_or(true)}>
                                            {"Export MP4"}
                                        </button>
                                    </div>
                                    <label class="toggle">
                                        <input
                                            type="checkbox"
                                            checked={*onion_skin_enabled}
                                            onchange={{
                                                let onion_skin_enabled = onion_skin_enabled.clone();
                                                Callback::from(move |_| onion_skin_enabled.set(!*onion_skin_enabled))
                                            }}
                                        />
                                        <span>{"Onion skin"}</span>
                                    </label>
                                    {
                                        if !snapshot.ffmpeg_available {
                                            html! { <p class="muted">{"`ffmpeg` is not on PATH, so export is disabled."}</p> }
                                        } else {
                                            html! {}
                                        }
                                    }
                                </div>
                            </aside>

                            <section class="workspace">
                                <div class="preview-card">
                                    <div class="preview-stage">
                                        {
                                            if busy {
                                                capture_feedback_frame.map(|frame| html! {
                                                    <img class="preview-image" src={frame.image_url.clone()} alt="Latest frame" />
                                                }).unwrap_or_else(|| {
                                                    if let Some(preview_stream_url) = preview_stream_url.clone() {
                                                        html! {
                                                            <>
                                                                <img class="preview-image" src={preview_stream_url} alt="Live preview" />
                                                                {
                                                                    if *onion_skin_enabled {
                                                                        live_onion_frame.map(|frame| html! {
                                                                            <img class="preview-image onion-layer" src={frame.image_url.clone()} alt="Onion skin frame" />
                                                                        }).unwrap_or_default()
                                                                    } else {
                                                                        html! {}
                                                                    }
                                                                }
                                                            </>
                                                        }
                                                    } else {
                                                        html! { <div class="preview-placeholder">{"Working..."}</div> }
                                                    }
                                                })
                                            } else if let Some(preview_stream_url) = preview_stream_url.clone() {
                                                html! {
                                                    <>
                                                        <img class="preview-image" src={preview_stream_url} alt="Live preview" />
                                                        {
                                                            if *onion_skin_enabled {
                                                                live_onion_frame.map(|frame| html! {
                                                                    <img class="preview-image onion-layer" src={frame.image_url.clone()} alt="Onion skin frame" />
                                                                }).unwrap_or_default()
                                                            } else {
                                                                html! {}
                                                            }
                                                        }
                                                    </>
                                                }
                                            } else if let Some(frame) = active_selected_frame {
                                                html! {
                                                    <img class="preview-image" src={frame.image_url.clone()} alt="Selected frame" />
                                                }
                                            } else {
                                                html! { <div class="preview-placeholder">{"Select a camera and capture your first frame."}</div> }
                                            }
                                        }
                                    </div>
                                </div>

                                <div class="timeline-card">
                                    <div class="timeline-header">
                                        <h2>{"Timeline"}</h2>
                                        <span>{current_project.as_ref().map(|project| format!("{} frames", project.frames.len())).unwrap_or_default()}</span>
                                    </div>
                                    <div class="timeline-strip">
                                        {
                                            current_project.as_ref().map(|project| {
                                                html! {
                                                    <>
                                                        { for project.frames.iter().map(|frame| {
                                                            let frame_id = frame.id;
                                                            let selected_frame_id = selected_frame_id.clone();
                                                            let dragged_frame_id = dragged_frame_id.clone();
                                                            let live_preview_enabled = live_preview_enabled.clone();
                                                            let live_preview_for_click = live_preview_enabled.clone();
                                                            let playing = playing.clone();
                                                            let playing_for_click = playing.clone();
                                                            let snapshot = snapshot.clone();
                                                            let drop_selected_frame = selected_frame_id.clone();
                                                            let project_cache = project_cache.clone();
                                                            let drag_start_state = dragged_frame_id.clone();
                                                            let drag_drop_state = dragged_frame_id.clone();
                                                            let ondragstart = Callback::from(move |_| {
                                                                drag_start_state.set(Some(frame_id));
                                                            });
                                                            let ondragover = Callback::from(move |event: DragEvent| {
                                                                event.prevent_default();
                                                            });
                                                            let ondrop = Callback::from(move |event: DragEvent| {
                                                                event.prevent_default();
                                                                let Some(source_id) = *drag_drop_state else {
                                                                    return;
                                                                };
                                                                if source_id == frame_id || snapshot.status.busy {
                                                                    return;
                                                                }

                                                                let Some(project) = snapshot.project.as_ref() else {
                                                                    return;
                                                                };
                                                                let mut order: Vec<u64> = project.frames.iter().map(|item| item.id).collect();
                                                                let Some(source_index) = order.iter().position(|id| *id == source_id) else {
                                                                    return;
                                                                };
                                                                let Some(target_index) = order.iter().position(|id| *id == frame_id) else {
                                                                    return;
                                                                };
                                                                let moved = order.remove(source_index);
                                                                order.insert(target_index, moved);

                                                                let snapshot = snapshot.clone();
                                                                let drop_selected_frame = drop_selected_frame.clone();
                                                                let project_cache = project_cache.clone();
                                                                spawn_local(async move {
                                                                    if let Ok(next_snapshot) = invoke_command::<AppSnapshot, _>(
                                                                        "reorder_frames",
                                                                        &ReorderFramesArg { frame_ids_in_order: order },
                                                                    )
                                                                    .await
                                                                    {
                                                                        sync_snapshot(
                                                                            snapshot,
                                                                            project_cache.clone(),
                                                                            drop_selected_frame,
                                                                            next_snapshot,
                                                                        );
                                                                    }
                                                                });
                                                            });
                                                            let onclick = Callback::from(move |_| {
                                                                playing_for_click.set(false);
                                                                live_preview_for_click.set(false);
                                                                selected_frame_id.set(Some(frame_id));
                                                            });

                                                            html! {
                                                                <button
                                                                    id={format!("timeline-frame-{}", frame.id)}
                                                                    class={classes!(
                                                                        "timeline-frame",
                                                                        (!*live_preview_enabled && active_selected_frame_id == Some(frame.id)).then_some("selected")
                                                                    )}
                                                                    draggable="true"
                                                                    {ondragstart}
                                                                    {ondragover}
                                                                    {ondrop}
                                                                    {onclick}
                                                                    disabled={busy}
                                                                >
                                                                    <img src={frame.thumb_url.clone()} alt="Frame thumbnail" />
                                                                    <span>{format!("#{}", frame.id)}</span>
                                                                </button>
                                                            }
                                                        }) }
                                                        <button
                                                            id="timeline-live-proxy"
                                                            class={classes!("timeline-frame", "timeline-live-proxy", (*live_preview_enabled && !*playing).then_some("selected"))}
                                                            onclick={{
                                                                let live_preview_enabled = live_preview_enabled.clone();
                                                                let playing = playing.clone();
                                                                Callback::from(move |_| {
                                                                    playing.set(false);
                                                                    live_preview_enabled.set(true);
                                                                })
                                                            }}
                                                            disabled={busy || snapshot.selected_camera.is_none()}
                                                        >
                                                            <div class="timeline-live-thumb">{"Live"}</div>
                                                            <span>{"Live Preview"}</span>
                                                        </button>
                                                    </>
                                                }
                                            }).unwrap_or_default()
                                        }
                                    </div>
                                </div>
                            </section>
                        </section>
                    }
                } else {
                    html! {
                        <section class="welcome-shell">
                            <div class="hero-card">
                                <p class="eyebrow">{"Jind Stop Motion"}</p>
                                <h1>{"Build frame-by-frame motion without manual saves."}</h1>
                                <p class="hero-copy">
                                    {"Every capture, delete, and reorder is written to the temp workspace first and then immediately archived back into a single `.jind` file."}
                                </p>
                                <div class="hero-actions">
                                    <button class="primary" onclick={on_create_project} disabled={busy}>{"Create Project"}</button>
                                    <button onclick={on_open_project} disabled={busy}>{"Open Project"}</button>
                                </div>
                            </div>
                        </section>
                    }
                }
            }

            {
                if snapshot.status.phase == "cleaning-up" {
                    html! {
                        <div class="modal-scrim">
                            <div class="modal-card">
                                <h2>{"Cleaning up..."}</h2>
                                <p>{"The current operation is finishing and the workspace is being closed safely."}</p>
                            </div>
                        </div>
                    }
                } else {
                    html! {}
                }
            }
        </main>
    }
}

fn sync_snapshot(
    snapshot: UseStateHandle<AppSnapshot>,
    project_cache: UseStateHandle<Option<ProjectView>>,
    selected_frame_id: UseStateHandle<Option<u64>>,
    next_snapshot: AppSnapshot,
) {
    if let Some(project) = next_snapshot.project.clone() {
        project_cache.set(Some(project));
    }

    let previous_selected = *selected_frame_id;
    let next_selected = next_snapshot.project.as_ref().and_then(|project| {
        previous_selected
            .filter(|selected| project.frames.iter().any(|frame| frame.id == *selected))
    });
    selected_frame_id.set(next_selected);
    snapshot.set(next_snapshot);
}

fn set_command_error(snapshot: UseStateHandle<AppSnapshot>, message: String) {
    let mut next = (*snapshot).clone();
    next.status.busy = false;
    next.status.phase = "idle".to_string();
    next.status.detail = None;
    next.status.last_result = Some(LastOperationResult { ok: false, message });
    snapshot.set(next);
}

async fn attach_listener<T, F>(event_name: &str, callback: F)
where
    T: DeserializeOwned + 'static,
    F: Fn(T) + 'static,
{
    let closure = Closure::<dyn FnMut(JsValue)>::wrap(Box::new(move |event: JsValue| {
        if let Ok(payload) = decode_event_payload::<T>(event) {
            callback(payload);
        }
    }));

    let _ = listen_js(event_name, closure.as_ref().unchecked_ref()).await;
    closure.forget();
}

fn decode_event_payload<T: DeserializeOwned>(event: JsValue) -> Result<T, String> {
    let payload = Reflect::get(&event, &JsValue::from_str("payload")).map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(payload).map_err(|error| error.to_string())
}

async fn invoke_command<T, A>(command: &str, args: &A) -> Result<T, String>
where
    T: DeserializeOwned,
    A: Serialize,
{
    let args = serde_wasm_bindgen::to_value(args).map_err(|error| error.to_string())?;
    let value = invoke_js(command, args).await.map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(value).map_err(|error| error.to_string())
}

fn js_error_to_string(error: JsValue) -> String {
    error
        .as_string()
        .unwrap_or_else(|| format!("{error:?}"))
}

fn scroll_timeline_item_into_view(element_id: &str) {
    let Some(window) = window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    let Some(element) = document.get_element_by_id(element_id) else {
        return;
    };
    let Some(element) = element.dyn_ref::<HtmlElement>() else {
        return;
    };
    let Some(parent) = element.parent_element() else {
        return;
    };
    let Some(parent) = parent.dyn_ref::<HtmlElement>() else {
        return;
    };

    let target_left = element.offset_left();
    let target_width = element.offset_width();
    let container_width = parent.client_width();
    let centered_left = target_left - ((container_width - target_width) / 2);
    parent.set_scroll_left(centered_left.max(0));
}

fn status_phase_label(status: &OperationStatusView) -> &'static str {
    match status.phase.as_str() {
        "cleaning-up" => "Cleaning up",
        "busy" => "Working",
        _ => "Ready",
    }
}

fn first_camera_selection(cameras: &[CameraDeviceView]) -> Option<SelectCameraArg> {
    let device = cameras.first()?;
    let mode = device.modes.first()?;
    Some(SelectCameraArg {
        device_id: device.device_id.clone(),
        mode_id: mode.id.clone(),
    })
}

async fn maybe_autoselect_camera(
    snapshot: UseStateHandle<AppSnapshot>,
    project_cache: UseStateHandle<Option<ProjectView>>,
    selected_frame_id: UseStateHandle<Option<u64>>,
    cameras: &UseStateHandle<Vec<CameraDeviceView>>,
) {
    if snapshot.selected_camera.is_some() {
        return;
    }

    let Some(selection) = first_camera_selection(cameras.as_ref()) else {
        return;
    };

    if let Ok(next_snapshot) = invoke_command::<AppSnapshot, _>("select_camera", &selection).await {
        sync_snapshot(snapshot, project_cache, selected_frame_id, next_snapshot);
    }
}
