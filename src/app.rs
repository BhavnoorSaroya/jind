use gloo::timers::callback::Interval;
use js_sys::Reflect;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    window, DragEvent, HtmlElement, HtmlInputElement, HtmlSelectElement, HtmlTextAreaElement,
};
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
    preview_still_url: Option<String>,
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

#[derive(Clone, Debug, Default, PartialEq)]
struct ExportModalState {
    visible: bool,
    completed: bool,
    export_path: Option<String>,
}

#[function_component(App)]
pub fn app() -> Html {
    let snapshot = use_state(AppSnapshot::default);
    let project_cache = use_state(|| None::<ProjectView>);
    let cameras = use_state(Vec::<CameraDeviceView>::new);
    let selected_frame_id = use_state(|| None::<u64>);
    let dragged_frame_id = use_state(|| None::<u64>);
    let live_preview_enabled = use_state(|| true);
    let onion_skin_enabled = use_state(|| true);
    let onion_skin_opacity = use_state(|| 38_u32);
    let playing = use_state(|| false);
    let camera_modal_open = use_state(|| false);
    let playback_cursor = use_mut_ref(|| 0_usize);
    let capture_preview_nonce = use_state(|| 0_u64);
    let frozen_preview_url = use_state(|| None::<String>);
    let export_modal = use_state(ExportModalState::default);

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
            (
                (*snapshot).project.is_some(),
                (*snapshot).selected_camera.clone(),
                (*cameras).clone(),
            ),
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
                                    sync_snapshot(
                                        snapshot,
                                        project_cache,
                                        selected_frame_id,
                                        next_snapshot,
                                    );
                                }
                            });
                        }
                    }
                }
                || {}
            },
        );
    }

    // global keyboard listener for keyboard shortcuts

    {
        let playing = playing.clone();
        let selected_frame_id = selected_frame_id.clone();
        let playback_cursor = playback_cursor.clone();
        let frame_ids = snapshot
            .project
            .as_ref()
            .or_else(|| (*project_cache).as_ref())
            .map(|project| {
                project
                    .frames
                    .iter()
                    .map(|frame| frame.id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        use_effect_with(
            ((*playing), frame_ids.clone()),
            move |(is_playing, frame_ids)| {
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
            },
        );
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
    let live_onion_frame = current_project
        .as_ref()
        .and_then(|project| project.frames.last());
    let preview_stream_url = snapshot
        .preview_url
        .clone()
        .filter(|_| snapshot.selected_camera.is_some() && *live_preview_enabled && !*playing)
        .map(|url| format!("{url}?device={current_device_id}&mode={current_mode_id}"));
    let preview_still_url = snapshot
        .preview_still_url
        .clone()
        .filter(|_| snapshot.selected_camera.is_some() && *live_preview_enabled && !*playing)
        .map(|url| {
            format!(
                "{url}?device={current_device_id}&mode={current_mode_id}&capture={}",
                *capture_preview_nonce
            )
        });
    let onion_skin_style = format!("opacity: {:.2};", *onion_skin_opacity as f64 / 100.0);

    {
        use_effect_with(
            (active_selected_frame_id, *live_preview_enabled, *playing),
            move |(frame_id, live, playing)| {
                let target_id = if *live && !*playing {
                    Some("timeline-live-proxy".to_string())
                } else {
                    frame_id.map(|frame_id| format!("timeline-frame-{frame_id}"))
                };

                if let Some(target_id) = target_id {
                    scroll_timeline_item_into_view(&target_id);
                }
                || {}
            },
        );
    }

    let status_line = snapshot
        .status
        .detail
        .clone()
        .or_else(|| {
            snapshot
                .status
                .last_result
                .as_ref()
                .map(|last| last.message.clone())
        })
        .unwrap_or_else(|| {
            if has_project {
                "Ready.".to_string()
            } else {
                "Create or open a `.jind` project to begin.".to_string()
            }
        });

    let frame_count = current_project
        .as_ref()
        .map(|project| project.frames.len())
        .unwrap_or(0);
    let resolution_label = current_project
        .as_ref()
        .and_then(|project| project.resolution.as_ref())
        .map(|resolution| format!("{}x{}", resolution.width, resolution.height))
        .unwrap_or_else(|| "Resolution unlocks on first capture".to_string());
    let app_shell_classes = classes!(
        "app-shell",
        has_project.then_some("app-shell--project"),
        (!has_project).then_some("app-shell--welcome"),
    );

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
                    invoke_command::<Option<String>, _>("pick_new_project_path", &EmptyArgs {})
                        .await
                {
                    let mut next_snapshot = if let Ok(next_snapshot) =
                        invoke_command::<AppSnapshot, _>("create_project", &PathArg { path }).await
                    {
                        next_snapshot
                    } else {
                        return;
                    };

                    if let Ok(available_cameras) =
                        invoke_command::<Vec<CameraDeviceView>, _>("list_cameras", &EmptyArgs {})
                            .await
                    {
                        cameras.set(available_cameras.clone());
                        if next_snapshot.selected_camera.is_none() {
                            if let Some(selection) = first_camera_selection(&available_cameras) {
                                if let Ok(camera_snapshot) =
                                    invoke_command::<AppSnapshot, _>("select_camera", &selection)
                                        .await
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
                    invoke_command::<Option<String>, _>("pick_open_project_path", &EmptyArgs {})
                        .await
                {
                    let mut next_snapshot = if let Ok(next_snapshot) =
                        invoke_command::<AppSnapshot, _>("open_project", &PathArg { path }).await
                    {
                        next_snapshot
                    } else {
                        return;
                    };

                    if let Ok(available_cameras) =
                        invoke_command::<Vec<CameraDeviceView>, _>("list_cameras", &EmptyArgs {})
                            .await
                    {
                        cameras.set(available_cameras.clone());
                        if next_snapshot.selected_camera.is_none() {
                            if let Some(selection) = first_camera_selection(&available_cameras) {
                                if let Ok(camera_snapshot) =
                                    invoke_command::<AppSnapshot, _>("select_camera", &selection)
                                        .await
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
        let snapshot = snapshot.clone();
        let project_cache = project_cache.clone();
        let live_preview_enabled = live_preview_enabled.clone();
        let playing = playing.clone();
        let selected_frame_id = selected_frame_id.clone();
        let playback_cursor = playback_cursor.clone();
        let capture_preview_nonce = capture_preview_nonce.clone();
        let frozen_preview_url = frozen_preview_url.clone();
        let current_device_id = current_device_id.clone();
        let current_mode_id = current_mode_id.clone();
        Callback::from(move |_| {
            let snapshot = snapshot.clone();
            let project_cache = project_cache.clone();
            let live_preview_enabled = live_preview_enabled.clone();
            let playing = playing.clone();
            let selected_frame_id = selected_frame_id.clone();
            let playback_cursor = playback_cursor.clone();
            let capture_preview_nonce = capture_preview_nonce.clone();
            let frozen_preview_url = frozen_preview_url.clone();
            let current_device_id = current_device_id.clone();
            let current_mode_id = current_mode_id.clone();
            playing.set(false);
            live_preview_enabled.set(true);
            let next_capture_nonce = (*capture_preview_nonce).wrapping_add(1);
            capture_preview_nonce.set(next_capture_nonce);
            frozen_preview_url.set(snapshot.preview_still_url.clone().map(|url| {
                format!(
                    "{url}?device={current_device_id}&mode={current_mode_id}&capture={next_capture_nonce}"
                )
            }));
            *playback_cursor.borrow_mut() = 0;
            spawn_local(async move {
                if let Ok(next_snapshot) =
                    invoke_command::<AppSnapshot, _>("capture_frame", &EmptyArgs {}).await
                {
                    playing.set(false);
                    live_preview_enabled.set(true);
                    frozen_preview_url.set(None);
                    sync_snapshot(snapshot, project_cache, selected_frame_id, next_snapshot);
                } else {
                    frozen_preview_url.set(None);
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
                        .and_then(|selected| {
                            project.frames.iter().find(|frame| frame.id == *selected)
                        })
                        .or_else(|| project.frames.first())
                })
                .map(|frame| frame.id);
            let next_selected_after_delete = current_project.as_ref().and_then(|project| {
                let index = project
                    .frames
                    .iter()
                    .position(|frame| Some(frame.id) == frame_id)?;
                project
                    .frames
                    .get(index + 1)
                    .or_else(|| {
                        index
                            .checked_sub(1)
                            .and_then(|left| project.frames.get(left))
                    })
                    .map(|frame| frame.id)
            });

            let snapshot = snapshot.clone();
            let project_cache = project_cache.clone();
            let selected_frame_id = selected_frame_id.clone();
            let playing = playing.clone();
            if let Some(frame_id) = frame_id {
                spawn_local(async move {
                    match invoke_command::<AppSnapshot, _>(
                        "delete_frame",
                        &DeleteFrameArg { frame_id },
                    )
                    .await
                    {
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

    {
        let on_capture = on_capture.clone();
        let on_delete = on_delete.clone();
        let keyboard_busy = snapshot.status.busy;
        let keyboard_has_camera = snapshot.selected_camera.is_some();

        use_effect_with(
            (
                keyboard_busy,
                keyboard_has_camera,
                on_capture.clone(),
                on_delete.clone(),
            ),
            move |(keyboard_busy, keyboard_has_camera, on_capture, on_delete)| {
                let window = window().expect("no window");
                let keyboard_busy = *keyboard_busy;
                let keyboard_has_camera = *keyboard_has_camera;
                let on_capture = on_capture.clone();
                let on_delete = on_delete.clone();

                let handler = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::wrap(Box::new(
                    move |event: web_sys::KeyboardEvent| {
                        // ignore if typing in an input control
                        if let Some(target) = event.target() {
                            if let Some(input) = target.dyn_ref::<HtmlInputElement>() {
                                let is_capture_from_toggle = event.code() == "Space"
                                    && matches!(input.type_().as_str(), "checkbox" | "range");
                                if !is_capture_from_toggle {
                                    return;
                                }
                            } else if target.dyn_ref::<HtmlSelectElement>().is_some()
                                || target.dyn_ref::<HtmlTextAreaElement>().is_some()
                            {
                                return;
                            }
                        }

                        if event.repeat() {
                            return;
                        }

                        if event.code() == "Space" {
                            event.prevent_default();

                            if let Some(target) = event.target() {
                                if let Some(input) = target.dyn_ref::<HtmlInputElement>() {
                                    let _ = input.blur();
                                }
                            }

                            if !keyboard_busy && keyboard_has_camera {
                                on_capture.emit(());
                            }
                        } else if event.key() == "Delete" {
                            event.prevent_default();

                            if !keyboard_busy {
                                on_delete.emit(());
                            }
                        }
                    },
                ));

                window
                    .add_event_listener_with_callback("keydown", handler.as_ref().unchecked_ref())
                    .unwrap();

                move || {
                    window
                        .remove_event_listener_with_callback(
                            "keydown",
                            handler.as_ref().unchecked_ref(),
                        )
                        .unwrap();
                }
            },
        );
    }

    let on_export = {
        let snapshot = snapshot.clone();
        let project_cache = project_cache.clone();
        let selected_frame_id = selected_frame_id.clone();
        let export_modal = export_modal.clone();
        Callback::from(move |_| {
            let snapshot = snapshot.clone();
            let project_cache = project_cache.clone();
            let selected_frame_id = selected_frame_id.clone();
            let export_modal = export_modal.clone();
            spawn_local(async move {
                if let Ok(Some(path)) =
                    invoke_command::<Option<String>, _>("pick_export_path", &EmptyArgs {}).await
                {
                    export_modal.set(ExportModalState {
                        visible: true,
                        completed: false,
                        export_path: Some(path.clone()),
                    });

                    match invoke_command::<AppSnapshot, _>(
                        "export_mp4",
                        &PathArg { path: path.clone() },
                    )
                    .await
                    {
                        Ok(next_snapshot) => {
                            sync_snapshot(
                                snapshot,
                                project_cache,
                                selected_frame_id,
                                next_snapshot,
                            );
                            export_modal.set(ExportModalState {
                                visible: true,
                                completed: true,
                                export_path: Some(path),
                            });
                        }
                        Err(error) => {
                            export_modal.set(ExportModalState::default());
                            set_command_error(snapshot, error);
                        }
                    }
                }
            });
        })
    };

    let on_close_export_modal = {
        let export_modal = export_modal.clone();
        Callback::from(move |_| {
            export_modal.set(ExportModalState::default());
        })
    };

    let on_open_export_folder = {
        let export_modal = export_modal.clone();
        Callback::from(move |_| {
            let Some(path) = export_modal.export_path.clone() else {
                return;
            };
            spawn_local(async move {
                let _ = invoke_command::<(), _>("reveal_export_in_folder", &PathArg { path }).await;
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
                            .and_then(|selected| {
                                project
                                    .frames
                                    .iter()
                                    .position(|frame| frame.id == *selected)
                            })
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

    let on_open_camera_modal = {
        let camera_modal_open = camera_modal_open.clone();
        Callback::from(move |_| camera_modal_open.set(true))
    };

    let on_close_camera_modal = {
        let camera_modal_open = camera_modal_open.clone();
        Callback::from(move |_| camera_modal_open.set(false))
    };

    let on_camera_modal_card_click = Callback::from(|event: MouseEvent| {
        event.stop_propagation();
    });

    fn to_mouse_cb(cb: Callback<()>) -> Callback<MouseEvent> {
        Callback::from(move |_| cb.emit(()))
    }

    let capture_icon = html! {
    // took from heroicons
        <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" class="size-6" style="height: 1.5rem; width: auto;">
            <path stroke-linecap="round" stroke-linejoin="round" d="M6.827 6.175A2.31 2.31 0 0 1 5.186 7.23c-.38.054-.757.112-1.134.175C2.999 7.58 2.25 8.507 2.25 9.574V18a2.25 2.25 0 0 0 2.25 2.25h15A2.25 2.25 0 0 0 21.75 18V9.574c0-1.067-.75-1.994-1.802-2.169a47.865 47.865 0 0 0-1.134-.175 2.31 2.31 0 0 1-1.64-1.055l-.822-1.316a2.192 2.192 0 0 0-1.736-1.039 48.774 48.774 0 0 0-5.232 0 2.192 2.192 0 0 0-1.736 1.039l-.821 1.316Z" />
            <path stroke-linecap="round" stroke-linejoin="round" d="M16.5 12.75a4.5 4.5 0 1 1-9 0 4.5 4.5 0 0 1 9 0ZM18.75 10.5h.008v.008h-.008V10.5Z" />
        </svg>

    };

    let delete_icon = html! {
    // took from heroicons
        <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" class="size-6" style="height: 1.5rem; width: auto;">
            <path stroke-linecap="round" stroke-linejoin="round" d="m14.74 9-.346 9m-4.788 0L9.26 9m9.968-3.21c.342.052.682.107 1.022.166m-1.022-.165L18.16 19.673a2.25 2.25 0 0 1-2.244 2.077H8.084a2.25 2.25 0 0 1-2.244-2.077L4.772 5.79m14.456 0a48.108 48.108 0 0 0-3.478-.397m-12 .562c.34-.059.68-.114 1.022-.165m0 0a48.11 48.11 0 0 1 3.478-.397m7.5 0v-.916c0-1.18-.91-2.164-2.09-2.201a51.964 51.964 0 0 0-3.32 0c-1.18.037-2.09 1.022-2.09 2.201v.916m7.5 0a48.667 48.667 0 0 0-7.5 0" />
        </svg>

    };

    let playback_icon = if *playing {
        html! {
            <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" class="size-6" style="height: 1.5rem; width: auto;">
                <path stroke-linecap="round" stroke-linejoin="round" d="M15.75 5.25v13.5m-7.5-13.5v13.5" />
            </svg>

        }
    } else {
        html! {
            <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" class="size-6" style="height: 1.5rem; width: auto;">
                <path stroke-linecap="round" stroke-linejoin="round" d="M5.25 5.653c0-.856.917-1.398 1.667-.986l11.54 6.347a1.125 1.125 0 0 1 0 1.972l-11.54 6.347a1.125 1.125 0 0 1-1.667-.986V5.653Z" />
            </svg>

        }
    };

    html! {
        <main class={app_shell_classes}>

            {
                if has_project {
                    html! {
                        <section class="editor-shell">
                            <aside class="control-panel">
                                <div class="panel-card panel-card--actions">
                                    <div class="panel-heading">
                                        <p class="panel-kicker">{"Controls"}</p>
                                    </div>
                                    <button class="secondary preview-camera-button" onclick={on_open_camera_modal.clone()} disabled={busy}>
                                            {"Capture source"}
                                        </button>

                                    <label class="toggle">
                                        <input
                                            class="toggle-input"
                                            type="checkbox"
                                            checked={*onion_skin_enabled}
                                            onchange={{
                                                let onion_skin_enabled = onion_skin_enabled.clone();
                                                Callback::from(move |event: Event| {
                                                    let input: HtmlInputElement =
                                                        event.target_unchecked_into();
                                                    onion_skin_enabled.set(input.checked());
                                                })
                                            }}
                                        />
                                        <span class="toggle-switch" aria-hidden="true"></span>
                                        <span>{"Onion skin"}</span>
                                    </label>
                                    <label class="field slider-field">
                                        <span>{format!("Onion strength {}%", *onion_skin_opacity)}</span>
                                        <input
                                            class="slider-input"
                                            type="range"
                                            min="0"
                                            max="100"
                                            step="1"
                                            value={(*onion_skin_opacity).to_string()}
                                            disabled={!*onion_skin_enabled}
                                            oninput={{
                                                let onion_skin_opacity = onion_skin_opacity.clone();
                                                Callback::from(move |event: InputEvent| {
                                                    let input: HtmlInputElement =
                                                        event.target_unchecked_into();
                                                    if let Ok(value) = input.value().parse::<u32>() {
                                                        onion_skin_opacity.set(value);
                                                    }
                                                })
                                            }}
                                        />
                                    </label>

                                    <button class="timeline-count" onclick={on_export} disabled={busy || !snapshot.ffmpeg_available || current_project.as_ref().map(|project| project.frames.is_empty()).unwrap_or(true)}>
                                        {format!("Export {} frames", frame_count) }
                                        // {format!(" {} seconds", frame_count/15)} // need to add seconds/minutes counter
                                    </button> // combine export with frame count thingy

                                    <div class="action-grid">
                                        // <button
                                        //     class="primary icon-button"
                                        //     onclick={to_mouse_cb(on_capture.clone())}
                                        //     disabled={busy || snapshot.selected_camera.is_none()}
                                        //     aria-label="Capture frame"
                                        //     title="Capture frame"
                                        // >
                                        //     {capture_icon}
                                        // </button>
                                        // <button
                                        //     class="danger icon-button"
                                        //     onclick={to_mouse_cb(on_delete.clone())}
                                        //     disabled={busy || delete_target_frame_id.is_none()}
                                        //     aria-label="Delete frame"
                                        //     title="Delete frame"
                                        // >
                                        //     {delete_icon}
                                        // </button>
                                        // <button
                                        //     class="secondary icon-button"
                                        //     onclick={on_toggle_playback}
                                        //     disabled={busy || current_project.as_ref().map(|project| project.frames.is_empty()).unwrap_or(true)}
                                        //     aria-label={if *playing { "Pause playback" } else { "Start playback" }}
                                        //     title={if *playing { "Pause playback" } else { "Start playback" }}
                                        // >
                                        //     {playback_icon}
                                        // </button>
                                    </div>

                                    {
                                        if !snapshot.ffmpeg_available {
                                            html! {}
                                            // html! { <p class="muted">{"`ffmpeg` not on PATH, export is disabled"}</p> }
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
                                                if let Some(preview_still_url) = (*frozen_preview_url)
                                                    .clone()
                                                    .or_else(|| preview_still_url.clone())
                                                {
                                                    html! {
                                                        <>
                                                            <img class="preview-image" src={preview_still_url} alt="Frozen live preview" />
                                                            {
                                                                if *onion_skin_enabled {
                                                                    live_onion_frame.map(|frame| html! {
                                                                        <img class="preview-image onion-layer" style={onion_skin_style.clone()} src={frame.image_url.clone()} alt="Onion skin frame" />
                                                                    }).unwrap_or_default()
                                                                } else {
                                                                    html! {}
                                                                }
                                                            }
                                                        </>
                                                    }
                                                } else if let Some(preview_stream_url) = preview_stream_url.clone() {
                                                    html! {
                                                        <>
                                                            <img class="preview-image" src={preview_stream_url} alt="Live preview" />
                                                            {
                                                                if *onion_skin_enabled {
                                                                    live_onion_frame.map(|frame| html! {
                                                                        <img class="preview-image onion-layer" style={onion_skin_style.clone()} src={frame.image_url.clone()} alt="Onion skin frame" />
                                                                    }).unwrap_or_default()
                                                                } else {
                                                                    html! {}
                                                                }
                                                            }
                                                        </>
                                                    }
                                                } else {
                                                    html! { <div class="preview-placeholder">{"Capturing..."}</div> }
                                                }
                                            } else if let Some(preview_stream_url) = preview_stream_url.clone() {
                                                html! {
                                                    <>
                                                        <img class="preview-image" src={preview_stream_url} alt="Live preview" />
                                                        {
                                                            if *onion_skin_enabled {
                                                                live_onion_frame.map(|frame| html! {
                                                                    <img class="preview-image onion-layer" style={onion_skin_style.clone()} src={frame.image_url.clone()} alt="Onion skin frame" />
                                                                }).unwrap_or_default()
                                                            } else {
                                                                html! {}
                                                            }
                                                        }
                                                    </>
                                                }
                                            } else if let Some(frame) = active_selected_frame {
                                                html! {
                                                    <img class="preview-image" src={frame.image_url.clone()} alt="selected frame" />
                                                }
                                            } else {
                                                html! { <div class="preview-placeholder">{"Select a camera, then capture a frame."}</div> }
                                            }
                                        }
                                    </div>
                                </div>
                            </section>

                            <div class="timeline-card timeline-card--dock">
                                <div class="timeline-header">
                                    // <div>
                                        // <p class="panel-kicker">{"Frames"}</p>
                                        // <h2>{"Timeline"}</h2>
                                        <button
                                            class="secondary icon-button control-icon"
                                            onclick={on_toggle_playback}
                                            disabled={busy || current_project.as_ref().map(|project| project.frames.is_empty()).unwrap_or(true)}
                                            aria-label={if *playing { "Pause playback" } else { "Start playback" }}
                                            title={if *playing { "Pause playback" } else { "Start playback" }}
                                        >
                                            {playback_icon}
                                        </button>
                                        <button
                                            class="primary icon-button circle-btn"
                                            onclick={to_mouse_cb(on_capture.clone())}
                                            disabled={busy || snapshot.selected_camera.is_none()}
                                            aria-label="Capture frame"
                                            title="Capture frame"
                                        >
                                            {capture_icon}
                                        </button>
                                        <button
                                            class="danger icon-button control-icon"
                                            onclick={to_mouse_cb(on_delete.clone())}
                                            disabled={busy || delete_target_frame_id.is_none()}
                                            aria-label="Delete frame"
                                            title="Delete frame"
                                        >
                                            {delete_icon}
                                        </button>

                                    // </div>
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
                                                                // <span>{format!("#{}", frame.id)}</span>
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
                                                        <center>
                                                        <span>{"Live Preview"}</span>
                                                        </center>
                                                    </button>
                                                </>
                                            }
                                        }).unwrap_or_default()
                                    }
                                </div>
                            </div>
                        </section>
                    }
                } else {
                    html! {
                    <div>

            // <section class="status-strip">
            //     <div class="status-pill">
            //         // <span class="status-dot"></span>
            //         // <div class="status-copy">
            //             // <span class="status-kicker">{"System status"}</span>
            //             // <strong>{status_phase_label(&snapshot.status)}</strong>
            //             // <span>{status_line.clone()}</span>
            //         // </div>
            //     </div>
            //     {
            //         if let Some(project) = current_project.as_ref() {
            //             html! {
            //                 <div class="project-meta">
            //                     <span title={project.project_path.clone()}>{project.project_path.clone()}</span>
            //                     <span>{format!("{} FPS", project.fps)}</span>
            //                     <span>{resolution_label.clone()}</span>
            //                 </div>
            //             }
            //         } else {
            //             html! {  }
            //         }
            //     }
            // </section>
                        <section class="welcome-shell">
                            <div class="hero-card">
                                <div class="hero-layout">
                                    <figure class="maharani-card">
                                        <div class="maharani-frame">
                                            <img src="public/original.png" alt="Portrait of Maharani Jind Kaur" />
                                        </div>
                                        // <figcaption>
                                        //     <span class="maharani-label">{"Maharani Jind Kaur"}</span>
                                        //     <span class="maharani-caption">
                                        //         {"Queen of the Sikh Empire and the namesake behind Jind."}
                                        //     </span>
                                        // </figcaption>
                                    </figure>
                                    <div class="hero-copy-block">
                                    // <div class="project-meta"><span>{"Written in rust"}</span></div>
                                        <p class="eyebrow">{"Release v0.1 Alpha"}</p>
                                        <h1>{"Jind stop motion"}</h1>
                                        <p class="hero-copy">
                                            // {"Jind means \"life\" or \"soul\""}
                                            {"yes, its written in rust"}
                                        </p>
                                        <div class="hero-actions">
                                            <button class="primary" onclick={on_create_project} disabled={busy}>{"New Project"}</button>
                                            <button class="secondary" onclick={on_open_project} disabled={busy}>{"Open Project"}</button>
                                        </div>
                                    </div>
                                </div>
                            </div>
                        </section>
                    </div>

                    }
                }
            }

            {
                if snapshot.status.phase == "cleaning-up" {
                    html! {
                        <div class="modal-scrim">
                            <div class="modal-card">
                                <h2>{"Cleaning Up"}</h2>
                                <p>{"The app will close when file operations finish."}</p>
                            </div>
                        </div>
                    }
                } else if export_modal.visible {
                    html! {
                        <div class="modal-scrim">
                            <div class="modal-card export-modal">
                                {
                                    if export_modal.completed {
                                        html! {
                                            <>
                                                <h2>{"Export Complete"}</h2>
                                                <p>{"MP4 export is complete"}</p>
                                                <div class="export-actions">
                                                    <button class="primary" onclick={on_open_export_folder.clone()}>
                                                        {"Open Folder"}
                                                    </button>
                                                    <button class="secondary" onclick={on_close_export_modal.clone()}>
                                                        {"Close"}
                                                    </button>
                                                </div>
                                            </>
                                        }
                                    } else {
                                        html! {
                                            <>
                                                <div class="export-loader" aria-hidden="true"></div>
                                                <h2>{"Exporting"}</h2>
                                                <p>{"Rendering your MP4. This can take a moment."}</p>
                                            </>
                                        }
                                    }
                                }
                            </div>
                        </div>
                    }
                } else if *camera_modal_open {
                    html! {
                        <div class="modal-scrim" onclick={on_close_camera_modal.clone()}>
                            <div class="modal-card camera-modal" onclick={on_camera_modal_card_click}>
                                <div class="panel-heading">
                                    <div>
                                        <p class="panel-kicker">{"Choose the camera input and resolution"}</p>
                                        // <h2>{"Camera"}</h2>
                                    </div>
                                    <button class="secondary modal-close-button" onclick={on_close_camera_modal.clone()} disabled={busy}>
                                        {"Close"}
                                    </button>
                                </div>

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
    let payload =
        Reflect::get(&event, &JsValue::from_str("payload")).map_err(js_error_to_string)?;
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
    error.as_string().unwrap_or_else(|| format!("{error:?}"))
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
