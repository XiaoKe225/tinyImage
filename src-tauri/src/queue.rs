//! 批量队列：并发 5、单任务超时、可取消、进度事件。

use crate::compress::{self, CompressResult};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Semaphore;

pub const DEFAULT_CONCURRENCY: usize = 5;
pub const MAX_BATCH: usize = 300;
pub const TASK_TIMEOUT_SECS: u64 = 120;

#[derive(Clone)]
pub struct CancelFlag(pub Arc<AtomicBool>);

impl Default for CancelFlag {
    fn default() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub done: usize,
    pub total: usize,
    pub success: usize,
    pub failed: usize,
    pub skipped: usize,
    pub saved_bytes: i64,
    pub current_path: Option<String>,
    pub last_error: Option<String>,
    pub cancelled: bool,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FailItem {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BatchSummary {
    pub total: usize,
    pub success: usize,
    pub failed: usize,
    pub skipped: usize,
    pub saved_bytes: i64,
    pub cancelled: bool,
    pub failures: Vec<FailItem>,
    /// 跳过项（含「无变小 / 保真门禁」等 method 说明）
    pub skips: Vec<FailItem>,
}

fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase()
            .as_str(),
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "ico"
    )
}

fn walk_dir(dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("无法读取目录 {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("读取目录项失败: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir(&path, out)?;
        } else if path.is_file() && is_image_path(&path) {
            out.push(path.display().to_string());
            if out.len() > MAX_BATCH {
                return Err(format!("图片超过 {MAX_BATCH} 张，请缩小范围后重试"));
            }
        }
    }
    Ok(())
}

/// 从拖入的文件/文件夹路径收集支持格式，上限 300。
pub fn collect_images(paths: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for raw in paths {
        let path = PathBuf::from(raw);
        if !path.exists() {
            return Err(format!("路径不存在: {raw}"));
        }
        if path.is_file() {
            if is_image_path(&path) {
                out.push(path.display().to_string());
            }
        } else if path.is_dir() {
            walk_dir(&path, &mut out)?;
        }
        if out.len() > MAX_BATCH {
            return Err(format!("图片超过 {MAX_BATCH} 张，请缩小范围后重试"));
        }
    }
    out.sort();
    out.dedup();
    if out.is_empty() {
        return Err("没有找到 JPG/PNG 图片".into());
    }
    if out.len() > MAX_BATCH {
        return Err(format!("图片超过 {MAX_BATCH} 张，请缩小范围后重试"));
    }
    Ok(out)
}

async fn compress_one_timed(path: String, intensity: u8) -> Result<CompressResult, String> {
    let handle =
        tauri::async_runtime::spawn_blocking(move || compress::compress_file(&path, intensity));
    match tokio::time::timeout(std::time::Duration::from_secs(TASK_TIMEOUT_SECS), handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => Err(format!("压缩任务异常: {join_err}")),
        Err(_) => Err(format!("压缩超时（{TASK_TIMEOUT_SECS}s）")),
    }
}

/// 并发压缩队列（默认 5）；可中途取消；进度经事件 `compress-progress` 推送。
pub async fn run_batch(
    app: AppHandle,
    cancel: State<'_, CancelFlag>,
    paths: Vec<String>,
    intensity: u8,
) -> Result<BatchSummary, String> {
    let intensity = compress::clamp_intensity(intensity);
    cancel.0.store(false, Ordering::SeqCst);
    let flag = cancel.0.clone();
    let total = paths.len();
    let sem = Arc::new(Semaphore::new(DEFAULT_CONCURRENCY));
    let mut handles = Vec::with_capacity(total);

    for path in paths {
        let sem = sem.clone();
        let flag = flag.clone();
        handles.push(tauri::async_runtime::spawn(async move {
            let permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(e) => return (path, Err(format!("队列许可失败: {e}"))),
            };
            if flag.load(Ordering::SeqCst) {
                drop(permit);
                return (path, Err("已取消".into()));
            }
            let path_for_result = path.clone();
            let result = compress_one_timed(path, intensity).await;
            drop(permit);
            (path_for_result, result)
        }));
    }

    let mut success = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut saved_bytes = 0i64;
    let mut failures = Vec::new();
    let mut skips = Vec::new();
    let mut done = 0usize;
    let mut cancelled_count = 0usize;

    for handle in handles {
        match handle.await {
            Ok((path, Ok(res))) => {
                done += 1;
                if res.skipped {
                    skipped += 1;
                    skips.push(FailItem {
                        path: path.clone(),
                        error: res.method.clone(),
                    });
                } else {
                    success += 1;
                    saved_bytes += res.saved_bytes;
                }
                let _ = app.emit(
                    "compress-progress",
                    ProgressEvent {
                        done,
                        total,
                        success,
                        failed,
                        skipped,
                        saved_bytes,
                        current_path: Some(path),
                        last_error: None,
                        cancelled: flag.load(Ordering::SeqCst),
                    },
                );
            }
            Ok((path, Err(err))) => {
                done += 1;
                if err == "已取消" {
                    cancelled_count += 1;
                } else {
                    failed += 1;
                    failures.push(FailItem {
                        path: path.clone(),
                        error: err.clone(),
                    });
                }
                let _ = app.emit(
                    "compress-progress",
                    ProgressEvent {
                        done,
                        total,
                        success,
                        failed,
                        skipped,
                        saved_bytes,
                        current_path: Some(path),
                        last_error: Some(err),
                        cancelled: flag.load(Ordering::SeqCst),
                    },
                );
            }
            Err(e) => {
                done += 1;
                failed += 1;
                let msg = format!("任务 join 失败: {e}");
                failures.push(FailItem {
                    path: String::new(),
                    error: msg.clone(),
                });
                let _ = app.emit(
                    "compress-progress",
                    ProgressEvent {
                        done,
                        total,
                        success,
                        failed,
                        skipped,
                        saved_bytes,
                        current_path: None,
                        last_error: Some(msg),
                        cancelled: flag.load(Ordering::SeqCst),
                    },
                );
            }
        }
    }

    let cancelled = flag.load(Ordering::SeqCst) || cancelled_count > 0;
    Ok(BatchSummary {
        total,
        success,
        failed,
        skipped,
        saved_bytes,
        cancelled,
        failures,
        skips,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn collect_skips_non_image_files() {
        let dir = std::env::temp_dir().join("tinyimage_collect_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), b"x").unwrap();
        fs::write(dir.join("b.jpg"), b"not-real-jpg").unwrap();
        let list = collect_images(&[dir.display().to_string()]).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].ends_with("b.jpg"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_rejects_empty() {
        let dir = std::env::temp_dir().join("tinyimage_collect_empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let err = collect_images(&[dir.display().to_string()]).unwrap_err();
        assert!(err.contains("没有找到"));
        let _ = fs::remove_dir_all(&dir);
    }
}
