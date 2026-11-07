use std::{
    env, io,
    path::{Path, PathBuf},
};

#[cfg(target_os = "windows")]
const CLASS_PATH_DELIMITER: &str = ";";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const CLASS_PATH_DELIMITER: &str = ":";

pub(crate) struct WorkingDirectoryGuard {
    original: PathBuf,
    restored: bool,
}

impl WorkingDirectoryGuard {
    pub(crate) fn enter(path: &Path) -> io::Result<Self> {
        let original = env::current_dir()?;
        env::set_current_dir(path)?;
        Ok(Self {
            original,
            restored: false,
        })
    }

    pub(crate) fn restore(&mut self) -> io::Result<()> {
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

/// The options that decide how the JVM is set up, which are the same wherever it's launched from
pub(crate) fn launch_options(
    class_path: &[String],
    vm_args: &[String],
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
    options
}

/// The launch options, then the ones roast adds for this launch, and last the caller's directory
pub(crate) fn build_vm_options(
    launch_options: &[String],
    launcher_options: &[String],
    application_working_directory: &Path,
) -> Vec<String> {
    let user_dir = application_working_directory
        .to_str()
        .expect("Caller working directory must be valid Unicode");
    launch_options
        .iter()
        .chain(launcher_options)
        .cloned()
        .chain([format!("-Duser.dir={user_dir}")])
        .collect()
}

#[cfg(target_os = "windows")]
pub(crate) fn add_to_dll_search_path(directory: &Path) -> io::Result<()> {
    use std::{ffi::c_int, iter, os::windows::ffi::OsStrExt};

    #[link(name = "kernel32")]
    extern "system" {
        fn SetDllDirectoryW(path: *const u16) -> c_int;
    }

    let path: Vec<u16> = directory
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    if unsafe { SetDllDirectoryW(path.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn is_zgc_supported() -> bool {
    // Windows 10 1803 is required for ZGC, see https://wiki.openjdk.java.net/display/zgc/Main#Main-SupportedPlatforms
    // Windows 10 1803 is build 17134.
    use windows_version::OsVersion;
    OsVersion::current() >= OsVersion::new(10, 0, 0, 17134)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_zgc_supported() -> bool {
    true
}
