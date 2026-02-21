use crate::audio_toolkit::decode_audio_file;
use crate::managers::history::HistoryManager;
use crate::managers::transcription::TranscriptionManager;
use log::{error, info};
use serde::Serialize;
use specta::Type;
use std::path::Path;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

const SUPPORTED_EXTENSIONS: &[&str] = &["wav", "mp3", "flac", "m4a", "aac", "ogg", "oga"];

#[derive(Serialize, Type)]
pub struct FileTranscriptionResult {
    pub text: String,
    pub file_name: String,
    pub duration_ms: u64,
}

#[derive(Clone, Serialize, Type)]
pub struct FileTranscriptionProgress {
    pub stage: String,
    pub message: Option<String>,
}

fn emit_progress(app: &AppHandle, stage: &str, message: Option<&str>) {
    let _ = app.emit(
        "file-transcription-progress",
        FileTranscriptionProgress {
            stage: stage.to_string(),
            message: message.map(|s| s.to_string()),
        },
    );
}

#[tauri::command]
#[specta::specta]
pub async fn transcribe_audio_file(
    app: AppHandle,
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
    history_manager: State<'_, Arc<HistoryManager>>,
    file_path: String,
) -> Result<FileTranscriptionResult, String> {
    let path = Path::new(&file_path);

    // Validate file exists
    if !path.exists() {
        return Err(format!("File not found: {}", file_path));
    }

    // Validate supported extension
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    if !SUPPORTED_EXTENSIONS.contains(&extension.as_str()) {
        return Err(format!(
            "Unsupported audio format: .{}. Supported: {}",
            extension,
            SUPPORTED_EXTENSIONS.join(", ")
        ));
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    info!("Starting file transcription: {}", file_name);

    // Stage 1: Decode audio file
    emit_progress(&app, "decoding", None);
    let path_owned = path.to_path_buf();
    let samples = tokio::task::spawn_blocking(move || decode_audio_file(&path_owned))
        .await
        .map_err(|e| format!("Decode task failed: {}", e))?
        .map_err(|e| format!("Failed to decode audio file: {}", e))?;

    // Stage 2: Ensure model is loaded
    emit_progress(&app, "loading_model", None);
    transcription_manager.initiate_model_load();

    // Stage 3: Transcribe
    emit_progress(&app, "transcribing", None);
    let start = std::time::Instant::now();
    let tm = transcription_manager.inner().clone();
    let samples_for_transcription = samples.clone();
    let text = tokio::task::spawn_blocking(move || tm.transcribe(samples_for_transcription))
        .await
        .map_err(|e| format!("Transcription task failed: {}", e))?
        .map_err(|e| format!("Transcription failed: {}", e))?;
    let duration_ms = start.elapsed().as_millis() as u64;

    // Stage 4: Save to history
    emit_progress(&app, "saving", None);
    if let Err(e) = history_manager
        .save_transcription(samples, text.clone(), None, None)
        .await
    {
        error!("Failed to save file transcription to history: {}", e);
        // Don't fail the whole operation for a history save error
    }

    info!(
        "File transcription complete: {} ({} ms)",
        file_name, duration_ms
    );

    Ok(FileTranscriptionResult {
        text,
        file_name,
        duration_ms,
    })
}
