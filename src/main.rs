#![cfg_attr(not(feature = "win_console"), windows_subsystem = "windows")]
use jni::{objects::JString, InitArgsBuilder, JNIVersion, JavaVM};
use serde::Deserialize;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

#[allow(non_snake_case)]
#[derive(Deserialize, Clone)]
struct Config {
    classPath: Vec<String>,
    mainClass: String,
    vmArgs: Option<Vec<String>>,
    args: Option<Vec<String>>,
    useZgcIfSupportedOs: Option<bool>,
    useMainAsContextClassLoader: Option<bool>,
    #[cfg(target_os = "macos")]
    runOnFirstThread: Option<bool>,
}

// Picks discrete GPU on Windows, if possible
#[allow(non_upper_case_globals)]
#[cfg(target_os = "windows")]
#[no_mangle]
pub static NvOptimusEnablement: std::os::raw::c_ulong = 0x00000001;

#[allow(non_upper_case_globals)]
#[cfg(target_os = "windows")]
#[no_mangle]
pub static AmdPowerXpressRequestHighPerformance: std::os::raw::c_int = 1;

#[cfg(target_os = "windows")]
static CLASS_PATH_DELIMITER: &str = ";";
#[cfg(any(target_os = "linux", target_os = "macos"))]
static CLASS_PATH_DELIMITER: &str = ":";

#[cfg(target_os = "windows")]
const RUNTIME_LOCATION: [&str; 3] = ["runtime", "bin", "server"];
#[cfg(all(target_os = "macos", not(feature = "macos_universal")))]
const RUNTIME_LOCATION: [&str; 3] = ["runtime", "lib", "server"];
#[cfg(all(target_os = "macos", feature = "macos_universal"))]
const RUNTIME_LOCATION_X64: [&str; 3] = ["runtime-x64", "lib", "server"];
#[cfg(all(target_os = "macos", feature = "macos_universal"))]
const RUNTIME_LOCATION_AARCH64: [&str; 3] = ["runtime-aarch64", "lib", "server"];
#[cfg(target_os = "linux")]
const RUNTIME_LOCATION: [&str; 3] = ["runtime", "lib", "server"];

const APP_FOLDER: &str = "app";

struct WorkingDirectoryGuard {
    original: PathBuf,
    restored: bool,
}

struct JvmDirectories<'a> {
    runtime: &'a Path,
    bootstrap: &'a Path,
    application: &'a Path,
}

impl WorkingDirectoryGuard {
    fn enter(path: &Path) -> io::Result<Self> {
        let original = env::current_dir()?;
        env::set_current_dir(path)?;
        Ok(Self {
            original,
            restored: false,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.restored {
            env::set_current_dir(&self.original)?;
            self.restored = true;
        }
        Ok(())
    }
}

impl Drop for WorkingDirectoryGuard {
    fn drop(&mut self) {
        if !self.restored {
            let _ = env::set_current_dir(&self.original);
        }
    }
}

fn build_vm_options(
    class_path: &[String],
    vm_args: &[String],
    application_working_directory: &Path,
    use_zgc_if_supported: bool,
) -> Vec<String> {
    let mut options = vec![format!(
        "-Djava.class.path={}",
        class_path.join(CLASS_PATH_DELIMITER)
    )];
    options.extend(vm_args.iter().cloned());

    if use_zgc_if_supported && is_zgc_supported() {
        options.push("-XX:+UnlockExperimentalVMOptions".to_string());
        options.push("-XX:+UseZGC".to_string());
    }

    let user_dir = application_working_directory
        .to_str()
        .expect("Caller working directory must be valid Unicode");
    options.push(format!("-Duser.dir={user_dir}"));
    options
}

fn start_jvm(
    directories: JvmDirectories<'_>,
    class_path: Vec<String>,
    main_class_name: &str,
    vm_args: Vec<String>,
    args: Vec<String>,
    use_zgc_if_supported: bool,
    use_main_as_context_class_loader: bool,
) {
    let mut args_builder = InitArgsBuilder::new().version(JNIVersion::V8);
    for option in build_vm_options(
        &class_path,
        &vm_args,
        directories.application,
        use_zgc_if_supported,
    ) {
        args_builder = args_builder.option(option);
    }

    // Build the VM properties
    let jvm_args = args_builder.build().expect("Failed to buid VM properties");

    // HotSpot validates relative AOT classpath entries against the native working directory used
    // during VM creation. Bootstrap from the launcher directory, then restore the caller before
    // any application class is loaded.
    let mut working_directory = WorkingDirectoryGuard::enter(directories.bootstrap)
        .expect("Failed to switch to launcher working directory");
    let jvm = JavaVM::with_libjvm(jvm_args, || {
        Ok(directories
            .runtime
            .join(java_locator::get_jvm_dyn_lib_file_name()))
    })
    .expect("Failed to create a new JavaVM");
    working_directory
        .restore()
        .expect("Failed to restore caller working directory");

    let mut env = jvm
        .attach_current_thread()
        .expect("Failed to attach the current thread");

    if use_main_as_context_class_loader {
        // Class mainClass = MainClass.class;
        let main_class = env
            .find_class(main_class_name)
            .expect("Failed to get main class");

        // ClassLoader loader = mainClass.getClassLoader()
        let class_loader = env
            .call_method(
                main_class,
                "getClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[],
            )
            .and_then(|it| it.l())
            .expect("Failed to get class loader from main class");

        // Thread thread = Thread.currentThread()
        let current_thread = env
            .call_static_method(
                "java/lang/Thread",
                "currentThread",
                "()Ljava/lang/Thread;",
                &[],
            )
            .and_then(|it| it.l())
            .expect("Failed to get current thread");

        // thread.setContextClassLoader(loader)
        env.call_method(
            current_thread,
            "setContextClassLoader",
            "(Ljava/lang/ClassLoader;)V",
            &[(&class_loader).into()],
        )
        .expect("Failed to set class loader");
    }

    let jstrings: Vec<JString> = args
        .iter()
        .map(|s| env.new_string(s)) // Convert to JString (maybe)
        .filter_map(Result::ok)
        .collect();

    let initial_value = env.new_string("").unwrap();
    let method_args = env
        .new_object_array(args.len() as i32, "java/lang/String", initial_value)
        .expect("Failed to create method arguments");

    let mut i = 0;
    for argument in jstrings {
        let _ = env.set_object_array_element(&method_args, i, argument);
        i = i + 1;
    }
    env.call_static_method(
        main_class_name,
        "main",
        "([Ljava/lang/String;)V",
        &[(&method_args).into()],
    )
    .expect("Failed to call main method");

    let exception_occurred = env
        .exception_check()
        .expect("Failed to check for exception");
    if exception_occurred {
        let exception = env
            .exception_occurred()
            .expect("Failed to retrieve occurred exception");
        // Thread thread = Thread.currentThread();
        let thread_class = env
            .find_class("java/lang/Thread")
            .expect("Failed to retrieve thread class");
        let current_thread = env
            .call_static_method(thread_class, "currentThread", "()Ljava/lang/Thread;", &[])
            .expect("Failed to get current thread");
        // call java.lang.Thread#dispatchUncaughtException(Throwable)
        env.call_method(
            current_thread.l().unwrap(),
            "dispatchUncaughtException",
            "(Ljava/lang/Throwable;)V",
            &[(&exception).into()],
        )
        .expect("Failed to dispatch uncaught exception");
        env.exception_clear()
            .expect("Failed to clear the exception")
    }
}

#[cfg(target_os = "windows")]
fn is_zgc_supported() -> bool {
    // Windows 10 1803 is required for ZGC, see https://wiki.openjdk.java.net/display/zgc/Main#Main-SupportedPlatforms
    // Windows 10 1803 is build 17134.
    use windows_version::OsVersion;
    return OsVersion::current() >= OsVersion::new(10, 0, 0, 17134);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_zgc_supported() -> bool {
    return true;
}

fn read_config(path: PathBuf) -> Result<Config, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let config = serde_json::from_str(&content)?;
    Ok(config)
}

fn read_config_from_disk() -> Config {
    let current_exe = env::current_exe().expect("Failed to get current exe location");
    let current_location = current_exe.parent().expect("Exe must be in a directory");
    let config_file_path = current_location
        .join(APP_FOLDER)
        .join(current_exe.with_extension("json").file_name().unwrap());

    read_config(config_file_path).unwrap_or_else(|err| {
        panic!(
            "Failed to load config file {}: {}",
            current_exe.with_extension("json").to_string_lossy(),
            err
        );
    })
}

fn start_jvm_with_config(config: &Config) {
    let application_working_directory =
        env::current_dir().expect("Failed to get caller working directory");
    let cli_args: Vec<String> = env::args().skip(1).collect();
    let current_exe = env::current_exe().expect("Failed to get current exe location");
    let current_location = current_exe.parent().expect("Exe must be in a directory");
    let runtime_location = current_location.join(runtime_location().iter().collect::<PathBuf>());

    let class_path: Vec<String> = config
        .classPath
        .iter()
        .map(|it| {
            current_location
                .join(it)
                .into_os_string()
                .into_string()
                .unwrap()
        })
        .collect();
    let main_class = &config.mainClass.replace(".", "/");
    let vm_args = config.vmArgs.as_ref().unwrap_or(&Vec::new()).to_vec();
    let config_args = config.args.as_ref().unwrap_or(&Vec::new()).to_vec();
    let use_zgc_if_supported = config.useZgcIfSupportedOs.unwrap_or(false);
    let use_main_as_context_class_loader = config.useMainAsContextClassLoader.unwrap_or(false);

    start_jvm(
        JvmDirectories {
            runtime: &runtime_location,
            bootstrap: current_location,
            application: &application_working_directory,
        },
        class_path,
        main_class,
        vm_args,
        [config_args, cli_args].concat(),
        use_zgc_if_supported,
        use_main_as_context_class_loader,
    );
}

#[cfg(all(target_os = "macos", feature = "macos_universal"))]
fn runtime_location() -> [&'static str; 3] {
    if cfg!(target_arch = "aarch64") {
        RUNTIME_LOCATION_AARCH64
    } else {
        RUNTIME_LOCATION_X64
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "windows",
    all(target_os = "macos", not(feature = "macos_universal"))
))]
fn runtime_location() -> [&'static str; 3] {
    RUNTIME_LOCATION
}

#[cfg(target_os = "macos")]
fn park_event_loop() {
    use core_foundation::date::CFAbsoluteTime;
    use core_foundation::runloop::{
        kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRunResult, CFRunLoopTimer, CFRunLoopTimerRef,
    };
    use std::{ffi::c_void, ptr, time::Duration};

    extern "C" fn dummy_timer(_: CFRunLoopTimerRef, _: *mut c_void) {}

    // Create a dummy timer with a far future fire time
    let timer = CFRunLoopTimer::new(
        CFAbsoluteTime::from(1.0e5), // Fire time
        0.0,                         // Interval
        0,                           // Flags
        0,                           // Order
        dummy_timer,                 // Dummy callback
        ptr::null_mut(),
    );

    unsafe {
        // Add the timer to the current run loop in default mode
        let current_run_loop = CFRunLoop::get_current();
        current_run_loop.add_timer(&timer, kCFRunLoopDefaultMode);

        // Park the thread in the run loop
        loop {
            let result = CFRunLoop::run_in_mode(
                kCFRunLoopDefaultMode,
                Duration::from_secs_f64(1.0e5),
                false,
            );
            if result == CFRunLoopRunResult::Finished {
                break;
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn maybe_run_in_thread() {
    use std::thread;

    let config = read_config_from_disk();
    let run_on_first_thread = config.runOnFirstThread.unwrap_or(false);

    if run_on_first_thread {
        start_jvm_with_config(&config);
    } else {
        let config_clone = config.clone();
        let _ = thread::spawn(move || {
            start_jvm_with_config(&config_clone);
        });
        park_event_loop();
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn maybe_run_in_thread() {
    let config = read_config_from_disk();
    start_jvm_with_config(&config);
}

fn main() {
    env_logger::init();
    maybe_run_in_thread();
}

#[cfg(test)]
mod tests {
    use super::{build_vm_options, WorkingDirectoryGuard};
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
}
