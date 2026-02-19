// ~/Sentinel/sentinel-addons/wallpaper/src/ipc_connector.rs

use serde::{Deserialize};
use serde_json::Value;
use windows::{
    core::{
        PCWSTR,
    },
    Win32::{
        System::Pipes::WaitNamedPipeW,
        Foundation::{HANDLE, INVALID_HANDLE_VALUE, CloseHandle},
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

/// Sends a JSON IPC request to the Sentinel IPC server and returns the universal IpcResponse.
fn send_ipc_request(req: &Value) -> Option<IpcResponse> {
    unsafe {
        let name = to_wstring(r"\\.\pipe\sentinel");
        let pipe_name = PCWSTR(name.as_ptr());

        // Wait for server
        if !WaitNamedPipeW(pipe_name, 5000).as_bool() {
            warn!("[{}][IPC] WaitNamedPipe failed or timed out", DEBUG_NAME);
            return None;
        }

        // Open pipe
        let handle: HANDLE = match CreateFileW(
            pipe_name,
            (FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0) as u32,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                error!("[{}][IPC] Failed to open pipe: {:?}", DEBUG_NAME, e);
                return None;
            }
        };

        if handle == INVALID_HANDLE_VALUE {
            error!("[{}][IPC] Invalid handle returned from CreateFileW", DEBUG_NAME);
            return None;
        }

        // Serialize request
        let req_bytes = match serde_json::to_vec(req) {
            Ok(b) => b,
            Err(e) => {
                error!("[{}][IPC] Failed to serialize request JSON: {:?}", DEBUG_NAME, e);
                if let Err(e2) = CloseHandle(handle) { warn!("[{}][IPC] CloseHandle failed: {:?}", DEBUG_NAME, e2); }
                return None;
            }
        };
        // Write request
        let mut written: u32 = 0;
        if WriteFile(handle, Some(&req_bytes), Some(&mut written), None).is_err() {
            error!("[{}][IPC] Failed to write to pipe", DEBUG_NAME);
            if let Err(e2) = CloseHandle(handle) { warn!("[{}][IPC] CloseHandle failed: {:?}", DEBUG_NAME, e2); }
            return None;
        }
        let mut buffer: Vec<u8> = vec![0u8; 16 * 1024];
        let mut read: u32 = 0;
        if ReadFile(handle, Some(&mut buffer), Some(&mut read), None).is_err() {
            error!("[{}][IPC] Failed to read from pipe", DEBUG_NAME);
            if let Err(e2) = CloseHandle(handle) { warn!("[{}][IPC] CloseHandle failed: {:?}", DEBUG_NAME, e2); }
            return None;
        }

        // Close handle
        if let Err(e2) = CloseHandle(handle) { warn!("[{}][IPC] CloseHandle failed: {:?}", DEBUG_NAME, e2); }

        // Parse response
        match serde_json::from_slice::<IpcResponse>(&buffer[..read as usize]) {
            Ok(v) => Some(v),
            Err(e) => {
                error!("[{}][IPC] Failed to parse IPC response JSON: {:?}", DEBUG_NAME, e);
                None
            }
        }
    }
}

pub fn request(ns: &str, cmd: &str, args: Option<serde_json::Value>) -> Option<String> {
    info!("[{}][IPC] Sending request: ns={}, cmd={}", DEBUG_NAME, ns, cmd);

    let req = serde_json::json!({
        "ns": ns,
        "cmd": cmd,
        "args": args
    });

    if let Some(resp) = send_ipc_request(&req) {
        if resp.ok {
            if let Some(data) = resp.data {
                // Return the response data as JSON string
                return Some(data.to_string());
            } else {
                info!("[{}][IPC] No data field in response", DEBUG_NAME);
                return None;
            }
        } else {
            info!("[{}][IPC] Error in response: {:?}", DEBUG_NAME, resp.error);
            return None;
        }
    } else {
        info!("[{}][IPC] No IPC response received", DEBUG_NAME);
        return None;
    }
}