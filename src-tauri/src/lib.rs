mod compress;
mod formats_extra;
mod jpeg_lossless;
mod quality;
mod queue;

use queue::{BatchSummary, CancelFlag};
use std::sync::atomic::Ordering;

#[tauri::command]
fn collect_images(paths: Vec<String>) -> Result<Vec<String>, String> {
    queue::collect_images(&paths)
}

#[tauri::command]
async fn compress_batch(
    app: tauri::AppHandle,
    cancel: tauri::State<'_, CancelFlag>,
    paths: Vec<String>,
    intensity: Option<u8>,
) -> Result<BatchSummary, String> {
    let intensity = intensity.unwrap_or(compress::DEFAULT_INTENSITY);
    queue::run_batch(app, cancel, paths, intensity).await
}

#[tauri::command]
fn cancel_batch(cancel: tauri::State<'_, CancelFlag>) {
    cancel.0.store(true, Ordering::SeqCst);
}

#[tauri::command]
async fn compress_image(
    path: String,
    intensity: Option<u8>,
) -> Result<compress::CompressResult, String> {
    let intensity = intensity.unwrap_or(compress::DEFAULT_INTENSITY);
    let handle =
        tauri::async_runtime::spawn_blocking(move || compress::compress_file(&path, intensity));
    match tokio::time::timeout(std::time::Duration::from_secs(queue::TASK_TIMEOUT_SECS), handle)
        .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => Err(format!("压缩任务异常: {join_err}")),
        Err(_) => Err(format!(
            "压缩超时（{}s），已中止该文件",
            queue::TASK_TIMEOUT_SECS
        )),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(CancelFlag::default())
        .invoke_handler(tauri::generate_handler![
            collect_images,
            compress_batch,
            cancel_batch,
            compress_image
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
