use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use crate::{
    audio::AppSounds,
    auth::AuthStore,
    create_screenshot,
    general_settings::{
        GeneralSettingsStore, MainWindowRecordingStartBehaviour, PostStudioRecordingBehaviour,
    },
    open_external_link,
    presets::PresetsStore,
    upload::{
        create_or_get_video, prepare_screenshot_upload, upload_video, InstantMultipartUpload,
    },
    web_api::ManagerExt,
    windows::{CapWindowId, ShowCapWindow},
    App, CurrentRecordingChanged, DynLoggingLayer, MutableState, NewStudioRecordingAdded,
    RecordingStarted, RecordingStopped, VideoUploadInfo,
};
use cap_fail::fail;
use cap_media::{feeds::CameraFeed, platform::display_for_window, sources::ScreenCaptureTarget};
use cap_media::{
    platform::Bounds,
    sources::{CaptureScreen, CaptureWindow},
};
use cap_project::{
    Platform, ProjectConfiguration, RecordingMeta, RecordingMetaInner, SharingMeta,
    StudioRecordingMeta, TimelineConfiguration, TimelineSegment, ZoomSegment,
};
use cap_recording::{
    instant_recording::{CompletedInstantRecording, InstantRecordingHandle},
    CompletedStudioRecording, RecordingError, RecordingMode, StudioRecordingHandle,
};
use cap_rendering::ProjectRecordingsMeta;
use cap_utils::{ensure_dir, spawn_actor};
use serde::Deserialize;
use specta::Type;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogBuilder};
use tauri_specta::Event;
use tracing::{error, info};

pub enum InProgressRecording {
    Instant {
        target_name: String,
        handle: InstantRecordingHandle,
        progressive_upload: Option<InstantMultipartUpload>,
        video_upload_info: VideoUploadInfo,
        inputs: StartRecordingInputs,
        recording_dir: PathBuf,
    },
    Studio {
        target_name: String,
        handle: StudioRecordingHandle,
        inputs: StartRecordingInputs,
        recording_dir: PathBuf,
    },
}

impl InProgressRecording {
    pub fn capture_target(&self) -> &ScreenCaptureTarget {
        match self {
            Self::Instant { handle, .. } => &handle.capture_target,
            Self::Studio { handle, .. } => &handle.capture_target,
        }
    }

    pub fn inputs(&self) -> &StartRecordingInputs {
        match self {
            Self::Instant { inputs, .. } => inputs,
            Self::Studio { inputs, .. } => inputs,
        }
    }

    pub async fn pause(&self) -> Result<(), RecordingError> {
        match self {
            Self::Instant { handle, .. } => handle.pause().await,
            Self::Studio { handle, .. } => handle.pause().await,
        }
    }

    pub async fn resume(&self) -> Result<(), RecordingError> {
        match self {
            Self::Instant { handle, .. } => handle.resume().await,
            Self::Studio { handle, .. } => handle.resume().await,
        }
    }

    pub fn recording_dir(&self) -> &PathBuf {
        match self {
            Self::Instant { recording_dir, .. } => recording_dir,
            Self::Studio { recording_dir, .. } => recording_dir,
        }
    }

    pub async fn stop(self) -> Result<CompletedRecording, RecordingError> {
        Ok(match self {
            Self::Instant {
                handle,
                progressive_upload,
                video_upload_info,
                target_name,
                ..
            } => CompletedRecording::Instant {
                recording: handle.stop().await?,
                progressive_upload,
                video_upload_info,
                target_name,
            },
            Self::Studio {
                handle,
                target_name,
                ..
            } => CompletedRecording::Studio {
                recording: handle.stop().await?,
                target_name,
            },
        })
    }

    pub async fn cancel(self) -> Result<(), RecordingError> {
        match self {
            Self::Instant { handle, .. } => handle.cancel().await,
            Self::Studio { handle, .. } => handle.cancel().await,
        }
    }

    pub fn bounds(&self) -> &Bounds {
        match self {
            Self::Instant { handle, .. } => &handle.bounds,
            Self::Studio { handle, .. } => &handle.bounds,
        }
    }
}

pub enum CompletedRecording {
    Instant {
        recording: CompletedInstantRecording,
        target_name: String,
        progressive_upload: Option<InstantMultipartUpload>,
        video_upload_info: VideoUploadInfo,
    },
    Studio {
        recording: CompletedStudioRecording,
        target_name: String,
    },
}

impl CompletedRecording {
    pub fn id(&self) -> &String {
        match self {
            Self::Instant { recording, .. } => &recording.id,
            Self::Studio { recording, .. } => &recording.id,
        }
    }

    pub fn project_path(&self) -> &PathBuf {
        match self {
            Self::Instant { recording, .. } => &recording.project_path,
            Self::Studio { recording, .. } => &recording.project_path,
        }
    }

    pub fn target_name(&self) -> &String {
        match self {
            Self::Instant { target_name, .. } => target_name,
            Self::Studio { target_name, .. } => target_name,
        }
    }
}

#[tauri::command(async)]
#[specta::specta]
pub async fn list_capture_screens() -> Vec<CaptureScreen> {
    cap_media::sources::list_screens()
        .into_iter()
        .map(|(v, _)| v)
        .collect()
}

#[tauri::command(async)]
#[specta::specta]
pub async fn list_capture_windows() -> Vec<CaptureWindow> {
    cap_media::sources::list_windows()
        .into_iter()
        .map(|(v, _)| v)
        .collect()
}

#[tauri::command(async)]
#[specta::specta]
pub fn list_cameras() -> Vec<String> {
    CameraFeed::list_cameras()
}

#[derive(Deserialize, Type, Clone)]
pub struct StartRecordingInputs {
    pub capture_target: ScreenCaptureTarget,
    #[serde(default)]
    pub capture_system_audio: bool,
    pub mode: RecordingMode,
}

#[tauri::command]
#[specta::specta]
#[tracing::instrument(name = "recording", skip_all)]
pub async fn start_recording(
    app: AppHandle,
    state_mtx: MutableState<'_, App>,
    inputs: StartRecordingInputs,
) -> Result<(), String> {
    let id = uuid::Uuid::new_v4().to_string();

    let recording_dir = app
        .path()
        .app_data_dir()
        .unwrap()
        .join("recordings")
        .join(format!("{id}.cap"));

    ensure_dir(&recording_dir).map_err(|e| format!("Failed to create recording directory: {e}"))?;
    let logfile = std::fs::File::create(recording_dir.join("recording-logs.log"))
        .map_err(|e| format!("Failed to create logfile: {e}"))?;

    state_mtx
        .write()
        .await
        .recording_logging_handle
        .reload(Some(Box::new(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_writer(logfile),
        ) as DynLoggingLayer))
        .map_err(|e| format!("Failed to reload logging layer: {e}"))?;

    let target_name = {
        let title = inputs.capture_target.get_title();

        match inputs.capture_target {
            ScreenCaptureTarget::Area { .. } => "Area".to_string(),
            ScreenCaptureTarget::Window { id, .. } => {
                let platform_windows: HashMap<u32, cap_media::platform::Window> =
                    cap_media::platform::get_on_screen_windows()
                        .into_iter()
                        .map(|window| (window.window_id, window))
                        .collect();

                platform_windows
                    .get(&id)
                    .map(|v| v.owner_name.to_string())
                    .unwrap_or_else(|| "Window".to_string())
            }
            ScreenCaptureTarget::Screen { .. } => title.unwrap_or_else(|| "Screen".to_string()),
        }
    };

    if let Some(window) = CapWindowId::Camera.get(&app) {
        let _ = window.set_content_protected(matches!(inputs.mode, RecordingMode::Studio));
    }

    let video_upload_info = match inputs.mode {
        RecordingMode::Instant => {
            match AuthStore::get(&app).ok().flatten() {
                Some(_) => {
                    // Pre-create the video and get the shareable link
                    if let Ok(s3_config) = create_or_get_video(
                        &app,
                        false,
                        None,
                        Some(format!(
                            "{target_name} {}",
                            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
                        )),
                    )
                    .await
                    {
                        let link = app.make_app_url(format!("/s/{}", s3_config.id())).await;
                        info!("Pre-created shareable link: {}", link);

                        Some(VideoUploadInfo {
                            id: s3_config.id().to_string(),
                            link: link.clone(),
                            config: s3_config,
                        })
                    } else {
                        None
                    }
                }
                // Allow the recording to proceed without error for any signed-in user
                _ => {
                    // User is not signed in
                    return Err("Please sign in to use instant recording".to_string());
                }
            }
        }
        RecordingMode::Studio => None,
    };

    match &inputs.capture_target {
        ScreenCaptureTarget::Window { id } => {
            #[cfg(target_os = "macos")]
            let display = display_for_window(*id).unwrap().id;

            #[cfg(windows)]
            let display = {
                let scap::Target::Window(target) = inputs.capture_target.get_target().unwrap()
                else {
                    unreachable!();
                };
                display_for_window(target.raw_handle).unwrap().0 as u32
            };

            let _ = ShowCapWindow::WindowCaptureOccluder { screen_id: display }
                .show(&app)
                .await;
        }
        ScreenCaptureTarget::Area { screen, .. } => {
            let _ = ShowCapWindow::WindowCaptureOccluder { screen_id: *screen }
                .show(&app)
                .await;
        }
        _ => {}
    }

    let (finish_upload_tx, finish_upload_rx) = flume::bounded(1);
    let progressive_upload = video_upload_info
        .as_ref()
        .filter(|_| matches!(inputs.mode, RecordingMode::Instant))
        .map(|video_upload_info| {
            InstantMultipartUpload::spawn(
                app.clone(),
                id.clone(),
                recording_dir.join("content/output.mp4"),
                video_upload_info.clone(),
                Some(finish_upload_rx),
            )
        });

    println!("spawning actor");

    // done in spawn to catch panics just in case
    let actor_done_rx = spawn_actor({
        let state_mtx = Arc::clone(&state_mtx);
        let app = app.clone();
        async move {
            fail!("recording::spawn_actor");
            let mut state = state_mtx.write().await;

            let base_inputs = cap_recording::RecordingBaseInputs {
                capture_target: inputs.capture_target,
                capture_system_audio: inputs.capture_system_audio,
                mic_feed: &state.mic_feed,
            };

            let (actor, actor_done_rx) = match inputs.mode {
                RecordingMode::Studio => {
                    let (handle, actor_done_rx) = cap_recording::spawn_studio_recording_actor(
                        id.clone(),
                        recording_dir.clone(),
                        base_inputs,
                        state.camera_feed.clone(),
                        GeneralSettingsStore::get(&app)
                            .ok()
                            .flatten()
                            .map(|s| s.custom_cursor_capture)
                            .unwrap_or_default(),
                    )
                    .await
                    .map_err(|e| {
                        error!("Failed to spawn studio recording actor: {e}");
                        e.to_string()
                    })?;

                    (
                        InProgressRecording::Studio {
                            handle,
                            target_name,
                            inputs,
                            recording_dir: recording_dir.clone(),
                        },
                        actor_done_rx,
                    )
                }
                RecordingMode::Instant => {
                    let Some(video_upload_info) = video_upload_info.clone() else {
                        return Err("Video upload info not found".to_string());
                    };

                    let (handle, actor_done_rx) =
                        cap_recording::instant_recording::spawn_instant_recording_actor(
                            id.clone(),
                            recording_dir.clone(),
                            base_inputs,
                        )
                        .await
                        .map_err(|e| {
                            error!("Failed to spawn studio recording actor: {e}");
                            e.to_string()
                        })?;

                    (
                        InProgressRecording::Instant {
                            handle,
                            progressive_upload,
                            video_upload_info,
                            target_name,
                            inputs,
                            recording_dir: recording_dir.clone(),
                        },
                        actor_done_rx,
                    )
                }
            };

            state.set_current_recording(actor);

            Ok::<_, String>(actor_done_rx)
        }
    })
    .await
    .map_err(|e| format!("Failed to spawn recording actor: {}", e))??;

    spawn_actor({
        let app = app.clone();
        let state_mtx = Arc::clone(&state_mtx);
        async move {
            fail!("recording::wait_actor_done");
            match actor_done_rx.await {
                Ok(Ok(_)) => {
                    let _ = finish_upload_tx.send(());
                    return;
                }
                Ok(Err(e)) => {
                    let mut state = state_mtx.write().await;

                    let mut dialog = MessageDialogBuilder::new(
                        app.dialog().clone(),
                        format!("An error occurred"),
                        e,
                    )
                    .kind(tauri_plugin_dialog::MessageDialogKind::Error);

                    if let Some(window) = CapWindowId::InProgressRecording.get(&app) {
                        dialog = dialog.parent(&window);
                    }

                    dialog.blocking_show();

                    // this clears the current recording for us
                    handle_recording_end(app, None, &mut state).await.ok();
                }
                _ => {}
            }
        }
    });

    if let Some(window) = CapWindowId::Main.get(&app) {
        match GeneralSettingsStore::get(&app)
            .ok()
            .flatten()
            .map(|s| s.main_window_recording_start_behaviour)
            .unwrap_or_default()
        {
            MainWindowRecordingStartBehaviour::Close => {
                let _ = window.close();
            }
            MainWindowRecordingStartBehaviour::Minimise => {
                let _ = window.minimize();
            }
        }
    }

    if let Some(window) = CapWindowId::InProgressRecording.get(&app) {
        window.eval("window.location.reload()").ok();
    } else {
        let _ = ShowCapWindow::InProgressRecording { position: None }
            .show(&app)
            .await;
    }

    AppSounds::StartRecording.play();

    RecordingStarted.emit(&app).ok();

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn pause_recording(state: MutableState<'_, App>) -> Result<(), String> {
    let mut state = state.write().await;

    if let Some(recording) = state.current_recording.as_mut() {
        recording.pause().await.map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn resume_recording(state: MutableState<'_, App>) -> Result<(), String> {
    let mut state = state.write().await;

    if let Some(recording) = state.current_recording.as_mut() {
        recording.resume().await.map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn stop_recording(app: AppHandle, state: MutableState<'_, App>) -> Result<(), String> {
    let mut state = state.write().await;
    let Some(current_recording) = state.clear_current_recording() else {
        return Err("Recording not in progress".to_string())?;
    };

    let completed_recording = current_recording.stop().await.map_err(|e| e.to_string())?;

    handle_recording_end(app, Some(completed_recording), &mut state).await?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn restart_recording(app: AppHandle, state: MutableState<'_, App>) -> Result<(), String> {
    let Some(recording) = state.write().await.clear_current_recording() else {
        return Err("No recording in progress".to_string());
    };

    let _ = CurrentRecordingChanged.emit(&app);

    let inputs = recording.inputs().clone();

    let _ = recording.cancel().await;

    tokio::time::sleep(Duration::from_millis(1000)).await;

    start_recording(app.clone(), state, inputs).await
}

#[tauri::command]
#[specta::specta]
pub async fn delete_recording(app: AppHandle, state: MutableState<'_, App>) -> Result<(), String> {
    let recording_data = {
        let mut app_state = state.write().await;
        if let Some(recording) = app_state.clear_current_recording() {
            let recording_dir = recording.recording_dir().clone();
            let video_id = match &recording {
                InProgressRecording::Instant {
                    video_upload_info, ..
                } => Some(video_upload_info.id.clone()),
                _ => None,
            };
            Some((recording, recording_dir, video_id))
        } else {
            None
        }
    };

    if let Some((recording, recording_dir, video_id)) = recording_data {
        CurrentRecordingChanged.emit(&app).ok();
        RecordingStopped {}.emit(&app).ok();

        let _ = recording.cancel().await;

        std::fs::remove_dir_all(&recording_dir).ok();

        if let Some(id) = video_id {
            let _ = app
                .authed_api_request(
                    format!("/api/desktop/video/delete?videoId={}", id),
                    |c, url| c.delete(url),
                )
                .await;
        }
    }

    Ok(())
}

// runs when a recording ends, whether from success or failure
async fn handle_recording_end(
    handle: AppHandle,
    recording: Option<CompletedRecording>,
    app: &mut App,
) -> Result<(), String> {
    // Clear current recording, just in case :)
    app.current_recording.take();

    if let Some(recording) = recording {
        handle_recording_finish(&handle, recording).await?;
    };

    let _ = RecordingStopped.emit(&handle);

    let _ = app.recording_logging_handle.reload(None);

    if let Some(window) = CapWindowId::InProgressRecording.get(&handle) {
        let _ = window.close();
    }

    if let Some(window) = CapWindowId::Main.get(&handle) {
        window.unminimize().ok();
    } else {
        CapWindowId::Camera.get(&handle).map(|v| {
            let _ = v.close();
        });
        app.camera_feed.take();
        app.mic_feed.take();
    }

    CurrentRecordingChanged.emit(&handle).ok();

    Ok(())
}

// runs when a recording successfully finishes
async fn handle_recording_finish(
    app: &AppHandle,
    completed_recording: CompletedRecording,
) -> Result<(), String> {
    let recording_dir = completed_recording.project_path().clone();

    let screenshots_dir = recording_dir.join("screenshots");
    std::fs::create_dir_all(&screenshots_dir).ok();

    let display_output_path = match &completed_recording {
        CompletedRecording::Studio { recording, .. } => match &recording.meta {
            StudioRecordingMeta::SingleSegment { segment } => {
                segment.display.path.to_path(&recording_dir)
            }
            StudioRecordingMeta::MultipleSegments { inner, .. } => {
                inner.segments[0].display.path.to_path(&recording_dir)
            }
        },
        CompletedRecording::Instant { recording, .. } => {
            recording.project_path.join("./content/output.mp4")
        }
    };

    let display_screenshot = screenshots_dir.join("display.jpg");
    let screenshot_task = tokio::spawn(create_screenshot(
        display_output_path,
        display_screenshot.clone(),
        None,
    ));

    let target_name = completed_recording.target_name().clone();

    let (meta_inner, sharing) = match completed_recording {
        CompletedRecording::Studio { recording, .. } => {
            let recordings = ProjectRecordingsMeta::new(&recording_dir, &recording.meta)?;

            let config = project_config_from_recording(
                &recording,
                &recordings,
                PresetsStore::get_default_preset(&app)?.map(|p| p.config),
            );

            config.write(&recording_dir).map_err(|e| e.to_string())?;

            (RecordingMetaInner::Studio(recording.meta), None)
        }
        CompletedRecording::Instant {
            recording,
            progressive_upload,
            video_upload_info,
            ..
        } => {
            // shareable_link = Some(video_upload_info.link.clone());
            let app = app.clone();
            let output_path = recording_dir.join("content/output.mp4");

            let _ = open_external_link(app.clone(), video_upload_info.link.clone());

            spawn_actor({
                let video_upload_info = video_upload_info.clone();

                async move {
                    if let Some(progressive_upload) = progressive_upload {
                        let video_upload_succeeded = match progressive_upload
                            .handle
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r)
                        {
                            Ok(()) => {
                                info!("Not attempting instant recording upload as progressive upload succeeded");
                                true
                            }
                            Err(e) => {
                                error!("Progressive upload failed: {}", e);
                                false
                            }
                        };

                        let _ = screenshot_task.await;

                        if video_upload_succeeded {
                            let resp = prepare_screenshot_upload(
                                &app,
                                &video_upload_info.config.clone(),
                                display_screenshot,
                            )
                            .await;

                            match resp {
                                Ok(r)
                                    if r.status().as_u16() >= 200 && r.status().as_u16() < 300 =>
                                {
                                    info!("Screenshot uploaded successfully");
                                }
                                Ok(r) => {
                                    error!("Failed to upload screenshot: {}", r.status());
                                }
                                Err(e) => {
                                    error!("Failed to upload screenshot: {e}");
                                }
                            }
                        } else {
                            // The upload_video function handles screenshot upload, so we can pass it along
                            match upload_video(
                                &app,
                                video_upload_info.id.clone(),
                                output_path,
                                Some(video_upload_info.config.clone()),
                                Some(display_screenshot.clone()),
                            )
                            .await
                            {
                                Ok(_) => {
                                    info!(
                                        "Final video upload with screenshot completed successfully"
                                    )
                                }
                                Err(e) => {
                                    error!("Error in final upload with screenshot: {}", e)
                                }
                            }
                        }
                    }
                }
            });

            (
                RecordingMetaInner::Instant(recording.meta),
                Some(SharingMeta {
                    link: video_upload_info.link,
                    id: video_upload_info.id,
                }),
            )
        }
    };

    let meta = RecordingMeta {
        platform: Some(Platform::default()),
        project_path: recording_dir.clone(),
        sharing,
        pretty_name: format!(
            "{target_name} {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        ),
        inner: meta_inner,
    };

    meta.save_for_project()
        .map_err(|e| format!("Failed to save recording meta: {e}"))?;

    if let RecordingMetaInner::Studio(_) = meta.inner {
        match GeneralSettingsStore::get(&app)
            .ok()
            .flatten()
            .map(|v| v.post_studio_recording_behaviour)
            .unwrap_or(PostStudioRecordingBehaviour::OpenEditor)
        {
            PostStudioRecordingBehaviour::OpenEditor => {
                let _ = ShowCapWindow::Editor {
                    project_path: recording_dir,
                }
                .show(&app)
                .await;
            }
            PostStudioRecordingBehaviour::ShowOverlay => {
                let _ = ShowCapWindow::RecordingsOverlay.show(&app).await;

                let app = AppHandle::clone(app);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(1000)).await;

                    let _ = NewStudioRecordingAdded {
                        path: recording_dir.clone(),
                    }
                    .emit(&app);
                });
            }
        };
    }

    // Play sound to indicate recording has stopped
    AppSounds::StopRecording.play();

    Ok(())
}

fn generate_zoom_segments_from_clicks(
    recording: &CompletedStudioRecording,
    recordings: &ProjectRecordingsMeta,
) -> Vec<ZoomSegment> {
    let mut segments = vec![];

    let max_duration = recordings.duration();

    const ZOOM_SEGMENT_AFTER_CLICK_PADDING: f64 = 1.5;

    // single-segment only
    // for click in &recording.cursor_data.clicks {
    //     let time = click.process_time_ms / 1000.0;

    //     if segments.last().is_none() {
    //         segments.push(ZoomSegment {
    //             start: (click.process_time_ms / 1000.0 - (ZOOM_DURATION + 0.2)).max(0.0),
    //             end: click.process_time_ms / 1000.0 + ZOOM_SEGMENT_AFTER_CLICK_PADDING,
    //             amount: 2.0,
    //         });
    //     } else {
    //         let last_segment = segments.last_mut().unwrap();

    //         if click.down {
    //             if last_segment.end > time {
    //                 last_segment.end =
    //                     (time + ZOOM_SEGMENT_AFTER_CLICK_PADDING).min(recordings.duration());
    //             } else if time < max_duration - ZOOM_DURATION {
    //                 segments.push(ZoomSegment {
    //                     start: (time - ZOOM_DURATION).max(0.0),
    //                     end: time + ZOOM_SEGMENT_AFTER_CLICK_PADDING,
    //                     amount: 2.0,
    //                 });
    //             }
    //         } else {
    //             last_segment.end =
    //                 (time + ZOOM_SEGMENT_AFTER_CLICK_PADDING).min(recordings.duration());
    //         }
    //     }
    // }

    segments
}

fn project_config_from_recording(
    completed_recording: &CompletedStudioRecording,
    recordings: &ProjectRecordingsMeta,
    default_config: Option<ProjectConfiguration>,
) -> ProjectConfiguration {
    ProjectConfiguration {
        timeline: Some(TimelineConfiguration {
            segments: recordings
                .segments
                .iter()
                .enumerate()
                .map(|(i, segment)| TimelineSegment {
                    recording_segment: i as u32,
                    start: 0.0,
                    end: segment.duration(),
                    timescale: 1.0,
                })
                .collect(),
            zoom_segments: generate_zoom_segments_from_clicks(&completed_recording, &recordings),
        }),
        ..default_config.unwrap_or_default()
    }
}
