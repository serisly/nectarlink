// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows 10 DirectShow virtual camera backend (UnityCapture filter protocol).
//!
//! Windows 10 lacks `MFCreateVirtualCamera` (introduced in Windows 11 build 22000).
//! On Windows 10, NectarLink registers the bundled MIT/zlib-licensed DirectShow
//! virtual camera filters (`UnityCaptureFilter64.dll` and `UnityCaptureFilter32.dll`)
//! under the display name `"NectarLink Webcam"` and writes decoded webcam frames
//! directly into the filter's shared-memory transport (`UnityCapture_Data`).

use std::{
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0},
        System::{
            Memory::{
                FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW,
                UnmapViewOfFile,
            },
            Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
            Threading::{
                CreateEventW, EVENT_MODIFY_STATE, OpenEventW, OpenMutexW, ReleaseMutex,
                SYNCHRONIZATION_ACCESS_RIGHTS, SetEvent, WaitForSingleObject,
            },
        },
    },
    core::{HSTRING, PCWSTR, w},
};

/// DirectShow filter CLSID exported by `UnityCaptureFilter{32,64}.dll`.
pub const DSHOW_CLSID_STR: &str = "{5C2CD55C-92AD-4999-8666-912BD3E70010}";
const HKLM_CLSID_PREFIX: &str = r"SOFTWARE\Classes\CLSID";
const DSHOW_CATEGORY_PREFIX: &str =
    r"SOFTWARE\Classes\CLSID\{860BB310-5D01-11d0-BD3B-00A0C911CE86}\Instance";

const DLL_64: &str = "UnityCaptureFilter64.dll";
const DLL_32: &str = "UnityCaptureFilter32.dll";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SYNCHRONIZE_RIGHT: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);
const MUTEX_WAIT_MS: u32 = 50;
const REOPEN_BACKOFF: Duration = Duration::from_millis(150);

const FORMAT_UINT8: i32 = 0;
const RESIZEMODE_LINEAR: i32 = 1;
const MIRRORMODE_DISABLED: i32 = 0;
const FRAME_TIMEOUT_MS: i32 = 1000;

/// Exact C layout of `SharedImageMemory::SharedMemHeader` from UnityCapture `shared.inl`.
#[repr(C)]
struct SharedMemHeader {
    max_size: u32,
    width: i32,
    height: i32,
    stride: i32,
    format: i32,
    resize_mode: i32,
    mirror_mode: i32,
    timeout: i32,
}

const HEADER_SIZE: usize = std::mem::size_of::<SharedMemHeader>();
const _: () = assert!(HEADER_SIZE == 32);

/// Resolves `(UnityCaptureFilter64.dll, UnityCaptureFilter32.dll)` next to the running exe.
pub fn dll_paths() -> Option<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some((dir.join(DLL_64), dir.join(DLL_32)))
}

fn read_hklm_sz(subkey: &str, value_name: Option<&str>) -> Option<String> {
    let mut buf = [0u16; 512];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let name_h = value_name.map(HSTRING::from);
    let name_pcwstr = name_h.as_ref().map_or(PCWSTR::null(), |h| PCWSTR(h.as_ptr()));
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(subkey),
            name_pcwstr,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Returns `true` when the DirectShow filter is registered in HKLM and its DLL exists on disk.
pub fn is_registered() -> bool {
    let inproc = format!(r"{HKLM_CLSID_PREFIX}\{DSHOW_CLSID_STR}\InprocServer32");
    let Some(dll) = read_hklm_sz(&inproc, None) else {
        return false;
    };
    if !Path::new(&dll).is_file() {
        return false;
    }
    let cat = format!(r"{DSHOW_CATEGORY_PREFIX}\{DSHOW_CLSID_STR}");
    read_hklm_sz(&cat, Some("FriendlyName")).is_some()
}

fn regsvr32_exe(use_wow64: bool) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let sub = if use_wow64 { "SysWOW64" } else { "System32" };
    let candidate = PathBuf::from(&root).join(sub).join("regsvr32.exe");
    if candidate.is_file() { candidate } else { PathBuf::from("regsvr32.exe") }
}

fn run_regsvr32(dll: &Path, register: bool, use_wow64: bool) -> Result<(), String> {
    let mut cmd = Command::new(regsvr32_exe(use_wow64));
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.arg("/s");
    if !register {
        cmd.arg("/u");
    }
    cmd.arg(dll);
    if register {
        cmd.arg(format!("/i:UnityCaptureName={}", super::vcam::FRIENDLY_NAME));
    }
    let status = cmd
        .status()
        .map_err(|e| format!("failed to launch regsvr32 for {}: {e}", dll.display()))?;
    if !status.success() {
        return Err(format!(
            "regsvr32 failed (code {:?}) for {}",
            status.code(),
            dll.display()
        ));
    }
    Ok(())
}

/// Registers the DirectShow virtual camera filters as `"NectarLink Webcam"` (admin required).
pub fn register_hklm() -> Result<(), String> {
    let (dll64, dll32) =
        dll_paths().ok_or_else(|| "cannot resolve DirectShow virtual camera DLL paths".to_owned())?;
    if !dll64.is_file() {
        return Err(format!("missing {}", dll64.display()));
    }
    run_regsvr32(&dll64, true, false)?;
    if dll32.is_file()
        && let Err(e) = run_regsvr32(&dll32, true, true)
    {
        tracing::warn!(error = %e, "32-bit DirectShow filter registration failed; 64-bit apps still work");
    }
    Ok(())
}

/// Unregisters the DirectShow virtual camera filters from HKLM (admin required).
pub fn unregister_hklm() -> Result<(), String> {
    if let Some((dll64, dll32)) = dll_paths() {
        if dll64.is_file() {
            let _ = run_regsvr32(&dll64, false, false);
        }
        if dll32.is_file() {
            let _ = run_regsvr32(&dll32, false, true);
        }
    }
    // Also clean up if an older copy was registered from another path.
    let inproc = format!(r"{HKLM_CLSID_PREFIX}\{DSHOW_CLSID_STR}\InprocServer32");
    if let Some(prev) = read_hklm_sz(&inproc, None) {
        let p = PathBuf::from(prev);
        if p.is_file() {
            let _ = run_regsvr32(&p, false, false);
        }
    }
    Ok(())
}

/// RAII guard for a Win32 mutex acquired via `WaitForSingleObject`.
struct MutexLock(HANDLE);

impl MutexLock {
    fn acquire(handle: HANDLE, timeout_ms: u32) -> Option<Self> {
        let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            Some(Self(handle))
        } else {
            None
        }
    }
}

impl Drop for MutexLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
        }
    }
}

struct DShowInner {
    mutex: Option<HANDLE>,
    want_event: Option<HANDLE>,
    sent_event: Option<HANDLE>,
    mapping: Option<HANDLE>,
    view: Option<MEMORY_MAPPED_VIEW_ADDRESS>,
    last_probe: Option<Instant>,
    rgba_scratch: Vec<u8>,
}

impl DShowInner {
    fn new() -> Self {
        Self {
            mutex: None,
            want_event: None,
            sent_event: None,
            mapping: None,
            view: None,
            last_probe: None,
            rgba_scratch: Vec::new(),
        }
    }

    fn close_handles(&mut self) {
        if let Some(view) = self.view.take() {
            if let Some(mutex) = self.mutex
                && let Some(_guard) = MutexLock::acquire(mutex, MUTEX_WAIT_MS)
            {
                let hdr = unsafe { &mut *(view.Value as *mut SharedMemHeader) };
                hdr.width = 0;
                hdr.height = 0;
            }
            unsafe {
                let _ = UnmapViewOfFile(view);
            }
        }
        for slot in [&mut self.mapping, &mut self.sent_event, &mut self.want_event, &mut self.mutex] {
            if let Some(h) = slot.take() {
                unsafe {
                    let _ = CloseHandle(h);
                }
            }
        }
    }

    /// Connects lazily to the DirectShow filter's shared memory once a consumer app opens the camera.
    fn try_open(&mut self) -> bool {
        if self.view.is_some() {
            return true;
        }
        if let Some(prev) = self.last_probe
            && prev.elapsed() < REOPEN_BACKOFF
        {
            return false;
        }
        self.last_probe = Some(Instant::now());

        let Ok(mutex) = (unsafe { OpenMutexW(SYNCHRONIZE_RIGHT, false, w!("UnityCapture_Mutx")) })
        else {
            return false;
        };

        let opened = (|| -> Option<(HANDLE, HANDLE, HANDLE, MEMORY_MAPPED_VIEW_ADDRESS)> {
            let _guard = MutexLock::acquire(mutex, MUTEX_WAIT_MS)?;
            let want = unsafe { CreateEventW(None, false, false, w!("UnityCapture_Want")) }.ok()?;
            let Ok(sent) = (unsafe { OpenEventW(EVENT_MODIFY_STATE, false, w!("UnityCapture_Sent")) })
            else {
                unsafe {
                    let _ = CloseHandle(want);
                }
                return None;
            };
            let Ok(mapping) =
                (unsafe { OpenFileMappingW(FILE_MAP_WRITE.0, false, w!("UnityCapture_Data")) })
            else {
                unsafe {
                    let _ = CloseHandle(sent);
                    let _ = CloseHandle(want);
                }
                return None;
            };
            let view = unsafe { MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, 0) };
            if view.Value.is_null() {
                unsafe {
                    let _ = CloseHandle(mapping);
                    let _ = CloseHandle(sent);
                    let _ = CloseHandle(want);
                }
                return None;
            }
            Some((want, sent, mapping, view))
        })();

        match opened {
            Some((want, sent, mapping, view)) => {
                self.mutex = Some(mutex);
                self.want_event = Some(want);
                self.sent_event = Some(sent);
                self.mapping = Some(mapping);
                self.view = Some(view);
                tracing::info!("connected to Windows 10 DirectShow virtual camera shared memory");
                true
            }
            None => {
                unsafe {
                    let _ = CloseHandle(mutex);
                }
                false
            }
        }
    }
}

impl Drop for DShowInner {
    fn drop(&mut self) {
        self.close_handles();
    }
}

/// Converts top-down `BGRX` pixels into bottom-up `RGBA` pixels expected by UnityCaptureFilter.
fn bgrx_to_bottom_up_rgba(width: usize, height: usize, bgrx: &[u8], out: &mut Vec<u8>) {
    let row_bytes = width * 4;
    let total = row_bytes * height;
    out.resize(total, 0);
    for y in 0..height {
        let src_start = (height - 1 - y) * row_bytes;
        let dst_start = y * row_bytes;
        let src_row = &bgrx[src_start..src_start + row_bytes];
        let dst_row = &mut out[dst_start..dst_start + row_bytes];
        for (s, d) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = 255;
        }
    }
}

/// Thread-safe writer that publishes decoded webcam frames into the Windows 10 DirectShow filter.
pub struct DShowWriter {
    inner: Mutex<DShowInner>,
}

// SAFETY: Win32 kernel handles and mapped views are process-wide and synchronized by `inner`.
unsafe impl Send for DShowWriter {}
unsafe impl Sync for DShowWriter {}

impl std::fmt::Debug for DShowWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DShowWriter").finish()
    }
}

impl DShowWriter {
    pub fn new() -> Self {
        Self { inner: Mutex::new(DShowInner::new()) }
    }

    pub fn write_bgrx(&self, width: u32, height: u32, bgrx: &[u8]) {
        if width == 0 || height == 0 {
            return;
        }
        let Some(data_size) = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
        else {
            return;
        };
        if bgrx.len() < data_size {
            return;
        }

        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if !inner.try_open() {
            return;
        }
        let (Some(mutex), Some(sent_event), Some(view)) =
            (inner.mutex, inner.sent_event, inner.view)
        else {
            return;
        };

        bgrx_to_bottom_up_rgba(width as usize, height as usize, bgrx, &mut inner.rgba_scratch);

        if let Some(_guard) = MutexLock::acquire(mutex, MUTEX_WAIT_MS) {
            let hdr = unsafe { &mut *(view.Value as *mut SharedMemHeader) };
            if (hdr.max_size as usize) >= data_size {
                hdr.width = width as i32;
                hdr.height = height as i32;
                hdr.stride = width as i32;
                hdr.format = FORMAT_UINT8;
                hdr.resize_mode = RESIZEMODE_LINEAR;
                hdr.mirror_mode = MIRRORMODE_DISABLED;
                hdr.timeout = FRAME_TIMEOUT_MS;
                unsafe {
                    let dst = (view.Value as *mut u8).add(HEADER_SIZE);
                    std::ptr::copy_nonoverlapping(inner.rgba_scratch.as_ptr(), dst, data_size);
                }
            }
        }
        unsafe {
            let _ = SetEvent(sent_event);
        }
    }

    pub fn set_idle(&self, phone_name: &str) {
        let (w, h) = (1280u32, 720u32);
        let mut bgrx = vec![0u8; (w * h * 4) as usize];
        nectarlink_vcam::render_placeholder_bgrx(&mut bgrx, w, h, phone_name);
        self.write_bgrx(w, h, &bgrx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_header_is_32_bytes() {
        assert_eq!(std::mem::size_of::<SharedMemHeader>(), 32);
    }

    #[test]
    fn bgrx_to_bottom_up_rgba_flips_and_swizzles() {
        // 2x2 top-down BGRX:
        // row 0 (top):    [B=10, G=20, R=30, X=0], [B=40, G=50, R=60, X=0]
        // row 1 (bottom): [B=70, G=80, R=90, X=0], [B=1,  G=2,  R=3,  X=0]
        let bgrx = vec![
            10, 20, 30, 0, 40, 50, 60, 0,
            70, 80, 90, 0, 1, 2, 3, 0,
        ];
        let mut out = Vec::new();
        bgrx_to_bottom_up_rgba(2, 2, &bgrx, &mut out);
        assert_eq!(
            out,
            vec![
                // row 0 in output = former bottom row, swizzled to RGBA
                90, 80, 70, 255, 3, 2, 1, 255,
                // row 1 in output = former top row, swizzled to RGBA
                30, 20, 10, 255, 60, 50, 40, 255,
            ]
        );
    }
}
