//! Custom Windows ConPTY backend. Helped correct multiple quirks around
//! resizing.
//!
//! Credit: the flag selection, version probe, and overall structure are
//! modelled after rmux's ConPTY backend by Helvesec - see
//! <https://github.com/Helvesec/rmux/tree/main/crates/rmux-pty/src/backend/windows>
//! (dual-licensed MIT/Apache-2.0).

use std::ffi::OsStr;
use std::io::{Error as IoError, Read, Write};
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use log::{debug, warn};
use windows_sys::Win32::Foundation::{
    FALSE, HANDLE, INVALID_HANDLE_VALUE, S_OK, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOEXW;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::CommandBuilder;

// ---------------------------------------------------------------------------
// ConPTY compatibility flags
// ---------------------------------------------------------------------------

const PSEUDOCONSOLE_INHERIT_CURSOR: u32 = 0x1;
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;
const PSEUDOCONSOLE_WIN32_INPUT_MODE: u32 = 0x4;
const PSEUDOCONSOLE_PASSTHROUGH_MODE: u32 = 0x8;

/// Minimum Windows 11 build that supports `PSEUDOCONSOLE_PASSTHROUGH_MODE`.
const PASSTHROUGH_MIN_BUILD: u32 = 22621;

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x20016;

/// Environment escape hatch: set to `1` to disable PASSTHROUGH_MODE even on
/// supported builds.
const DISABLE_PASSTHROUGH_ENV: &str = "SSHMUX_NO_CONPTY_PASSTHROUGH";

/// Selects the ConPTY flag bitmask for the current Windows version.
fn selected_flags() -> u32 {
    let mut flags =
        PSEUDOCONSOLE_INHERIT_CURSOR | PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE;
    let build = windows_build();
    let disabled = std::env::var(DISABLE_PASSTHROUGH_ENV)
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"));
    if !disabled && build >= PASSTHROUGH_MIN_BUILD {
        flags |= PSEUDOCONSOLE_PASSTHROUGH_MODE;
    }
    debug!(
        "ConPTY flags=0x{flags:x} (build={build}, passthrough={})",
        flags & PSEUDOCONSOLE_PASSTHROUGH_MODE != 0
    );
    flags
}

#[link(name = "ntdll")]
unsafe extern "system" {
    /// `RtlGetVersion` reports the real Windows version (unlike `GetVersionExW`,
    /// which lies about anything past Windows 8 unless your manifest opts in).
    fn RtlGetVersion(version_information: *mut OSVERSIONINFOEXW) -> i32;
}

fn windows_build() -> u32 {
    // SAFETY: writes only into `info`, which has the size field set as required.
    let mut info: OSVERSIONINFOEXW = unsafe { mem::zeroed() };
    info.dwOSVersionInfoSize = mem::size_of::<OSVERSIONINFOEXW>() as u32;
    let status = unsafe { RtlGetVersion(&mut info) };
    if status < 0 {
        return 0;
    }
    info.dwBuildNumber
}

// ---------------------------------------------------------------------------
// Pipes and handle helpers
// ---------------------------------------------------------------------------

struct PipePair {
    read: OwnedHandle,
    write: OwnedHandle,
}

fn create_pipe() -> Result<PipePair> {
    let mut read: HANDLE = INVALID_HANDLE_VALUE;
    let mut write: HANDLE = INVALID_HANDLE_VALUE;
    let sa = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: FALSE,
    };
    // SAFETY: out-pointers `read` / `write` are writable; CreatePipe fills them.
    let ok = unsafe { CreatePipe(&mut read, &mut write, &sa, 0) };
    if ok == 0 {
        return Err(anyhow!("CreatePipe failed: {}", IoError::last_os_error()));
    }
    Ok(PipePair {
        // SAFETY: handles were just allocated by CreatePipe and are non-null.
        read: unsafe { OwnedHandle::from_raw_handle(read as _) },
        write: unsafe { OwnedHandle::from_raw_handle(write as _) },
    })
}

fn try_clone_handle(handle: &OwnedHandle) -> std::io::Result<OwnedHandle> {
    handle.try_clone()
}

// ---------------------------------------------------------------------------
// Read/Write streams over a pipe handle
// ---------------------------------------------------------------------------

/// Owned read end of a pipe; implements `Read` for the reader thread.
pub struct PipeReader(OwnedHandle);

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut bytes_read: u32 = 0;
        // SAFETY: handle is owned and live; buf is a unique writable slice.
        let ok = unsafe {
            ReadFile(
                self.0.as_raw_handle() as HANDLE,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut bytes_read,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = IoError::last_os_error();
            // ERROR_BROKEN_PIPE (109) means the write end closed — EOF.
            if err.raw_os_error() == Some(109) {
                return Ok(0);
            }
            return Err(err);
        }
        Ok(bytes_read as usize)
    }
}

/// Owned write end of a pipe; implements `Write` for sending input to the child.
pub struct PipeWriter(OwnedHandle);

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut bytes_written: u32 = 0;
        // SAFETY: handle is owned and live; buf is a readable slice.
        let ok = unsafe {
            WriteFile(
                self.0.as_raw_handle() as HANDLE,
                buf.as_ptr(),
                buf.len() as u32,
                &mut bytes_written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(IoError::last_os_error());
        }
        Ok(bytes_written as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Owned HPCON handle
// ---------------------------------------------------------------------------

/// RAII wrapper for an HPCON. Closes on drop.
struct Hpcon(HPCON);

// HPCON is just an opaque kernel-managed handle. Cross-thread sharing is safe
// as long as we serialize calls into ConPTY APIs, which we do via the master's
// mutex.
unsafe impl Send for Hpcon {}
unsafe impl Sync for Hpcon {}

impl Drop for Hpcon {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: handle was returned by CreatePseudoConsole and not closed yet.
            unsafe { ClosePseudoConsole(self.0) };
        }
    }
}

// ---------------------------------------------------------------------------
// Shared inner state
// ---------------------------------------------------------------------------

struct Inner {
    hpc: Arc<Hpcon>,
    read_handle: OwnedHandle,
    write_handle: Option<OwnedHandle>,
    rows: u16,
    cols: u16,
}

// ---------------------------------------------------------------------------
// Public API: PtyPair, PtyMaster, PtySlave, PtyChild
// ---------------------------------------------------------------------------

pub struct PtyPair {
    pub master: PtyMaster,
    pub slave: PtySlave,
}

pub struct PtyMaster {
    inner: Arc<Mutex<Inner>>,
}

pub struct PtySlave {
    inner: Arc<Mutex<Inner>>,
}

pub struct PtyChild {
    process: OwnedHandle,
    /// Hold an `Arc<Hpcon>` so ConPTY stays alive at least as long as the child.
    _hpc: Arc<Hpcon>,
}

#[derive(Debug, Clone, Copy)]
pub struct ExitStatus {
    pub code: u32,
}

impl PtyMaster {
    pub fn take_writer(&self) -> Result<Box<dyn Write + Send>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("ConPTY inner mutex poisoned"))?;
        let writer = inner
            .write_handle
            .take()
            .ok_or_else(|| anyhow!("writer already taken"))?;
        Ok(Box::new(PipeWriter(writer)))
    }

    pub fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("ConPTY inner mutex poisoned"))?;
        let cloned = try_clone_handle(&inner.read_handle)?;
        Ok(Box::new(PipeReader(cloned)))
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("ConPTY inner mutex poisoned"))?;
        let coord = coord(rows, cols)?;
        // SAFETY: hpc is owned and live for the duration of the Arc.
        let hr = unsafe { ResizePseudoConsole(inner.hpc.0, coord) };
        if hr != S_OK {
            // E_HANDLE / ERROR_BROKEN_PIPE after child exit are benign.
            bail!(
                "ResizePseudoConsole failed: HRESULT=0x{:x} ({})",
                hr,
                IoError::from_raw_os_error(hr)
            );
        }
        inner.rows = rows;
        inner.cols = cols;
        Ok(())
    }
}

impl PtySlave {
    pub fn spawn_command(&self, cmd: CommandBuilder) -> Result<PtyChild> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("ConPTY inner mutex poisoned"))?;
        spawn(&inner, cmd)
    }
}

impl PtyChild {
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        // SAFETY: handle is owned and live; `WaitForSingleObject` with 0 is non-blocking.
        let wait = unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, 0) };
        if wait == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait != WAIT_OBJECT_0 {
            return Err(IoError::last_os_error());
        }
        let mut code: u32 = 0;
        // SAFETY: handle is owned and live; `code` is a writable out-pointer.
        let ok =
            unsafe { GetExitCodeProcess(self.process.as_raw_handle() as HANDLE, &mut code) };
        if ok == 0 {
            return Err(IoError::last_os_error());
        }
        Ok(Some(ExitStatus { code }))
    }

    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        // SAFETY: see try_wait.
        let wait =
            unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, INFINITE) };
        if wait != WAIT_OBJECT_0 {
            return Err(IoError::last_os_error());
        }
        let mut code: u32 = 0;
        let ok =
            unsafe { GetExitCodeProcess(self.process.as_raw_handle() as HANDLE, &mut code) };
        if ok == 0 {
            return Err(IoError::last_os_error());
        }
        Ok(ExitStatus { code })
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        // SAFETY: handle is owned and live.
        let ok = unsafe { TerminateProcess(self.process.as_raw_handle() as HANDLE, 1) };
        if ok == 0 {
            return Err(IoError::last_os_error());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// openpty
// ---------------------------------------------------------------------------

pub fn openpty(rows: u16, cols: u16) -> Result<PtyPair> {
    let input = create_pipe()?;
    let output = create_pipe()?;

    let mut hpc: HPCON = 0;
    let flags = selected_flags();
    // SAFETY: pipe handles are live; `hpc` is a writable out-pointer.
    let hr = unsafe {
        CreatePseudoConsole(
            coord(rows, cols)?,
            input.read.as_raw_handle() as HANDLE,
            output.write.as_raw_handle() as HANDLE,
            flags,
            &mut hpc,
        )
    };
    if hr != S_OK {
        // If PASSTHROUGH was requested and rejected, retry without it.
        if flags & PSEUDOCONSOLE_PASSTHROUGH_MODE != 0 {
            warn!(
                "CreatePseudoConsole with PASSTHROUGH failed (HRESULT=0x{hr:x}); retrying without"
            );
            let fallback = flags & !PSEUDOCONSOLE_PASSTHROUGH_MODE;
            // SAFETY: same as above.
            let hr2 = unsafe {
                CreatePseudoConsole(
                    coord(rows, cols)?,
                    input.read.as_raw_handle() as HANDLE,
                    output.write.as_raw_handle() as HANDLE,
                    fallback,
                    &mut hpc,
                )
            };
            if hr2 != S_OK {
                bail!(
                    "CreatePseudoConsole failed: HRESULT=0x{hr2:x} ({})",
                    IoError::from_raw_os_error(hr2)
                );
            }
        } else {
            bail!(
                "CreatePseudoConsole failed: HRESULT=0x{hr:x} ({})",
                IoError::from_raw_os_error(hr)
            );
        }
    }

    // ConPTY duplicates these internally; we close our copies so the child sees
    // EOF when the master end goes away.
    drop(input.read);
    drop(output.write);

    let inner = Arc::new(Mutex::new(Inner {
        hpc: Arc::new(Hpcon(hpc)),
        read_handle: output.read,
        write_handle: Some(input.write),
        rows,
        cols,
    }));

    Ok(PtyPair {
        master: PtyMaster {
            inner: Arc::clone(&inner),
        },
        slave: PtySlave { inner },
    })
}

fn coord(rows: u16, cols: u16) -> Result<COORD> {
    let x = i16::try_from(cols).map_err(|_| anyhow!("cols {cols} exceeds Windows COORD range"))?;
    let y = i16::try_from(rows).map_err(|_| anyhow!("rows {rows} exceeds Windows COORD range"))?;
    Ok(COORD { X: x, Y: y })
}

// ---------------------------------------------------------------------------
// Spawning a child attached to the ConPTY
// ---------------------------------------------------------------------------

fn spawn(inner: &Inner, cmd: CommandBuilder) -> Result<PtyChild> {
    let mut cmdline_w = build_cmdline(&cmd)?;
    let mut env_w = build_env_block(&cmd);
    let cwd_w = cmd.get_cwd().map(|c| wide_with_nul(c.as_os_str()));

    // Initialize a proc thread attribute list with one slot, populate the
    // PSEUDOCONSOLE attribute.
    let mut size: usize = 0;
    // SAFETY: passing null + 0 to size queries the required buffer size.
    unsafe {
        InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
    }
    let mut attr_buf: Vec<u8> = vec![0; size];
    let attr_ptr = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    // SAFETY: buf has the size returned by the probe call.
    let ok = unsafe { InitializeProcThreadAttributeList(attr_ptr, 1, 0, &mut size) };
    if ok == 0 {
        return Err(anyhow!(
            "InitializeProcThreadAttributeList failed: {}",
            IoError::last_os_error()
        ));
    }
    // SAFETY: attr_ptr is initialized; hpc is live; size_of::<HPCON> matches.
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_ptr,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            inner.hpc.0 as *mut _,
            mem::size_of::<HPCON>(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: attr_ptr was initialized.
        unsafe { DeleteProcThreadAttributeList(attr_ptr) };
        return Err(anyhow!(
            "UpdateProcThreadAttribute failed: {}",
            IoError::last_os_error()
        ));
    }

    let mut si: STARTUPINFOEXW = unsafe { mem::zeroed() };
    si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    si.lpAttributeList = attr_ptr;

    let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

    // Pass null for lpApplicationName so CreateProcessW parses the first
    // token of lpCommandLine and resolves it via PATH + PATHEXT. Otherwise
    // we'd need to ship our own `which`-equivalent.
    // SAFETY: all pointers are valid for the duration of the call.
    let res = unsafe {
        CreateProcessW(
            ptr::null(),
            cmdline_w.as_mut_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            FALSE,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            env_w.as_mut_ptr() as *mut _,
            cwd_w
                .as_ref()
                .map(|v| v.as_ptr())
                .unwrap_or(ptr::null()),
            &si.StartupInfo,
            &mut pi,
        )
    };

    // Drop attribute list now that CreateProcessW has copied what it needs.
    unsafe { DeleteProcThreadAttributeList(attr_ptr) };

    if res == 0 {
        let err = IoError::last_os_error();
        return Err(anyhow!("CreateProcessW failed: {err}"));
    }

    // Close thread handle (we never wait on it). Wrap process handle for RAII.
    let _ = unsafe { OwnedHandle::from_raw_handle(pi.hThread as _) };
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as _) };

    Ok(PtyChild {
        process,
        _hpc: Arc::clone(&inner.hpc),
    })
}

// ---------------------------------------------------------------------------
// Command-line and environment-block construction
// ---------------------------------------------------------------------------

fn wide_with_nul(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Build the wide command-line for CreateProcessW.
///
/// `lpApplicationName` is null, so the first token here is what Windows
/// will resolve via PATH + PATHEXT.
fn build_cmdline(cmd: &CommandBuilder) -> Result<Vec<u16>> {
    let argv = cmd.get_argv();
    if argv.is_empty() {
        bail!("CommandBuilder has no program");
    }
    let mut cmdline: Vec<u16> = Vec::new();
    append_quoted(&argv[0], &mut cmdline);
    for arg in argv.iter().skip(1) {
        cmdline.push(' ' as u16);
        if arg.encode_wide().any(|c| c == 0) {
            bail!("argument contains a null byte: {arg:?}");
        }
        append_quoted(arg, &mut cmdline);
    }
    cmdline.push(0);
    Ok(cmdline)
}

fn append_quoted(arg: &OsStr, out: &mut Vec<u16>) {
    let needs_quotes = arg.is_empty()
        || arg.encode_wide().any(|c| {
            matches!(
                c,
                c if c == ' ' as u16
                    || c == '\t' as u16
                    || c == '\n' as u16
                    || c == 0x0b
                    || c == '"' as u16
            )
        });
    if !needs_quotes {
        out.extend(arg.encode_wide());
        return;
    }
    out.push('"' as u16);
    let mut backslashes: usize = 0;
    for c in arg.encode_wide() {
        if c == '\\' as u16 {
            backslashes += 1;
        } else if c == '"' as u16 {
            // Escape preceding backslashes (each becomes two) and the quote.
            for _ in 0..backslashes * 2 + 1 {
                out.push('\\' as u16);
            }
            backslashes = 0;
            out.push('"' as u16);
        } else {
            for _ in 0..backslashes {
                out.push('\\' as u16);
            }
            backslashes = 0;
            out.push(c);
        }
    }
    // Escape trailing backslashes before the closing quote.
    for _ in 0..backslashes * 2 {
        out.push('\\' as u16);
    }
    out.push('"' as u16);
}

/// Build the wide environment block for CreateProcessW.
///
/// Format: `KEY=VALUE\0KEY=VALUE\0\0` (double-nul terminated).
fn build_env_block(cmd: &CommandBuilder) -> Vec<u16> {
    let mut block = Vec::<u16>::new();
    for (k, v) in cmd.iter_full_env_as_str() {
        block.extend(OsStr::new(k).encode_wide());
        block.push('=' as u16);
        block.extend(OsStr::new(v).encode_wide());
        block.push(0);
    }
    // Final terminator. CreateProcessW requires a double-nul even if empty.
    block.push(0);
    block
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_number_nonzero() {
        // Smoke-test: on any supported Windows build this is in the thousands.
        assert!(windows_build() > 0);
    }

    #[test]
    fn flags_always_include_quirk_and_input() {
        let f = selected_flags();
        assert!(f & PSEUDOCONSOLE_RESIZE_QUIRK != 0);
        assert!(f & PSEUDOCONSOLE_WIN32_INPUT_MODE != 0);
    }

    #[test]
    fn append_quoted_simple() {
        let mut out = Vec::new();
        append_quoted(OsStr::new("hello"), &mut out);
        let s: String = String::from_utf16(&out).unwrap();
        assert_eq!(s, "hello");
    }

    #[test]
    fn append_quoted_with_space() {
        let mut out = Vec::new();
        append_quoted(OsStr::new("a b"), &mut out);
        let s: String = String::from_utf16(&out).unwrap();
        assert_eq!(s, "\"a b\"");
    }

    #[test]
    fn append_quoted_with_embedded_quote() {
        let mut out = Vec::new();
        append_quoted(OsStr::new(r#"a "b" c"#), &mut out);
        let s: String = String::from_utf16(&out).unwrap();
        assert_eq!(s, r#""a \"b\" c""#);
    }

    #[test]
    fn append_quoted_trailing_backslash() {
        let mut out = Vec::new();
        append_quoted(OsStr::new(r"a b\"), &mut out);
        let s: String = String::from_utf16(&out).unwrap();
        assert_eq!(s, r#""a b\\""#);
    }
}
