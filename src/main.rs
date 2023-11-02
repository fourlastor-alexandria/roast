use jni::{objects::JString, InitArgsBuilder, JNIVersion, JavaVM};
use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[allow(non_snake_case)]
#[derive(Deserialize)]
struct Config {
    classPath: Vec<String>,
    mainClass: String,
    vmArgs: Vec<String>,
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
const JVM_LOCATION: [&str; 3] = ["jdk", "bin", "server"];
#[cfg(target_os = "macos")]
const JVM_LOCATION: [&str; 5] = ["jdk", "Contents", "Home", "lib", "server"];
#[cfg(target_os = "linux")]
const JVM_LOCATION: [&str; 3] = ["jdk", "lib", "server"];

fn start_jvm(
    jvm_location: &PathBuf,
    class_path: Vec<String>,
    main_class: &str,
    vm_args: Vec<String>,
    args: Vec<String>,
) {
    let mut args_builder = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!("-Djava.class.path={}", class_path.join(CLASS_PATH_DELIMITER)));

    for arg in vm_args {
        args_builder = args_builder.option(arg);
    }

    // Build the VM properties
    let jvm_args = args_builder.build().expect("Failed to buid VM properties");

    // Create a new VM
    let jvm = JavaVM::with_libjvm(jvm_args, || {
        Ok(jvm_location
            .as_path()
            .join(java_locator::get_jvm_dyn_lib_file_name()))
    })
    .expect("Failed to create a new JavaVM");

    let mut env = jvm
        .attach_current_thread()
        .expect("Failed to attach the current thread");

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
        main_class,
        "main",
        "([Ljava/lang/String;)V",
        &[(&method_args).into()],
    ).expect("Failed to call main method");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let current_exe = env::current_exe()
        .expect("Failed to get current exe location");
    let current_location = current_exe.parent().expect("Exe must be in a directory");
    let jvm_location = current_location.join(JVM_LOCATION.iter().collect::<PathBuf>());
    let config_file_path = current_location.join("config.json");
    let data = fs::read_to_string(config_file_path).expect("Unable to read config file");
    let config: Config = serde_json::from_str(&data).expect("Invalid config json");
    start_jvm(
        &jvm_location,
        config.classPath,
        &config.mainClass.replace(".", "/"),
        config.vmArgs,
        args,
    );
}
