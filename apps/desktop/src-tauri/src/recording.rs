use std::path::{Path, PathBuf};
use std::collections::HashSet;
use std::io::{self, BufReader, BufRead, ErrorKind};
use std::fs::File;
use std::sync::Arc;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Semaphore, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use serde::{Serialize, Deserialize};
use tauri::State;

use crate::utils::ffmpeg_path_as_str;
use crate::upload::upload_file;

pub struct RecordingState {
  pub screen_process: Option<tokio::process::Child>,
  pub video_process: Option<tokio::process::Child>,
  pub upload_handles: Mutex<Vec<JoinHandle<Result<(), String>>>>,
  pub recording_options: Option<RecordingOptions>,
  pub shutdown_flag: Arc<AtomicBool>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecordingOptions {
  pub user_id: String,
  pub video_id: String,
  pub screen_index: String,
  pub video_index: String,
  pub aws_region: String,
  pub aws_bucket: String,
  pub framerate: String,
  pub resolution: String,
}

#[tauri::command]
pub async fn start_dual_recording(
  state: State<'_, Arc<Mutex<RecordingState>>>,
  options: RecordingOptions,
) -> Result<(), String> {
  println!("Starting screen recording...");

  let shutdown_flag = Arc::new(AtomicBool::new(false));

  let ffmpeg_binary_path_str = ffmpeg_path_as_str()?;
  
  let screen_chunks_dir = std::env::current_dir()
      .map_err(|_| "Cannot get current directory".to_string())?
      .join("chunks/screen");

  let video_chunks_dir = std::env::current_dir()
      .map_err(|_| "Cannot get current directory".to_string())?
      .join("chunks/video");

  clean_and_create_dir(&screen_chunks_dir)?;
  clean_and_create_dir(&video_chunks_dir)?;

  let ffmpeg_screen_args_future = construct_recording_args(&options, &screen_chunks_dir, "screen", &options.screen_index);
  let ffmpeg_video_args_future = construct_recording_args(&options, &video_chunks_dir, "video", &options.video_index);
  let ffmpeg_screen_args = ffmpeg_screen_args_future.await.map_err(|e| e.to_string())?;
  let ffmpeg_video_args = ffmpeg_video_args_future.await.map_err(|e| e.to_string())?;
  
  println!("Screen args: {:?}", ffmpeg_screen_args);
  println!("Video args: {:?}", ffmpeg_video_args);

  let mut screen_child = tokio::process::Command::new(&ffmpeg_binary_path_str)
      .args(&ffmpeg_screen_args)
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .map_err(|e| e.to_string())?;

  let mut video_child = tokio::process::Command::new(&ffmpeg_binary_path_str)
      .args(&ffmpeg_video_args)
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .map_err(|e| e.to_string())?;

  let screen_stdout = screen_child.stdout.take().unwrap();
  let screen_stderr = screen_child.stderr.take().unwrap();
  tokio::spawn(log_output(screen_stdout, "Screen stdout".to_string()));
  tokio::spawn(log_output(screen_stderr, "Screen stderr".to_string()));

  let video_stdout = video_child.stdout.take().unwrap();
  let video_stderr = video_child.stderr.take().unwrap();
  tokio::spawn(log_output(video_stdout, "Video stdout".to_string()));
  tokio::spawn(log_output(video_stderr, "Video stderr".to_string()));

  let mut guard = state.lock().await;
  guard.screen_process = Some(screen_child);
  guard.video_process = Some(video_child);
  guard.upload_handles = Mutex::new(vec![]);
  guard.recording_options = Some(options.clone());
  guard.shutdown_flag = shutdown_flag.clone();

  drop(guard);

  tokio::join!(
      start_upload_loop(state.clone(), screen_chunks_dir, options.clone(), "screen".to_string(), shutdown_flag.clone()),
      start_upload_loop(state.clone(), video_chunks_dir, options.clone(), "video".to_string(), shutdown_flag.clone()),
  );
    
  Ok(())
}

#[tauri::command]
pub async fn stop_all_recordings(state: State<'_, Arc<Mutex<RecordingState>>>) -> Result<(), String> {
    println!("!!STOPPING screen recording...");

    let mut guard = state.lock().await;

    guard.shutdown_flag.store(true, Ordering::SeqCst);
    
    if let Some(child_process) = &mut guard.screen_process {
      if let Err(e) = child_process.kill().await {
          eprintln!("Failed to kill the child process: {}", e);
      } else {
          println!("Child process terminated successfully.");
      }
    }
    if let Some(child_process) = &mut guard.video_process {
      if let Err(e) = child_process.kill().await {
          eprintln!("Failed to kill the child process: {}", e);
      } else {
          println!("Child process terminated successfully.");
      }
    }
    
    guard.screen_process = None;
    guard.video_process = None;

    let chunks_dir_screen = std::env::current_dir()
        .map_err(|e| format!("Cannot get current directory: {}", e))?
        .join("chunks/screen");

    let chunks_dir_video = std::env::current_dir()
        .map_err(|e| format!("Cannot get current directory: {}", e))?
        .join("chunks/video");

    let recording_options = guard.recording_options.clone();

    drop(guard);

    // Create join handles for the final uploads
    let handle_screen = upload_remaining_chunks(&chunks_dir_screen, recording_options.clone(), "screen");
    let handle_video = upload_remaining_chunks(&chunks_dir_video, recording_options.clone(), "video");

    // Await the final upload tasks
    tokio::select! {
        // Await either completion or an error from the screen upload task
        result = handle_screen => {
            if let Err(e) = result {
                eprintln!("Error uploading remaining screen chunks: {}", e);
            }
        }
        // Await either completion or an error from the video upload task
        result = handle_video => {
            if let Err(e) = result {
                eprintln!("Error uploading remaining video chunks: {}", e);
            }
        }
    }

    let guard = state.lock().await;
    let mut upload_handles = guard.upload_handles.lock().await;

    // Drain the upload_handles to get ownership of the JoinHandles
    let handles: Vec<_> = upload_handles.drain(..).collect();

    // Explicitly drop the locks before awaiting the handles
    drop(upload_handles);
    drop(guard);

    // Await each of the JoinHandles to completion
    for handle in handles {
        let _ = handle.await.map_err(|e| e.to_string())?;
    }

    // All checks and uploads are done, return Ok(())
    Ok(())
}

fn clean_and_create_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.path().is_file() {
                std::fs::remove_file(entry.path()).map_err(|e| e.to_string())?;
            }
        }
    } else {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn log_output(reader: impl tokio::io::AsyncRead + Unpin + Send + 'static, desc: String) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut reader = BufReader::new(reader).lines();

    while let Ok(Some(line)) = reader.next_line().await {
        println!("{}: {}", desc, line);
    }
}

async fn construct_recording_args(
    options: &RecordingOptions,
    chunks_dir: &Path, 
    video_type: &str,
    input_index: &str, 
) -> Result<Vec<String>, String> {
    let output_filename_pattern = format!("{}/recording_chunk_%03d.mkv", chunks_dir.display());
    let segment_list_filename = format!("{}/segment_list.txt", chunks_dir.display());
    
    ensure_segment_list_exists(PathBuf::from(&segment_list_filename))
        .map_err(|e| format!("Failed to ensure segment list file exists: {}", e))?;
      
    let fps = if video_type == "screen" { "60" } else { &options.framerate };
    let preset = "ultrafast".to_string();
    let crf = "28".to_string();
    let pix_fmt = "nv12".to_string();
    let codec = "libx264".to_string();
    let gop = "30".to_string();
    let segment_time = "3".to_string();
    let segment_list_type = "flat".to_string();
    let input_string = format!("{}:none", input_index);

    match std::env::consts::OS {
        "macos" => {
            if video_type == "screen" {
                Ok(vec![
                    "-f".to_string(), "avfoundation".to_string(),
                    "-framerate".to_string(), fps.to_string(),
                    "-i".to_string(), input_string.to_string(),
                    "-c:v".to_string(), codec,
                    "-preset".to_string(), preset,
                    "-pix_fmt".to_string(), pix_fmt,
                    "-g".to_string(), gop,
                    "-r".to_string(), fps.to_string(),
                    "-f".to_string(), "segment".to_string(),
                    "-segment_time".to_string(), segment_time,
                    "-segment_format".to_string(), "matroska".to_string(),
                    "-segment_list".to_string(), segment_list_filename,
                    "-segment_list_type".to_string(), segment_list_type,
                    "-reset_timestamps".to_string(), "1".to_string(),
                    output_filename_pattern,
                ])
            } else {
                Ok(vec![
                    "-f".to_string(), "avfoundation".to_string(),
                    "-video_size".to_string(), options.resolution.to_string(),
                    "-framerate".to_string(), fps.to_string(),
                    "-i".to_string(), input_string.to_string(),
                    "-c:v".to_string(), codec,
                    "-preset".to_string(), preset,
                    "-pix_fmt".to_string(), pix_fmt,
                    "-g".to_string(), gop,
                    "-r".to_string(), fps.to_string(),
                    "-f".to_string(), "segment".to_string(),
                    "-segment_time".to_string(), segment_time,
                    "-segment_format".to_string(), "matroska".to_string(),
                    "-segment_list".to_string(), segment_list_filename,
                    "-segment_list_type".to_string(), segment_list_type,
                    "-reset_timestamps".to_string(), "1".to_string(),
                    output_filename_pattern,
                ])
            }
        },
        "linux" => {
            if video_type == "screen" {
                Ok(vec![
                    "-f".to_string(), "x11grab".to_string(),
                    "-i".to_string(), format!("{}+0,0", input_index),
                    "-draw_mouse".to_string(), "1".to_string(),
                    "-pix_fmt".to_string(), pix_fmt,
                    "-c:v".to_string(), codec,
                    "-crf".to_string(), crf,
                    "-preset".to_string(), preset,
                    "-g".to_string(), gop,
                    "-r".to_string(), fps.to_string(),
                    "-f".to_string(), "segment".to_string(),
                    "-segment_time".to_string(), segment_time,
                    "-segment_format".to_string(), "mpegts".to_string(),
                    "-segment_list".to_string(), segment_list_filename,
                    "-segment_list_type".to_string(), segment_list_type,
                    "-reset_timestamps".to_string(), "1".to_string(),
                    output_filename_pattern,
                ])
            } else {
                Ok(vec![
                    "-f".to_string(), "x11grab".to_string(),
                    "-i".to_string(), format!("{}+0,0", input_index),
                    "-pix_fmt".to_string(), pix_fmt,
                    "-c:v".to_string(), codec,
                    "-crf".to_string(), crf,
                    "-preset".to_string(), preset,
                    "-g".to_string(), gop,
                    "-r".to_string(), fps.to_string(),
                    "-f".to_string(), "segment".to_string(),
                    "-segment_time".to_string(), segment_time,
                    "-segment_format".to_string(), "mpegts".to_string(),
                    "-segment_list".to_string(), segment_list_filename,
                    "-segment_list_type".to_string(), segment_list_type,
                    "-reset_timestamps".to_string(), "1".to_string(),
                    output_filename_pattern,
                ])
            }
        },
        "windows" => {
            if video_type == "screen" {
                Ok(vec![
                    "-f".to_string(), "gdigrab".to_string(),
                    "-i".to_string(), "desktop".to_string(),
                    "-pixel_format".to_string(), pix_fmt,
                    "-c:v".to_string(), codec,
                    "-crf".to_string(), crf,
                    "-preset".to_string(), preset,
                    "-g".to_string(), gop,
                    "-r".to_string(), fps.to_string(),
                    "-f".to_string(), "segment".to_string(),
                    "-segment_time".to_string(), segment_time,
                    "-segment_format".to_string(), "mpegts".to_string(),
                    "-segment_list".to_string(), segment_list_filename,
                    "-segment_list_type".to_string(), segment_list_type,
                    "-reset_timestamps".to_string(), "1".to_string(),
                    output_filename_pattern,
                ])
            } else {
                Ok(vec![
                    "-f".to_string(), "dshow".to_string(),
                    "-i".to_string(), format!("video={}", input_index),
                    "-pixel_format".to_string(), pix_fmt,
                    "-c:v".to_string(), codec,
                    "-crf".to_string(), crf,
                    "-preset".to_string(), preset,
                    "-g".to_string(), gop,
                    "-r".to_string(), fps.to_string(),
                    "-f".to_string(), "segment".to_string(),
                    "-segment_time".to_string(), segment_time,
                    "-segment_format".to_string(), "mpegts".to_string(),
                    "-segment_list".to_string(), segment_list_filename,
                    "-segment_list_type".to_string(), segment_list_type,
                    "-reset_timestamps".to_string(), "1".to_string(),
                    output_filename_pattern,
                ])
            }
        },
        _ => Err("Unsupported OS".to_string()),
    }
}

async fn start_upload_loop(
    state: State<'_, Arc<Mutex<RecordingState>>>,
    chunks_dir: PathBuf,
    options: RecordingOptions,
    video_type: String,
    shutdown_flag: Arc<AtomicBool>,
) {
    let segment_list_path = chunks_dir.join("segment_list.txt");

    let mut watched_segments: HashSet<String> = HashSet::new();
    let upload_interval = std::time::Duration::from_secs(3);

    loop {
        if shutdown_flag.load(Ordering::SeqCst) {
            println!("Shutdown flag set, exiting upload loop for {}", video_type);
            break;
        }

        match load_segment_list(&segment_list_path) {
            Ok(new_segments) => {
                for segment_filename in new_segments {
                    let segment_path = chunks_dir.join(&segment_filename);

                    // Check if the segment is new and schedule it for upload
                    if segment_path.is_file() && watched_segments.insert(segment_filename.clone()) {
                        let filepath_str = segment_path.to_str().unwrap_or_default().to_owned();
                        let options_clone = options.clone();
                        let video_type_clone = video_type.clone();

                        let handle = tokio::spawn(async move {
                            // Log the file path and the video type in one print, starting with "Uploading video from"
                            println!("Uploading video for {}: {}", video_type_clone, filepath_str);
  
                            match upload_file(Some(options_clone.clone()), filepath_str.clone(), video_type_clone.clone()).await {
                                Ok(file_key) => {
                                    println!("Chunk uploaded: {}", file_key);
                                },
                                Err(e) => {
                                    eprintln!("Failed to upload chunk {}: {}", filepath_str, e);
                                }
                            }

                            Ok(())
                        });

                        // Store the handle in the state for later awaits or cancels if required.
                        let guard = state.lock().await;
                        guard.upload_handles.lock().await.push(handle);
                    }
                }
            }
            Err(e) => eprintln!("Failed to read segment list for {}: {}", video_type, e),
        }

        // Sleep for the interval before checking the segment list again
        tokio::time::sleep(upload_interval).await;
    }
}

fn ensure_segment_list_exists(file_path: PathBuf) -> io::Result<()> {
    match File::open(&file_path) {
        Ok(_) => (), 
        Err(ref e) if e.kind() == ErrorKind::NotFound => {
            File::create(&file_path)?;
        },
        Err(e) => {
            return Err(e);
        },
    }
    Ok(())
}

fn load_segment_list(segment_list_path: &Path) -> io::Result<HashSet<String>> {
    let file = File::open(segment_list_path)?;
    let reader = BufReader::new(file);

    let mut segments = HashSet::new();
    for line_result in reader.lines() {
        let line = line_result?;
        if !line.is_empty() {
            segments.insert(line);
        }
    }

    Ok(segments)
}

async fn upload_remaining_chunks(
    chunks_dir: &PathBuf,
    options: Option<RecordingOptions>,
    video_type: &str,
) -> Result<(), String> {
    if let Some(actual_options) = options {
        tokio::time::sleep(Duration::from_secs(1)).await;

        let retry_interval = Duration::from_secs(2);
        let upload_timeout = Duration::from_secs(15);
        let file_stability_timeout = Duration::from_secs(1);
        let file_stability_checks = 2;

        // Get directory entries
        let entries = std::fs::read_dir(chunks_dir).map_err(|e| format!("Error reading directory: {}", e))?;

        // A semaphore to limit the number of concurrent uploads
        let semaphore = Arc::new(Semaphore::new(8));

        // Create upload tasks for each file entry
        let tasks: Vec<_> = entries.filter_map(|entry| entry.ok())
            .map(|entry| {
                let path = entry.path();
                if path.is_file() && path.extension().map_or(false, |e| e == "mkv") {
                    let video_type = video_type.to_string();
                    let semaphore_clone = semaphore.clone();
                    let actual_options_clone = actual_options.clone();

                    // Spawn a task to upload the file
                    Some(tokio::spawn(async move {
                        let _permit = semaphore_clone.acquire().await;
                        let filepath_str = path.to_str().unwrap_or_default().to_owned();

                        // Check for file size stability
                        let mut last_size = 0;
                        let mut stable_count = 0;
                        while stable_count < file_stability_checks {
                            if !path.exists() {
                                eprintln!("File does not exist: {}", path.display());
                                break; // Exit the loop if the file does not exist
                            }
                            match std::fs::metadata(&path) {
                                Ok(metadata) => {
                                    let current_size = metadata.len();
                                    if last_size == current_size {
                                        stable_count += 1;
                                    } else {
                                        last_size = current_size;
                                        stable_count = 0;
                                    }
                                },
                                Err(e) => {
                                    eprintln!("Failed to get file metadata: {}", e);
                                    break; // Exit the loop if any other error occurs
                                }
                            }
                            tokio::time::sleep(file_stability_timeout).await;
                        }

                        println!("File size stable: {}", filepath_str);

                        // Proceed with upload after confirming file stability
                        let mut attempts = 0;
                        // Retry loop with timeout
                        while attempts < 3 {
                            attempts += 1;
                            match timeout(upload_timeout, upload_file(Some(actual_options_clone.clone()), filepath_str.clone(), video_type.clone())).await {
                                Ok(Ok(_)) => {
                                    // Upload succeeded
                                    println!("Successful upload on attempt {}", attempts);
                                    break; // Break out of the loop on success
                                }
                                Ok(Err(e)) => {
                                    // Upload failed but did not timeout
                                    eprintln!("Failed to upload (attempt {}): {}", attempts, e);
                                }
                                Err(_) => {
                                    // Upload attempt timed out
                                    eprintln!("Upload attempt timed out (attempt {})", attempts);
                                }
                            }
                            // Wait for retry_interval before retrying
                            tokio::time::sleep(retry_interval).await;
                        }
                    }))
                } else {
                    None
                }
            })
            .collect();

        // Wait for all the tasks to finish
        for task in tasks {
            if let Some(handle) = task {
                if let Err(e) = handle.await {
                    eprintln!("Failed to join task: {:?}", e);
                }
            }
        }

        Ok(())
    } else {
        Err("No recording options provided".to_string())
    }
}