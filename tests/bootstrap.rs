#[path = "../src/bootstrap.rs"]
mod bootstrap;

use bootstrap::{build_vm_options, WorkingDirectoryGuard};
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

static WORKING_DIRECTORY_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn switches_to_launcher_for_bootstrap_and_restores_caller() {
    let _lock = WORKING_DIRECTORY_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = WorkingDirectoryFixture::new();
    env::set_current_dir(&fixture.caller).unwrap();

    let mut guard = WorkingDirectoryGuard::enter(&fixture.launcher).unwrap();
    assert_eq!(fixture.launcher, env::current_dir().unwrap());

    guard.restore().unwrap();
    assert_eq!(fixture.caller, env::current_dir().unwrap());
}

#[test]
fn restores_caller_when_jvm_bootstrap_unwinds() {
    let _lock = WORKING_DIRECTORY_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = WorkingDirectoryFixture::new();
    env::set_current_dir(&fixture.caller).unwrap();

    {
        let _guard = WorkingDirectoryGuard::enter(&fixture.launcher).unwrap();
        assert_eq!(fixture.launcher, env::current_dir().unwrap());
    }

    assert_eq!(fixture.caller, env::current_dir().unwrap());
}

#[test]
fn caller_user_dir_is_the_final_vm_option() {
    let caller = Path::new("caller with spaces");
    let options = build_vm_options(
        &["/install/app/app.jar".to_string()],
        &["-Duser.dir=/wrong".to_string(), "-Xmx1G".to_string()],
        caller,
        false,
    );

    assert_eq!(
        Some("-Duser.dir=caller with spaces"),
        options.last().map(String::as_str)
    );
}

#[cfg(target_os = "windows")]
#[test]
fn finds_dlls_by_name_in_the_added_directory() {
    let _lock = WORKING_DIRECTORY_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = WorkingDirectoryFixture::new();
    let system_root = env::var("SystemRoot").unwrap();
    fs::copy(
        Path::new(&system_root).join("System32/version.dll"),
        fixture.launcher.join("roast-search-path-probe.dll"),
    )
    .unwrap();

    assert!(!can_load_dll("roast-search-path-probe.dll"));

    bootstrap::add_to_dll_search_path(&fixture.launcher).unwrap();

    assert!(can_load_dll("roast-search-path-probe.dll"));
}

#[cfg(target_os = "windows")]
fn can_load_dll(name: &str) -> bool {
    use std::{
        ffi::{c_int, c_void, OsStr},
        iter,
        os::windows::ffi::OsStrExt,
    };

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> c_int;
    }

    let name: Vec<u16> = OsStr::new(name)
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    unsafe {
        let module = LoadLibraryW(name.as_ptr());
        if module.is_null() {
            return false;
        }
        FreeLibrary(module);
    }
    true
}

struct WorkingDirectoryFixture {
    original: PathBuf,
    root: PathBuf,
    caller: PathBuf,
    launcher: PathBuf,
}

impl WorkingDirectoryFixture {
    fn new() -> Self {
        let original = env::current_dir().unwrap();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("roast cwd {unique} ø"));
        let caller = root.join("caller");
        let launcher = root.join("launcher");
        fs::create_dir_all(&caller).unwrap();
        fs::create_dir_all(&launcher).unwrap();
        let root = root.canonicalize().unwrap();
        let caller = root.join("caller");
        let launcher = root.join("launcher");
        Self {
            original,
            root,
            caller,
            launcher,
        }
    }
}

impl Drop for WorkingDirectoryFixture {
    fn drop(&mut self) {
        env::set_current_dir(&self.original).unwrap();
        fs::remove_dir_all(&self.root).unwrap();
    }
}
