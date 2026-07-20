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

pub(crate) fn build_vm_options(
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
