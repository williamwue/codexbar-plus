//! Bounded Windows subprocess execution.
//!
//! The runner uses `CreateProcessW` directly: arguments are never interpreted by a shell,
//! the child receives an explicit environment, stdout/stderr are drained concurrently,
//! and a kill-on-close Job Object owns the complete process tree. The job and the
//! inheritable-handle allowlist are applied atomically at process creation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default capture bound for each output stream.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SubprocessOptions {
    /// Complete child environment. The ambient process environment is not inherited.
    pub environment: HashMap<String, String>,
    pub current_dir: Option<PathBuf>,
    pub timeout: Duration,
    /// Independent limit for stdout and stderr.
    pub max_output_bytes: usize,
    pub accepts_non_zero_exit: bool,
}

impl SubprocessOptions {
    pub fn new(environment: HashMap<String, String>, timeout: Duration) -> Self {
        Self {
            environment,
            current_dir: None,
            timeout,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            accepts_non_zero_exit: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubprocessResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum SubprocessError {
    #[error("subprocess binary was not found: {0}")]
    BinaryNotFound(String),
    #[error("subprocess input is invalid: {0}")]
    InvalidInput(String),
    #[error("could not launch subprocess: {0}")]
    LaunchFailed(String),
    #[error("could not contain subprocess in a job object: {0}")]
    JobFailed(String),
    #[error("subprocess timed out after {0:?}")]
    TimedOut(Duration),
    #[error("subprocess output exceeded {0} bytes")]
    OutputTooLarge(usize),
    #[error("subprocess exited with code {code}: {stderr}")]
    NonZeroExit { code: u32, stderr: String },
    #[error("subprocess runner is only available on Windows")]
    UnsupportedPlatform,
}

/// Runs one executable without a shell.
///
/// Blocking. Call from `spawn_blocking` when used by an async provider.
pub fn run_blocking(
    binary: impl AsRef<Path>,
    arguments: &[String],
    options: SubprocessOptions,
) -> Result<SubprocessResult, SubprocessError> {
    platform::run(binary.as_ref(), arguments, options)
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub(super) fn run(
        _binary: &Path,
        _arguments: &[String],
        _options: SubprocessOptions,
    ) -> Result<SubprocessResult, SubprocessError> {
        Err(SubprocessError::UnsupportedPlatform)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::{c_void, OsStr};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_BROKEN_PIPE, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
        PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
        STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    const HANDLE_FLAG_INHERIT: u32 = 1;
    const INFINITE: u32 = 0xffff_ffff;

    struct OwnedHandle(HANDLE);

    unsafe impl Send for OwnedHandle {}

    impl OwnedHandle {
        fn new(handle: HANDLE) -> io::Result<Self> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    struct Child {
        process: OwnedHandle,
        _thread: OwnedHandle,
        job: OwnedHandle,
    }

    struct AttributeList {
        storage: Vec<usize>,
        job: Box<HANDLE>,
    }

    impl AttributeList {
        fn with_handles_and_job(handles: &[HANDLE], job: HANDLE) -> Result<Self, SubprocessError> {
            let mut bytes = 0;
            unsafe {
                InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut bytes);
            }
            if bytes == 0 {
                return Err(SubprocessError::LaunchFailed(last_error()));
            }
            let words = bytes.div_ceil(std::mem::size_of::<usize>());
            let mut list = Self {
                storage: vec![0; words],
                job: Box::new(job),
            };
            if unsafe { InitializeProcThreadAttributeList(list.raw(), 2, 0, &mut bytes) } == 0 {
                return Err(SubprocessError::LaunchFailed(last_error()));
            }
            if unsafe {
                UpdateProcThreadAttribute(
                    list.raw(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    std::mem::size_of_val(handles),
                    null_mut(),
                    null(),
                )
            } == 0
            {
                return Err(SubprocessError::LaunchFailed(last_error()));
            }
            if unsafe {
                UpdateProcThreadAttribute(
                    list.raw(),
                    0,
                    PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                    list.job.as_ref() as *const HANDLE as *const c_void,
                    std::mem::size_of::<HANDLE>(),
                    null_mut(),
                    null(),
                )
            } == 0
            {
                return Err(SubprocessError::JobFailed(last_error()));
            }
            Ok(list)
        }

        fn raw(&mut self) -> *mut c_void {
            self.storage.as_mut_ptr().cast()
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            if !self.storage.is_empty() {
                unsafe { DeleteProcThreadAttributeList(self.raw()) };
            }
        }
    }

    pub(super) fn run(
        binary: &Path,
        arguments: &[String],
        options: SubprocessOptions,
    ) -> Result<SubprocessResult, SubprocessError> {
        validate(&options, arguments)?;
        let executable = resolve_binary(binary, &options.environment)?;
        let command_line = command_line(&executable, arguments)?;
        let environment = environment_block(&options.environment)?;
        let current_dir = options.current_dir.as_deref().map(wide_nul).transpose()?;

        let (stdout_read, stdout_write) = pipe()?;
        let (stderr_read, stderr_write) = pipe()?;
        let stdin = open_null_input()?;
        let child = create_child(
            &executable,
            command_line,
            environment,
            current_dir.as_deref(),
            &stdin,
            &stdout_write,
            &stderr_write,
        )?;

        // The parent must close its writers before waiting for pipe EOF.
        drop(stdout_write);
        drop(stderr_write);
        drop(stdin);

        let stdout_overflow = Arc::new(AtomicBool::new(false));
        let stderr_overflow = Arc::new(AtomicBool::new(false));
        let stdout_thread = capture(
            stdout_read,
            options.max_output_bytes,
            Arc::clone(&stdout_overflow),
        );
        let stderr_thread = capture(
            stderr_read,
            options.max_output_bytes,
            Arc::clone(&stderr_overflow),
        );

        let wait_ms = timeout_millis(options.timeout);
        let wait = unsafe { WaitForSingleObject(child.process.raw(), wait_ms) };
        let timed_out = wait == WAIT_TIMEOUT;
        if timed_out {
            unsafe { TerminateJobObject(child.job.raw(), 1) };
            unsafe { WaitForSingleObject(child.process.raw(), INFINITE) };
        } else if wait == WAIT_FAILED {
            unsafe { TerminateJobObject(child.job.raw(), 1) };
            return Err(SubprocessError::LaunchFailed(last_error()));
        } else if wait != WAIT_OBJECT_0 {
            unsafe { TerminateJobObject(child.job.raw(), 1) };
            return Err(SubprocessError::LaunchFailed(format!(
                "unexpected process wait result {wait}"
            )));
        }
        // Closing a kill-on-close job also ends descendants after the root exits. This is
        // required before joining readers: a descendant may have inherited the pipe writers.
        drop(child.job);

        let stdout = join_capture(stdout_thread)?;
        let stderr = join_capture(stderr_thread)?;
        if timed_out {
            return Err(SubprocessError::TimedOut(options.timeout));
        }
        if stdout_overflow.load(Ordering::Relaxed) || stderr_overflow.load(Ordering::Relaxed) {
            return Err(SubprocessError::OutputTooLarge(options.max_output_bytes));
        }

        let mut exit_code = 0;
        if unsafe { GetExitCodeProcess(child.process.raw(), &mut exit_code) } == 0 {
            return Err(SubprocessError::LaunchFailed(last_error()));
        }
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        if exit_code != 0 && !options.accepts_non_zero_exit {
            return Err(SubprocessError::NonZeroExit {
                code: exit_code,
                stderr,
            });
        }
        Ok(SubprocessResult {
            stdout,
            stderr,
            exit_code,
        })
    }

    fn validate(options: &SubprocessOptions, arguments: &[String]) -> Result<(), SubprocessError> {
        if options.timeout.is_zero() {
            return Err(SubprocessError::InvalidInput(
                "timeout must be greater than zero".into(),
            ));
        }
        if options.max_output_bytes == usize::MAX {
            return Err(SubprocessError::InvalidInput(
                "max_output_bytes must leave room for the overflow sentinel".into(),
            ));
        }
        if arguments.iter().any(|argument| argument.contains('\0')) {
            return Err(SubprocessError::InvalidInput(
                "arguments cannot contain NUL".into(),
            ));
        }
        Ok(())
    }

    fn resolve_binary(
        binary: &Path,
        environment: &HashMap<String, String>,
    ) -> Result<PathBuf, SubprocessError> {
        if binary.is_absolute() || binary.components().count() > 1 {
            return executable_file(binary);
        }

        let path = environment
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
            .map(|(_, value)| value.as_str())
            .unwrap_or_default();
        let has_extension = binary.extension().is_some();
        for directory in std::env::split_paths(path) {
            let direct = directory.join(binary);
            if direct.is_file() {
                return executable_file(&direct);
            }
            if !has_extension {
                let exe = direct.with_extension("exe");
                if exe.is_file() {
                    return executable_file(&exe);
                }
            }
        }
        Err(SubprocessError::BinaryNotFound(
            binary.to_string_lossy().into_owned(),
        ))
    }

    fn executable_file(path: &Path) -> Result<PathBuf, SubprocessError> {
        if !path.is_file() {
            return Err(SubprocessError::BinaryNotFound(
                path.to_string_lossy().into_owned(),
            ));
        }
        std::fs::canonicalize(path)
            .map_err(|error| SubprocessError::LaunchFailed(error.to_string()))
    }

    fn create_child(
        executable: &Path,
        mut command_line: Vec<u16>,
        mut environment: Vec<u16>,
        current_dir: Option<&[u16]>,
        stdin: &OwnedHandle,
        stdout: &OwnedHandle,
        stderr: &OwnedHandle,
    ) -> Result<Child, SubprocessError> {
        let job = OwnedHandle::new(unsafe { CreateJobObjectW(null(), null()) })
            .map_err(|error| SubprocessError::JobFailed(error.to_string()))?;
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(SubprocessError::JobFailed(last_error()));
        }

        let inherited = [stdin.raw(), stdout.raw(), stderr.raw()];
        let mut attributes = AttributeList::with_handles_and_job(&inherited, job.raw())?;
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = stdin.raw();
        startup.StartupInfo.hStdOutput = stdout.raw();
        startup.StartupInfo.hStdError = stderr.raw();
        startup.lpAttributeList = attributes.raw();
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let executable = wide_nul(executable)?;
        let directory = current_dir.map_or(null(), |value| value.as_ptr());
        let created = unsafe {
            CreateProcessW(
                executable.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                environment.as_mut_ptr().cast(),
                directory,
                &startup.StartupInfo,
                &mut info,
            )
        };
        if created == 0 {
            return Err(SubprocessError::LaunchFailed(last_error()));
        }
        let process = OwnedHandle::new(info.hProcess)
            .map_err(|error| SubprocessError::LaunchFailed(error.to_string()))?;
        let thread = OwnedHandle::new(info.hThread)
            .map_err(|error| SubprocessError::LaunchFailed(error.to_string()))?;

        Ok(Child {
            process,
            _thread: thread,
            job,
        })
    }

    fn pipe() -> Result<(OwnedHandle, OwnedHandle), SubprocessError> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let mut read = null_mut();
        let mut write = null_mut();
        if unsafe { CreatePipe(&mut read, &mut write, &mut attributes, 0) } == 0 {
            return Err(SubprocessError::LaunchFailed(last_error()));
        }
        let read = OwnedHandle::new(read)
            .map_err(|error| SubprocessError::LaunchFailed(error.to_string()))?;
        let write = OwnedHandle::new(write)
            .map_err(|error| SubprocessError::LaunchFailed(error.to_string()))?;
        if unsafe {
            windows_sys::Win32::Foundation::SetHandleInformation(read.raw(), HANDLE_FLAG_INHERIT, 0)
        } == 0
        {
            return Err(SubprocessError::LaunchFailed(last_error()));
        }
        Ok((read, write))
    }

    fn open_null_input() -> Result<OwnedHandle, SubprocessError> {
        let nul: Vec<u16> = OsStr::new("NUL").encode_wide().chain(Some(0)).collect();
        let handle = OwnedHandle::new(unsafe {
            CreateFileW(
                nul.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        })
        .map_err(|error| SubprocessError::LaunchFailed(error.to_string()))?;
        if unsafe {
            windows_sys::Win32::Foundation::SetHandleInformation(
                handle.raw(),
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        } == 0
        {
            return Err(SubprocessError::LaunchFailed(last_error()));
        }
        Ok(handle)
    }

    fn capture(
        handle: OwnedHandle,
        limit: usize,
        overflow: Arc<AtomicBool>,
    ) -> thread::JoinHandle<Result<Vec<u8>, io::Error>> {
        thread::spawn(move || {
            let mut output = Vec::with_capacity(limit.min(64 * 1024));
            let mut buffer = [0u8; 8192];
            loop {
                let mut read = 0;
                let ok = unsafe {
                    ReadFile(
                        handle.raw(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        &mut read,
                        null_mut(),
                    )
                };
                if ok == 0 {
                    let code = unsafe { GetLastError() };
                    if code == ERROR_BROKEN_PIPE {
                        break;
                    }
                    return Err(io::Error::from_raw_os_error(code as i32));
                }
                if read == 0 {
                    break;
                }
                let available = limit.saturating_add(1).saturating_sub(output.len());
                let keep = (read as usize).min(available);
                output.extend_from_slice(&buffer[..keep]);
                if output.len() > limit {
                    overflow.store(true, Ordering::Relaxed);
                }
            }
            Ok(output)
        })
    }

    fn join_capture(
        thread: thread::JoinHandle<Result<Vec<u8>, io::Error>>,
    ) -> Result<Vec<u8>, SubprocessError> {
        thread
            .join()
            .map_err(|_| SubprocessError::LaunchFailed("output reader panicked".into()))?
            .map_err(|error| SubprocessError::LaunchFailed(error.to_string()))
    }

    fn timeout_millis(timeout: Duration) -> u32 {
        timeout.as_millis().clamp(1, (u32::MAX - 1) as u128) as u32
    }

    fn environment_block(
        environment: &HashMap<String, String>,
    ) -> Result<Vec<u16>, SubprocessError> {
        let mut entries: Vec<_> = environment.iter().collect();
        entries.sort_unstable_by(|(left, _), (right, _)| {
            left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
        });
        let mut block = Vec::new();
        for (key, value) in entries {
            if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
                return Err(SubprocessError::InvalidInput(format!(
                    "invalid environment variable name {key:?}"
                )));
            }
            block.extend(OsStr::new(key).encode_wide());
            block.push('=' as u16);
            block.extend(OsStr::new(value).encode_wide());
            block.push(0);
        }
        block.push(0);
        if block.len() == 1 {
            block.push(0);
        }
        Ok(block)
    }

    fn command_line(executable: &Path, arguments: &[String]) -> Result<Vec<u16>, SubprocessError> {
        let mut command = quote_argument(&executable.to_string_lossy());
        for argument in arguments {
            command.push(' ');
            command.push_str(&quote_argument(argument));
        }
        if command.encode_utf16().count() >= 32_767 {
            return Err(SubprocessError::InvalidInput(
                "command line exceeds the Windows 32,767 UTF-16 unit limit".into(),
            ));
        }
        Ok(OsStr::new(&command).encode_wide().chain(Some(0)).collect())
    }

    // CommandLineToArgvW-compatible quoting, including backslashes immediately before quotes.
    fn quote_argument(argument: &str) -> String {
        if !argument.is_empty()
            && !argument
                .chars()
                .any(|character| character.is_whitespace() || character == '"')
        {
            return argument.to_owned();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for character in argument.chars() {
            if character == '\\' {
                backslashes += 1;
            } else if character == '"' {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            } else {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                quoted.push(character);
            }
        }
        quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
        quoted.push('"');
        quoted
    }

    fn wide_nul(value: impl AsRef<OsStr>) -> Result<Vec<u16>, SubprocessError> {
        let value = value.as_ref();
        if value.encode_wide().any(|unit| unit == 0) {
            return Err(SubprocessError::InvalidInput("path contains NUL".into()));
        }
        Ok(value.encode_wide().chain(Some(0)).collect())
    }

    fn last_error() -> String {
        io::Error::last_os_error().to_string()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn quotes_windows_arguments() {
            assert_eq!(quote_argument("plain"), "plain");
            assert_eq!(quote_argument(""), "\"\"");
            assert_eq!(quote_argument("two words"), "\"two words\"");
            assert_eq!(quote_argument(r#"a\"b"#), r#""a\\\"b""#);
            assert_eq!(
                quote_argument(r#"C:\path with space\"#),
                r#""C:\path with space\\""#
            );
        }

        #[test]
        fn environment_is_sorted_and_double_terminated() {
            let block = environment_block(&HashMap::from([
                ("z".into(), "last".into()),
                ("A".into(), "first".into()),
            ]))
            .unwrap();
            let text = String::from_utf16_lossy(&block);
            assert_eq!(text, "A=first\0z=last\0\0");
        }

        fn helper_options(mode: &str, timeout: Duration) -> SubprocessOptions {
            SubprocessOptions::new(
                HashMap::from([
                    ("CB_SUBPROCESS_HELPER".into(), mode.into()),
                    ("CB_SUBPROCESS_VALUE".into(), "explicit child value".into()),
                ]),
                timeout,
            )
        }

        fn helper_arguments() -> Vec<String> {
            vec![
                "--ignored".into(),
                "--exact".into(),
                "subprocess::platform::tests::process_helper".into(),
                "--nocapture".into(),
            ]
        }

        #[test]
        #[ignore = "invoked by subprocess runner tests"]
        fn process_helper() {
            use std::io::Write;

            match std::env::var("CB_SUBPROCESS_HELPER").as_deref() {
                Ok("capture") => {
                    print!("{}", std::env::var("CB_SUBPROCESS_VALUE").unwrap());
                    eprint!("explicit stderr");
                }
                Ok("fail") => {
                    eprint!("failure detail");
                    std::process::exit(7);
                }
                Ok("overflow") => {
                    std::io::stdout().write_all(&vec![b'x'; 4096]).unwrap();
                }
                Ok("sleep") => thread::sleep(Duration::from_secs(10)),
                Ok("tree-parent") => {
                    let marker = std::env::var("CB_SUBPROCESS_MARKER").unwrap();
                    std::process::Command::new(std::env::current_exe().unwrap())
                        .args(helper_arguments())
                        .env_clear()
                        .env("CB_SUBPROCESS_HELPER", "tree-leaf")
                        .env("CB_SUBPROCESS_MARKER", marker)
                        .spawn()
                        .unwrap();
                    thread::sleep(Duration::from_secs(10));
                }
                Ok("tree-root-exit") => {
                    let marker = std::env::var("CB_SUBPROCESS_MARKER").unwrap();
                    std::process::Command::new(std::env::current_exe().unwrap())
                        .args(helper_arguments())
                        .env_clear()
                        .env("CB_SUBPROCESS_HELPER", "tree-leaf")
                        .env("CB_SUBPROCESS_MARKER", marker)
                        .spawn()
                        .unwrap();
                }
                Ok("tree-leaf") => {
                    thread::sleep(Duration::from_millis(750));
                    std::fs::write(std::env::var("CB_SUBPROCESS_MARKER").unwrap(), b"escaped")
                        .unwrap();
                }
                value => panic!("unexpected helper mode: {value:?}"),
            }
        }

        #[test]
        fn captures_both_streams_with_explicit_environment() {
            let result = run(
                &std::env::current_exe().unwrap(),
                &helper_arguments(),
                helper_options("capture", Duration::from_secs(5)),
            )
            .unwrap();
            assert!(result.stdout.contains("explicit child value"));
            assert!(result.stderr.contains("explicit stderr"));
            assert_eq!(result.exit_code, 0);
        }

        #[test]
        fn rejects_non_zero_exit_with_stderr() {
            let error = run(
                &std::env::current_exe().unwrap(),
                &helper_arguments(),
                helper_options("fail", Duration::from_secs(5)),
            )
            .unwrap_err();
            match error {
                SubprocessError::NonZeroExit { code, stderr } => {
                    assert_eq!(code, 7);
                    assert!(stderr.contains("failure detail"));
                }
                other => panic!("unexpected error: {other}"),
            }
        }

        #[test]
        fn rejects_output_over_bound() {
            let mut options = helper_options("overflow", Duration::from_secs(5));
            options.max_output_bytes = 128;
            let error = run(
                &std::env::current_exe().unwrap(),
                &helper_arguments(),
                options,
            )
            .unwrap_err();
            assert!(matches!(error, SubprocessError::OutputTooLarge(128)));
        }

        #[test]
        fn timeout_terminates_process_tree() {
            let marker = std::env::temp_dir().join(format!(
                "codexbar-subprocess-tree-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let mut options = helper_options("tree-parent", Duration::from_millis(300));
            options.environment.insert(
                "CB_SUBPROCESS_MARKER".into(),
                marker.to_string_lossy().into_owned(),
            );
            let error = run(
                &std::env::current_exe().unwrap(),
                &helper_arguments(),
                options,
            )
            .unwrap_err();
            assert!(matches!(error, SubprocessError::TimedOut(_)));
            thread::sleep(Duration::from_secs(1));
            assert!(
                !marker.exists(),
                "a descendant escaped the subprocess Job Object"
            );
        }

        #[test]
        fn natural_root_exit_still_terminates_descendants() {
            let marker = std::env::temp_dir().join(format!(
                "codexbar-subprocess-root-exit-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let mut options = helper_options("tree-root-exit", Duration::from_secs(5));
            options.environment.insert(
                "CB_SUBPROCESS_MARKER".into(),
                marker.to_string_lossy().into_owned(),
            );
            run(
                &std::env::current_exe().unwrap(),
                &helper_arguments(),
                options,
            )
            .unwrap();
            thread::sleep(Duration::from_secs(1));
            assert!(
                !marker.exists(),
                "a descendant survived the root process and escaped the Job Object"
            );
        }
    }
}
