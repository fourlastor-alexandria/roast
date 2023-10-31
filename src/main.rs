use jni::{
    errors::StartJvmResult,
    objects::JString,
    InitArgsBuilder, JNIVersion, JavaVM,
};
use std::{env, path::PathBuf};

// Picks discrete GPU on Windows, if possible
#[allow(non_upper_case_globals)]
#[cfg(target_os = "windows")]
#[no_mangle]
pub static NvOptimusEnablement: std::os::raw::c_ulong = 0x00000001;

#[allow(non_upper_case_globals)]
#[cfg(target_os = "windows")]
#[no_mangle]
pub static AmdPowerXpressRequestHighPerformance: std::os::raw::c_int = 1;

fn start_jvm(
    jvm_location: &PathBuf,
    jar_file: &str,
    main_class: &str,
    args: Vec<String>,
) -> StartJvmResult<()> {
    // Build the VM properties
    let jvm_args = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option("-Xcheck:jni")
        .option(format!("-Djava.class.path={}", jar_file))
        .build()
        .unwrap();

    // Create a new VM
    let jvm = JavaVM::with_libjvm(jvm_args, || {
        Ok(jvm_location
            .as_path()
            .join(java_locator::get_jvm_dyn_lib_file_name()))
    })?;

    let mut env = jvm.attach_current_thread()?;

    let jstrings: Vec<JString> = args
        .iter()
        .map(|s| env.new_string(s)) // Convert to JString (maybe)
        .filter_map(Result::ok)
        .collect();

    let initial_value = env.new_string("").unwrap();
    let method_args = env
        .new_object_array(args.len() as i32, "java/lang/String", initial_value)
        .unwrap();
    let mut i = 0;
    for argument in jstrings {
        // let value = env.new_string(argument);
        let _ = env.set_object_array_element(&method_args, i, argument);
        i = i + 1;
    }
    env.call_static_method(
        main_class,
        "main",
        "([Ljava/lang/String;)V",
        &[(&method_args).into()],
    )?
    .v()?;
    Ok(())
}

#[cfg(target_os = "windows")]
const JVM_LOCATION: [&str; 3] = ["jdk", "bin", "server"];
#[cfg(target_os = "macos")]
const JVM_LOCATION: [&str; 5] = ["jdk", "Contents", "Home", "lib", "server"];
#[cfg(target_os = "linux")]
const JVM_LOCATION: [&str; 3] = ["jdk", "lib", "server"];

fn main() -> StartJvmResult<()> {
    let args: Vec<String> = env::args().collect();
    let jvm_location = JVM_LOCATION.iter().collect::<PathBuf>();
    return start_jvm(
        &jvm_location,
        "game.jar",
        "io/github/fourlastor/gdx/Main",
        args,
    );
}
