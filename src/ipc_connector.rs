// ~/Sentinel/sentinel-addons/wallpaper/src/ipc_connector.rs

use serde::Deserialize;
use serde_json::Value;
use std::thread;
use std::time::Duration;
use windows::{
    core::HRESULT,
    core::PCWSTR,
    Win32::{
        System::Pipes::WaitNamedPipeW,
        Foundation::{HANDLE, INVALID_HANDLE_VALUE, CloseHandle, ERROR_BROKEN_PIPE, ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED},
        Storage::FileSystem::{
            CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            ReadFile, WriteFile,
        },
    },
};

use crate::{
    info, warn, error,
    utility::to_wstring,
    DEBUG_NAME,
};

#[derive(Debug, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    pub data: Option<Value>,
    pub error: Option<String>,
}

fn is_win32_error(err: &windows::core::Error, win32_code: u32) -> bool {
    err.code() == HRESULT::from_win32(win32_code)
}

/// Open the named pipe, retrying briefly on PIPE_BUSY.
/// Returns None if the pipe doesn't exist or can't be opened.
unsafe fn open_pipe(quick: bool) -> Option<HANDLE> {
    let name = to_wstring(r"\\.\pipe\sentinel");
    let pipe_name = PCWSTR(name.as_ptr());

    // Try up to `attempts` times with a short WaitNamedPipe in between.
    let attempts: u32 = if quick { 3 } else { 6 };

    for attempt in 0..attempts {
        let result = CreateFileW(
            pipe_name,
            (FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0) as u32,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        );

        match result {
            Ok(h) if h != INVALID_HANDLE_VALUE => return Some(h),
            Ok(h) => {
                // INVALID_HANDLE_VALUE — treat as failure
                warn!("[{}][IPC] CreateFileW returned INVALID_HANDLE_VALUE", DEBUG_NAME);
                let _ = CloseHandle(h);
            }
            Err(e) if is_win32_error(&e, ERROR_PIPE_BUSY.0) => {
                // All server instances are currently in use — wait briefly
                // for one to become free, then retry.
                let wait_ms = if quick { 500 } else { 2000 };
                info!("[{}][IPC] Pipe busy (attempt {}/{}), waiting {}ms",
                    DEBUG_NAME, attempt + 1, attempts, wait_ms);
                let _ = WaitNamedPipeW(pipe_name, wait_ms);
                continue;
            }
            Err(e) => {
                // Any other error (pipe doesn't exist, access denied, etc.)
                if attempt == 0 {
                    warn!("[{}][IPC] Failed to open pipe: {:?}", DEBUG_NAME, e);
                }
                return None;
            }
        }
    }

    warn!("[{}][IPC] Pipe busy after {} attempts", DEBUG_NAME, attempts);
    None
}

/// Sends a JSON IPC request to the Sentinel IPC server and returns the universal IpcResponse.
fn send_ipc_request_once(req: &Value, quick: bool) -> Option<IpcResponse> {
    unsafe {
        let handle = open_pipe(quick)?;

        // Serialize request
        let req_bytes = match serde_json::to_vec(req) {
            Ok(b) => b,
            Err(e) => {
                error!("[{}][IPC] Failed to serialize request JSON: {:?}", DEBUG_NAME, e);
                let _ = CloseHandle(handle);
                return None;
            }
        };

        // Write request
        let mut written: u32 = 0;
        if let Err(e) = WriteFile(handle, Some(&req_bytes), Some(&mut written), None) {
            if is_win32_error(&e, ERROR_BROKEN_PIPE.0) {
                warn!("[{}][IPC] Pipe closed while writing request", DEBUG_NAME);
            } else {
                warn!("[{}][IPC] Failed to write to pipe: {:?}", DEBUG_NAME, e);
            }
            let _ = CloseHandle(handle);
            return None;
        }

        // Read response
        let mut response = Vec::<u8>::new();
        loop {
            let mut chunk: Vec<u8> = vec![0u8; 64 * 1024];
            let mut read: u32 = 0;

            match ReadFile(handle, Some(&mut chunk), Some(&mut read), None) {
                Ok(_) => {
                    if read == 0 {
                        break;
                    }
                    response.extend_from_slice(&chunk[..read as usize]);
                }
                Err(e) => {
                    if read > 0 {
                        response.extend_from_slice(&chunk[..read as usize]);
                    }

                    if is_win32_error(&e, ERROR_MORE_DATA.0) {
                        continue;
                    }

                    // Broken pipe after accumulating data means the server
                    // closed its end — treat whatever we have as the full response.
                    if is_win32_error(&e, ERROR_BROKEN_PIPE.0)
                        || is_win32_error(&e, ERROR_PIPE_NOT_CONNECTED.0)
                        || is_win32_error(&e, ERROR_NO_DATA.0)
                    {
                        break;
                    }

                    warn!("[{}][IPC] Failed to read from pipe: {:?}", DEBUG_NAME, e);
                    let _ = CloseHandle(handle);
                    return None;
                }
            }
        }

        let _ = CloseHandle(handle);

        if response.is_empty() {
            warn!("[{}][IPC] Empty response from server", DEBUG_NAME);
            return None;
        }

        // Parse response
        match serde_json::from_slice::<IpcResponse>(&response) {
            Ok(v) => Some(v),
            Err(e) => {
                error!("[{}][IPC] Failed to parse IPC response JSON: {:?}", DEBUG_NAME, e);
                None
            }
        }
    }
}

pub fn request(ns: &str, cmd: &str, args: Option<serde_json::Value>) -> Option<String> {
    warn!("[{}][IPC] Sending request: ns={}, cmd={}", DEBUG_NAME, ns, cmd);

    let req = serde_json::json!({
        "ns": ns,
        "cmd": cmd,
        "args": args
    });

    if let Some(resp) = send_ipc_request(&req) {
        if resp.ok {
            if let Some(data) = resp.data {
                return Some(data.to_string());
            } else {
                warn!("[{}][IPC] No data field in response", DEBUG_NAME);
                return None;
            }
        } else {
            warn!("[{}][IPC] Error in response: {:?}", DEBUG_NAME, resp.error);
            return None;
        }
    } else {
        warn!("[{}][IPC] No IPC response received", DEBUG_NAME);
        return None;
    }
}

/// Quick request — single attempt, no retries.
/// Designed for the real-time tick loop where fast failure is preferred over
/// blocking for seconds on retries (the next tick will try again anyway).
pub fn request_quick(ns: &str, cmd: &str, args: Option<serde_json::Value>) -> Option<String> {
    let req = serde_json::json!({
        "ns": ns,
        "cmd": cmd,
        "args": args
    });

    if let Some(resp) = send_ipc_request_once(&req, true) {
        if resp.ok {
            if let Some(data) = resp.data {
                return Some(data.to_string());
            }
        }
    }

    None
}

fn send_ipc_request(req: &Value) -> Option<IpcResponse> {
    // Retry with increasing backoff: 200, 400, 800, 1600, 3200 ms
    let backoff = [200u64, 400, 800, 1600, 3200];

    if let Some(resp) = send_ipc_request_once(req, false) {
        return Some(resp);
    }

    for (i, delay) in backoff.iter().enumerate() {
        warn!(
            "[{}][IPC] Retry {}/{} after {}ms",
            DEBUG_NAME,
            i + 1,
            backoff.len(),
            delay
        );
        thread::sleep(Duration::from_millis(*delay));
        if let Some(resp) = send_ipc_request_once(req, false) {
            return Some(resp);
        }
    }

    error!("[{}][IPC] All retries exhausted — request failed", DEBUG_NAME);
    None
}