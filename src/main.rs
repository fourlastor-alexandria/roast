#![cfg_attr(not(feature = "win_console"), windows_subsystem = "windows")]
mod aot;
mod bootstrap;

use bootstrap::{build_vm_options, launch_options, WorkingDirectoryGuard};
use jni::{
    errors::Error,
    jni_sig, jni_str,
    objects::{JObject, JThrowable},
    refs::LoaderContext,
    strings::JNIString,
    Env, InitArgsBuilder, JNIVersion, JavaVM,
};
use serde::Deserialize;
use std::{env, fs, path::PathBuf, process};

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
    aotCache: Option<aot::AotCacheConfig>,
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

/// Where the runtime and the application are, and the options that decide how the JVM is set up
struct Launch {
    executable: PathBuf,
    install: PathBuf,
    runtime_home: PathBuf,
    libjvm: PathBuf,
    class_path: Vec<String>,
    options: Vec<String>,
}

impl Launch {
    fn new(config: &Config) -> Self {
        let executable = env::current_exe().expect("Failed to get current exe location");
        let install = executable
            .parent()
            .expect("Exe must be in a directory")
            .to_path_buf();
        let [home, lib, server] = runtime_location();
        let runtime_home = install.join(home);
        let libjvm = runtime_home
            .join(lib)
            .join(server)
            .join(java_locator::get_jvm_dyn_lib_file_name());
        let class_path: Vec<String> = config
            .classPath
            .iter()
            .map(|it| install.join(it).into_os_string().into_string().unwrap())
            .collect();
        let options = launch_options(
            &class_path,
            config.vmArgs.as_deref().unwrap_or_default(),
            config.useZgcIfSupportedOs.unwrap_or(false),
        );
        Self {
            executable,
            install,
            runtime_home,
            libjvm,
            class_path,
            options,
        }
    }

    fn aot_inputs<'a>(&'a self, main_class: &'a str) -> aot::Inputs<'a> {
        aot::Inputs {
            executable: &self.executable,
            runtime_home: &self.runtime_home,
            libjvm: &self.libjvm,
            class_path: &self.class_path,
            options: &self.options,
            main_class,
        }
    }
}

fn create_jvm(launch: &Launch, options: Vec<String>) -> JavaVM {
    let mut args_builder = InitArgsBuilder::new().version(JNIVersion::V1_8);
    for option in options {
        args_builder = args_builder.option(option);
    }

    // Build the VM properties
    let jvm_args = args_builder.build().expect("Failed to buid VM properties");

    // Make sure Roast can find dll files which are in the runtime/bin folder
    #[cfg(target_os = "windows")]
    bootstrap::add_to_dll_search_path(
        launch
            .libjvm
            .parent()
            .and_then(std::path::Path::parent)
            .expect("Runtime server folder must be in a directory"),
    )
    .expect("Failed to add the runtime to the DLL search path");

    // HotSpot validates relative AOT classpath entries against the native working directory used
    // during VM creation. Bootstrap from the launcher directory, then restore the caller before
    // any application class is loaded.
    let mut working_directory = WorkingDirectoryGuard::enter(&launch.install)
        .expect("Failed to switch to launcher working directory");
    let jvm = JavaVM::with_libjvm(jvm_args, || Ok(launch.libjvm.clone()))
        .expect("Failed to create a new JavaVM");
    working_directory
        .restore()
        .expect("Failed to restore caller working directory");
    jvm
}

fn start_jvm(
    launch: &Launch,
    options: Vec<String>,
    main_class_name: &str,
    args: Vec<String>,
    use_main_as_context_class_loader: bool,
    mut aot_plan: aot::Plan,
) {
    let jvm = create_jvm(launch, options);
    aot_plan.started();

    let failed = jvm
        .attach_current_thread(|env| {
            match call_main(
                env,
                main_class_name,
                &args,
                use_main_as_context_class_loader,
            ) {
                Ok(()) => Ok(false),
                Err(Error::JavaException) => report_pending_exception(env).map(|()| true),
                Err(Error::ExceptionInInitializer {
                    exception: Some(exception),
                }) => report_uncaught_exception(env, &exception).map(|()| true),
                Err(error) => {
                    eprintln!("Failed to call main method: {error}");
                    Ok(true)
                }
            }
        })
        .expect("Failed to call main method");

    // The same way the java launcher finishes: destroying the VM waits for non-daemon threads and
    // runs the shutdown hooks. The explicit exit is needed on macOS when the JVM isn't on the first
    // thread, as that stays in its run loop.
    jvm.detach_current_thread()
        .expect("Failed to detach the main thread");
    unsafe { jvm.destroy() }.expect("Failed to destroy the JavaVM");
    // The application has finished, so assembling what it recorded doesn't keep anyone waiting
    aot_plan.finish();
    process::exit(i32::from(failed));
}

/// Runs in the copy of roast that assembles an AOT cache. In create mode the JVM writes the cache
/// and ends the process itself, so creating it doesn't return.
fn assemble_aot_cache(config: &Config, assembly: &aot::Assembly) -> ! {
    let launch = Launch::new(config);
    let caller = env::current_dir().expect("Failed to get caller working directory");
    let options = build_vm_options(&launch.options, &assembly.vm_options(), &caller);
    create_jvm(&launch, options);
    process::exit(1);
}

fn call_main(
    env: &mut Env,
    main_class_name: &str,
    args: &[String],
    use_main_as_context_class_loader: bool,
) -> Result<(), Error> {
    // As the java launcher does, leave initializing the class to the call to main, so that a failure
    // in a static initializer is reported as an uncaught exception
    let main_class =
        LoaderContext::None.load_class(env, JNIString::from(main_class_name), false)?;
    if use_main_as_context_class_loader {
        // ClassLoader loader = mainClass.getClassLoader()
        let class_loader = env
            .call_method(
                &main_class,
                jni_str!("getClassLoader"),
                jni_sig!("()Ljava/lang/ClassLoader;"),
                &[],
            )?
            .l()?;
        // Thread.currentThread().setContextClassLoader(loader)
        let current_thread = current_thread(env)?;
        env.call_method(
            &current_thread,
            jni_str!("setContextClassLoader"),
            jni_sig!("(Ljava/lang/ClassLoader;)V"),
            &[(&class_loader).into()],
        )?;
    }

    let initial_value = env.new_string("")?;
    let method_args = env.new_object_array(
        args.len() as i32,
        jni_str!("java/lang/String"),
        &initial_value,
    )?;
    for (index, argument) in args.iter().enumerate() {
        let argument = env.new_string(argument)?;
        method_args.set_element(env, index, &argument)?;
    }

    env.call_static_method(
        &main_class,
        jni_str!("main"),
        jni_sig!("([Ljava/lang/String;)V"),
        &[(&method_args).into()],
    )?;
    Ok(())
}

fn current_thread<'local>(env: &mut Env<'local>) -> Result<JObject<'local>, Error> {
    env.call_static_method(
        jni_str!("java/lang/Thread"),
        jni_str!("currentThread"),
        jni_sig!("()Ljava/lang/Thread;"),
        &[],
    )?
    .l()
}

// Detaching clears a pending exception before the JVM can see it, so it has to be reported here
fn report_pending_exception(env: &mut Env) -> Result<(), Error> {
    let Some(exception) = env.exception_occurred() else {
        return Ok(());
    };
    env.exception_clear();
    report_uncaught_exception(env, &exception)
}

// As the JVM does, this ignores anything thrown by the handler itself
fn report_uncaught_exception(env: &mut Env, exception: &JThrowable) -> Result<(), Error> {
    let current_thread = current_thread(env)?;
    let _ = env.call_method(
        &current_thread,
        jni_str!("dispatchUncaughtException"),
        jni_sig!("(Ljava/lang/Throwable;)V"),
        &[exception.into()],
    );
    env.exception_clear();
    Ok(())
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
    let launch = Launch::new(config);
    let config_args = config.args.as_ref().unwrap_or(&Vec::new()).to_vec();
    let use_main_as_context_class_loader = config.useMainAsContextClassLoader.unwrap_or(false);
    let aot_plan = aot::plan(
        config.aotCache.as_ref(),
        &launch.aot_inputs(&config.mainClass),
    );
    let options = build_vm_options(
        &launch.options,
        &aot_plan.vm_options(),
        &application_working_directory,
    );

    start_jvm(
        &launch,
        options,
        &config.mainClass,
        [config_args, cli_args].concat(),
        use_main_as_context_class_loader,
        aot_plan,
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
    if let Some(assembly) = aot::requested_assembly() {
        assemble_aot_cache(&read_config_from_disk(), &assembly);
    }
    maybe_run_in_thread();
}
