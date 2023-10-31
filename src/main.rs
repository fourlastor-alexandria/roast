use jni::{
    errors::StartJvmResult,
    objects::{JObjectArray, JString},
    InitArgsBuilder, JNIVersion, JavaVM,
};
use std::{env, path::PathBuf};

// Picks discrete GPU on Windows, if possible
#[allow(non_upper_case_globals)]
#[cfg(target_env = "msvc")]
#[no_mangle]
pub static NvOptimusEnablement: u32 = 0x00000001;

#[allow(non_upper_case_globals)]
#[cfg(target_env = "msvc")]
#[no_mangle]
pub static AmdPowerXpressRequestHighPerformance: u32 = 0x00000001;

fn start_jvm(
    jvm_location: &str,
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
        Ok([jvm_location, java_locator::get_jvm_dyn_lib_file_name()]
            .iter()
            .collect::<PathBuf>())
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
    let mutable_args: &JObjectArray = method_args.as_ref();
    let mut i = 0;
    for argument in jstrings {
        // let value = env.new_string(argument);
        let _ = env.set_object_array_element(mutable_args, i, argument);
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

fn main() -> StartJvmResult<()> {
    let args: Vec<String> = env::args().collect();
    // jvm_location is different between platforms
    // app-image/lib/server on linux
    // app-image/bin/server on windows
    return start_jvm(
        "app-image/bin/server",
        "pokewilds.jar",
        "com/pkmngen/game/desktop/DesktopLauncher",
        args,
    );
}
