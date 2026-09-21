//! The strict Windows spawn path uses JOB_LIST at CreateProcess, so no game
//! instruction (or descendant) can run before ownership is established.
//! std's spawn_with_attributes is unstable; keep this small adapter until it
//! stabilizes. Ordinary, unmounted engines keep std::process unchanged.
use std::{
    io,
    process::{Child, Command, ExitStatus},
};

pub(crate) struct Process {
    inner: Inner,
    pub stdin: Option<Box<dyn io::Write + Send>>,
    pub stdout: Option<Box<dyn io::Read + Send>>,
    pub stderr: Option<Box<dyn io::Read + Send>>,
}
enum Inner {
    Ordinary(Child),
    #[cfg(windows)]
    Owned(std::os::windows::io::OwnedHandle, u32),
}
impl From<Child> for Process {
    fn from(mut child: Child) -> Self {
        Self {
            stdin: child
                .stdin
                .take()
                .map(|p| Box::new(p) as Box<dyn io::Write + Send>),
            stdout: child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn io::Read + Send>),
            stderr: child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn io::Read + Send>),
            inner: Inner::Ordinary(child),
        }
    }
}
impl Process {
    pub fn id(&self) -> u32 {
        match &self.inner {
            Inner::Ordinary(c) => c.id(),
            #[cfg(windows)]
            Inner::Owned(_, id) => *id,
        }
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_for(false)
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_for(true).map(Option::unwrap)
    }
    fn wait_for(&mut self, block: bool) -> io::Result<Option<ExitStatus>> {
        match &mut self.inner {
            Inner::Ordinary(c) => {
                if block {
                    c.wait().map(Some)
                } else {
                    c.try_wait()
                }
            }
            #[cfg(windows)]
            Inner::Owned(h, _) => {
                use std::os::windows::{io::AsRawHandle, process::ExitStatusExt};
                use windows_sys::Win32::{Foundation::*, System::Threading::*};
                // SAFETY: owned process handle with synchronize/query rights.
                match unsafe {
                    WaitForSingleObject(h.as_raw_handle(), if block { INFINITE } else { 0 })
                } {
                    WAIT_TIMEOUT => Ok(None),
                    WAIT_OBJECT_0 => {
                        let mut code = 0;
                        if unsafe { GetExitCodeProcess(h.as_raw_handle(), &mut code) } == 0 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(Some(ExitStatus::from_raw(code)))
                    }
                    _ => Err(io::Error::last_os_error()),
                }
            }
        }
    }
}

impl super::Job {
    pub(crate) fn spawn(&self, command: &Command) -> io::Result<Process> {
        #[cfg(windows)]
        {
            windows::spawn(self.handle, command)
        }
        #[cfg(not(windows))]
        {
            let _ = command;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Mounted games require process-tree ownership.",
            ))
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::{OsStr, OsString},
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
    };
    use windows_sys::Win32::{
        Foundation::*,
        Security::SECURITY_ATTRIBUTES,
        System::{Pipes::CreatePipe, Threading::*},
    };

    fn wide(s: &OsStr) -> io::Result<Vec<u16>> {
        let mut w: Vec<u16> = s.encode_wide().collect();
        if w.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL in process configuration",
            ));
        }
        w.push(0);
        Ok(w)
    }
    // Microsoft CRT quoting: double backslashes before a quote and before the
    // closing quote. No shell participates. argv[0] is separately constrained.
    fn quote(arg: &OsStr, dst: &mut Vec<u16>) -> io::Result<()> {
        dst.push(b'"' as u16);
        let mut slashes = 0;
        for c in arg.encode_wide() {
            if c == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "NUL in argument",
                ));
            }
            if c == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            dst.extend(std::iter::repeat_n(
                b'\\' as u16,
                if c == b'"' as u16 {
                    slashes * 2 + 1
                } else {
                    slashes
                },
            ));
            slashes = 0;
            dst.push(c);
        }
        dst.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        dst.push(b'"' as u16);
        Ok(())
    }
    fn pipe(child_reads: bool) -> io::Result<(OwnedHandle, OwnedHandle)> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: valid output pointers and attributes, synchronous anonymous pipe.
        if unsafe { CreatePipe(&mut read, &mut write, &sa, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let (read, write) = unsafe {
            (
                OwnedHandle::from_raw_handle(read),
                OwnedHandle::from_raw_handle(write),
            )
        };
        let (child, parent) = if child_reads {
            (read, write)
        } else {
            (write, read)
        };
        // Only the child's end is inheritable. HANDLE_LIST further limits spawn.
        if unsafe { SetHandleInformation(parent.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((child, parent))
    }
    struct Attributes {
        _storage: Vec<usize>,
        ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
    }
    impl Attributes {
        fn new() -> io::Result<Self> {
            let mut bytes = 0;
            unsafe {
                InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut bytes);
            }
            let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
            let ptr = storage.as_mut_ptr().cast();
            if unsafe { InitializeProcThreadAttributeList(ptr, 2, 0, &mut bytes) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                _storage: storage,
                ptr,
            })
        }
        fn handles(&mut self, key: u32, handles: &[HANDLE]) -> io::Result<()> {
            // SAFETY: caller keeps both the handle array and handles alive through
            // CreateProcess. Attribute storage is initialized and sufficiently sized.
            if unsafe {
                UpdateProcThreadAttribute(
                    self.ptr,
                    0,
                    key as usize,
                    handles.as_ptr().cast(),
                    std::mem::size_of_val(handles),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }
    impl Drop for Attributes {
        fn drop(&mut self) {
            unsafe {
                DeleteProcThreadAttributeList(self.ptr);
            }
        }
    }

    // A headless runner must return loader failures, not display a modal
    // Windows dialog. Thread-local mode avoids changing the embedding host.
    struct ErrorMode(u32);
    impl ErrorMode {
        fn quiet() -> io::Result<Self> {
            use windows_sys::Win32::System::Diagnostics::Debug::*;
            let old = unsafe { GetThreadErrorMode() };
            if unsafe {
                SetThreadErrorMode(
                    old | SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(old))
        }
    }
    impl Drop for ErrorMode {
        fn drop(&mut self) {
            unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::SetThreadErrorMode(
                    self.0,
                    std::ptr::null_mut(),
                );
            }
        }
    }

    pub(super) fn spawn(job: HANDLE, command: &Command) -> io::Result<Process> {
        let program = command.get_program();
        // Strict spawn accepts an explicit exe path, never a command interpreter.
        let path = std::path::Path::new(program);
        if !path.is_absolute()
            || !path
                .extension()
                .is_some_and(|s| s.eq_ignore_ascii_case("exe"))
            || program.encode_wide().any(|c| c == b'"' as u16)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mounted games require an absolute .exe path.",
            ));
        }
        let application = wide(program)?;
        let _mode = ErrorMode::quiet()?;
        let mut binary_type = 0;
        // Refuse DOS/16-bit images before Windows can try a compatibility host.
        use windows_sys::Win32::{
            Storage::FileSystem::GetBinaryTypeW,
            System::WindowsProgramming::{SCS_32BIT_BINARY, SCS_64BIT_BINARY},
        };
        if unsafe { GetBinaryTypeW(application.as_ptr(), &mut binary_type) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if !matches!(binary_type, SCS_32BIT_BINARY | SCS_64BIT_BINARY) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mounted games require a native 32-bit or 64-bit executable.",
            ));
        }
        let mut argv = vec![b'"' as u16];
        argv.extend(program.encode_wide());
        argv.push(b'"' as u16);
        for arg in command.get_args() {
            argv.push(b' ' as u16);
            quote(arg, &mut argv)?;
        }
        argv.push(0);
        let cwd = command
            .get_current_dir()
            .map(|p| wide(p.as_os_str()))
            .transpose()?;
        // Windows environment names are case-insensitive; preserve the actual
        // spelling/value while replacing inherited keys with explicit settings.
        let mut env = std::collections::BTreeMap::<OsString, (OsString, OsString)>::new();
        for (k, v) in std::env::vars_os() {
            env.insert(OsString::from(k.to_string_lossy().to_uppercase()), (k, v));
        }
        for (k, v) in command.get_envs() {
            let key = OsString::from(k.to_string_lossy().to_uppercase());
            if let Some(v) = v {
                env.insert(key, (k.into(), v.into()));
            } else {
                env.remove(&key);
            }
        }
        let mut environment = Vec::new();
        for (_, (k, v)) in env {
            let mut entry = k;
            entry.push("=");
            entry.push(v);
            environment.extend(wide(&entry)?);
        }
        environment.push(0);
        if environment.len() == 1 {
            environment.push(0);
        }
        let (stdin_child, stdin) = pipe(true)?;
        let (stdout_child, stdout) = pipe(false)?;
        let (stderr_child, stderr) = pipe(false)?;
        let handles = [
            stdin_child.as_raw_handle(),
            stdout_child.as_raw_handle(),
            stderr_child.as_raw_handle(),
        ];
        let jobs = [job];
        let mut attributes = Attributes::new()?;
        attributes.handles(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
        attributes.handles(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of_val(&startup) as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = handles[0];
        startup.StartupInfo.hStdOutput = handles[1];
        startup.StartupInfo.hStdError = handles[2];
        startup.lpAttributeList = attributes.ptr;
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: all strings, environment, attributes and handle arrays remain
        // live for the call. JOB_LIST assigns before the initial thread executes;
        // assignment failure fails CreateProcess, never launches an unowned game.
        if unsafe {
            CreateProcessW(
                application.as_ptr(),
                argv.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                environment.as_ptr().cast(),
                cwd.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                &startup.StartupInfo,
                &mut info,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
        let _thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
        Ok(Process {
            inner: Inner::Owned(process, info.dwProcessId),
            stdin: Some(Box::new(std::fs::File::from(stdin))),
            stdout: Some(Box::new(std::fs::File::from(stdout))),
            stderr: Some(Box::new(std::fs::File::from(stderr))),
        })
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::{
        engine::{RunOutcome, RunRequest, StopSignal},
        process_engine::{Invocation, ProcessEngine},
    };
    use std::{collections::BTreeMap, io::Read, sync::Arc};

    #[test]
    fn fixture() {
        let Ok(path) = std::env::var("HOMERUN_OWNED_TEST_OUTPUT") else {
            return;
        };
        std::fs::write(
            path,
            serde_json::to_vec(&std::env::args().collect::<Vec<_>>()).unwrap(),
        )
        .unwrap();
        println!("OWNED STDOUT");
        eprintln!("OWNED STDERR");
    }
    fn invocation(output: &std::path::Path) -> Invocation {
        Invocation {
            program: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "--exact".into(),
                "job::owned::tests::fixture".into(),
                "--nocapture".into(),
            ],
            env: BTreeMap::from([(
                "HOMERUN_OWNED_TEST_OUTPUT".into(),
                output.to_string_lossy().into_owned(),
            )]),
        }
    }
    #[test]
    fn failed_job_assignment_never_executes_game_code() {
        use windows_sys::Win32::System::Threading::CreateEventW;
        // A real event is a valid kernel handle but cannot own a process.
        // Inject it at the native JOB_LIST assignment boundary, not a mock
        // early return: CreateProcess must fail before the fixture can write.
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!handle.is_null());
        let job = Arc::new(crate::job::Job { handle });
        let root =
            std::env::temp_dir().join(format!("job-assignment-failure-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let output = root.join("game-ran");
        let mut engine = ProcessEngine::new(invocation(&output));
        engine.require_job(job);
        let result = engine.run_streamed(
            &RunRequest {
                server_id: "fixture".into(),
                data_dir: root.to_string_lossy().into_owned(),
                java_port: 0,
                settings: None,
            },
            StopSignal::default(),
            &|_, _| {},
            &|| panic!("unowned game announced readiness"),
        );
        assert!(
            matches!(result, RunOutcome::Crashed(_)),
            "ownership failure was ignored: {result:?}"
        );
        assert!(
            !output.exists(),
            "the game executed before job assignment was accepted"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn required_spawn_preserves_arguments_environment_and_both_pipes() {
        let root = std::env::temp_dir().join(format!("job-argv-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let output = root.join("game-arguments");
        let inv = invocation(&output);
        let mut command = Command::new(&inv.program);
        command.args(&inv.args).current_dir(&root).envs(&inv.env);
        let arguments = [
            "space in value",
            "ends-in-slash\\",
            "a\\\"quote",
            "",
            "nonascii-\u{00e9}",
        ];
        // libtest accepts extra test filters, exposing their exact parsed argv.
        command.args(arguments);
        let job =
            crate::job::Job::required(&format!("Global\\HomerunSave-argv-{}", std::process::id()))
                .unwrap();
        let mut child = job.spawn(&command).unwrap();
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(child.wait().unwrap().success());
        let argv: Vec<String> = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(&argv[argv.len() - arguments.len()..], &arguments);
        assert!(stdout.contains("OWNED STDOUT"), "stdout was lost: {stdout}");
        assert!(stderr.contains("OWNED STDERR"), "stderr was lost: {stderr}");
        job.terminate_and_wait().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
