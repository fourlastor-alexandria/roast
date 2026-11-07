//! Keeps a per-user AOT cache (JEP 483) trained on real sessions of the application.
//!
//! A launch without a matching cache runs in AOT record mode. Once that JVM is destroyed, roast
//! starts a copy of itself to assemble the cache, as only a new JVM can do that, and later launches
//! start from the cache. When the application ends the process itself, with `System.exit` for
//! example, roast doesn't get control back, so the next launch assembles the recording first.
//!
//! The key of a cache covers the runtime, the class path files and the launch options, so a new
//! build of the application gets a new cache. That's needed for correctness, not only speed: Java
//! 25.0.0 to 25.0.3 and 26 to 26.0.1 don't reject a cache when a jar changes (JDK-8377932) and run
//! the classes stored in the cache instead.

use log::{debug, info, warn};
use serde::Deserialize;
use std::{
    env,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The first release that stores method profiles in the cache and logs with the `aot` tag
const MIN_JAVA_VERSION: u32 = 25;
/// Turns the cache off when set to `0`, `false`, `off` or `no`
const DISABLE_ENV: &str = "ROAST_AOT";
/// Replaces the per-user folder that holds the caches
const CACHE_DIR_ENV: &str = "ROAST_AOT_CACHE_DIR";
/// Set by roast on the copy of itself that assembles a cache, to the recording to assemble
const ASSEMBLE_ENV: &str = "ROAST_AOT_ASSEMBLE";
/// Where that copy writes the cache
const OUTPUT_ENV: &str = "ROAST_AOT_OUTPUT";
/// Environment variables the JVM reads options from, which change what a cache must match
const ENVIRONMENT_OPTIONS: [&str; 2] = ["JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS"];
/// Options that manage class data sharing themselves, turn it off, or stop the JVM from using it
const CONFLICTING_OPTIONS: [&str; 15] = [
    "-XX:AOT",
    "-XX:+AOT",
    "-XX:-AOT",
    "-Xshare",
    "-XX:SharedArchiveFile",
    "-XX:SharedClassListFile",
    "-XX:ArchiveClassesAtExit",
    "-XX:+AutoCreateSharedArchive",
    "-XX:DumpLoadedClassList",
    "--patch-module",
    "--upgrade-module-path",
    "--limit-modules",
    "-javaagent",
    "-agentlib",
    "-agentpath",
];
const ASSEMBLY_TIMEOUT: Duration = Duration::from_secs(300);
/// Recording sessions a key gets, retries after a failed assembly or a refused cache included
const MAX_RECORDINGS: u32 = 3;
/// Caches of other keys to keep, so that two installs of the same app don't evict each other
const OTHER_CACHES_KEPT: usize = 2;
/// Younger files of other keys may still be in use by a running launch
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

#[derive(Deserialize, Clone, Default)]
pub(crate) struct AotCacheConfig {
    /// Names the folder that holds the caches, defaults to the executable's name
    pub(crate) id: Option<String>,
}

/// What a cache has to match
pub(crate) struct Inputs<'a> {
    pub(crate) executable: &'a Path,
    pub(crate) runtime_home: &'a Path,
    pub(crate) libjvm: &'a Path,
    pub(crate) class_path: &'a [String],
    /// Every option except `-Duser.dir`, which changes with the caller and doesn't matter here
    pub(crate) options: &'a [String],
    pub(crate) main_class: &'a str,
}

pub(crate) enum Plan {
    Off,
    Use {
        files: CacheFiles,
        start: Option<StartMarker>,
    },
    Record {
        files: CacheFiles,
        recording: PathBuf,
        start: Option<StartMarker>,
    },
}

impl Plan {
    pub(crate) fn vm_options(&self) -> Vec<String> {
        match self {
            Plan::Off => Vec::new(),
            Plan::Use { files, .. } => {
                let mut options = vec![
                    format!("-XX:AOTCache={}", files.cache().display()),
                    // Without the checksums the JVM runs whatever a damaged cache holds, and fails
                    // in odd places. With them it refuses the cache, at the cost of a few ms.
                    "-XX:+VerifySharedSpaces".to_string(),
                ];
                options.extend(log_options(&files.log()));
                options
            }
            Plan::Record {
                files, recording, ..
            } => {
                let mut options = vec![
                    "-XX:AOTMode=record".to_string(),
                    format!("-XX:AOTConfiguration={}", recording.display()),
                    // The JVM prints where it saved the recording no matter the log settings
                    "-XX:+DisplayVMOutputToStderr".to_string(),
                ];
                options.extend(log_options(&files.log()));
                options
            }
        }
    }

    /// Called once the JVM is up, which shows that roast's options didn't stop it from starting
    pub(crate) fn started(&mut self) {
        if let Plan::Use { start, .. } | Plan::Record { start, .. } = self {
            if let Some(StartMarker { file, path }) = start.take() {
                drop(file);
                remove(&path);
            }
        }
    }

    /// Assembles what this session recorded, once the JVM is destroyed
    pub(crate) fn finish(self) {
        let Plan::Record {
            files, recording, ..
        } = self
        else {
            return;
        };
        // An empty or missing recording means the JVM couldn't write it, and the count of
        // recordings stops roast from trying forever
        if is_non_empty(&recording) && assemble(&files, &recording) {
            files.prune();
        }
        remove(&recording);
    }
}

/// Keeps the JVM's AOT messages off the application's output, in a log next to the cache
fn log_options(log: &Path) -> [String; 2] {
    [
        "-Xlog:aot*=off".to_string(),
        format!("-Xlog:aot*=warning:file=\"{}\"::filecount=0", log.display()),
    ]
}

/// The files of one key in the cache folder
pub(crate) struct CacheFiles {
    dir: PathBuf,
    key: String,
}

impl CacheFiles {
    fn path(&self, suffix: &str) -> PathBuf {
        self.dir.join(format!("{}{suffix}", self.key))
    }

    fn cache(&self) -> PathBuf {
        self.path(".aot")
    }

    fn log(&self) -> PathBuf {
        self.path(".log")
    }

    fn assembly_log(&self) -> PathBuf {
        self.path(".assembly.log")
    }

    fn failed(&self) -> PathBuf {
        self.path(".failed")
    }

    fn recordings(&self) -> PathBuf {
        self.path(".recordings")
    }

    fn new_recording(&self) -> PathBuf {
        self.path(&format!(".{}.aotconf", process::id()))
    }

    fn assembly_output(&self) -> PathBuf {
        self.path(&format!(".{}.aot.part", process::id()))
    }

    fn mark_start(&self) -> Option<StartMarker> {
        let path = self.path(&format!(".{}.starting", process::id()));
        let file = File::create(&path).ok()?;
        if file.lock().is_err() {
            drop(file);
            remove(&path);
            return None;
        }
        Some(StartMarker { file, path })
    }

    /// Whether a launch died while its JVM was starting with roast's options. Its marker is left
    /// behind, and the lock on it went away with the process.
    fn take_failed_start(&self) -> bool {
        let prefix = format!("{}.", self.key);
        let mut failed = false;
        for entry in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) || !name.ends_with(".starting") {
                continue;
            }
            let Ok(file) = File::options().write(true).open(entry.path()) else {
                continue;
            };
            // A launch that is still starting holds the lock
            if file.try_lock().is_ok() {
                failed = true;
                drop(file);
                remove(&entry.path());
            }
        }
        failed
    }

    /// Recordings left by sessions that ended the process themselves, newest first
    fn pending_recordings(&self) -> Vec<PathBuf> {
        let prefix = format!("{}.", self.key);
        let mut recordings: Vec<(SystemTime, PathBuf)> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with(&prefix) && name.ends_with(".aotconf")
            })
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
                (metadata.len() > 0).then(|| (modified, entry.path()))
            })
            .collect();
        recordings.sort_by(|a, b| b.0.cmp(&a.0));
        recordings.into_iter().map(|(_, path)| path).collect()
    }

    /// Counts a recording session, and returns how many there were, this one included
    fn count_recording(&self) -> io::Result<u32> {
        let previous = fs::read_to_string(self.recordings())
            .ok()
            .and_then(|count| count.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let count = previous.saturating_add(1);
        fs::write(self.recordings(), count.to_string())?;
        Ok(count)
    }

    /// Whether a launch that started from the cache had the JVM refuse it, which the JVM logs as
    /// an error. Logs older than the cache come from the session that recorded it.
    fn was_refused(&self) -> bool {
        let modified = |path: PathBuf| fs::metadata(path).and_then(|metadata| metadata.modified());
        let (Ok(logged), Ok(assembled)) = (modified(self.log()), modified(self.cache())) else {
            return false;
        };
        logged >= assembled
            && fs::read_to_string(self.log()).is_ok_and(|log| log.contains("[error"))
    }

    /// Removes what other keys left behind, keeping the newest of their caches
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let prefix = format!("{}.", self.key);
        let kept = [
            self.log(),
            self.assembly_log(),
            self.cache(),
            self.recordings(),
        ];
        let mut other_caches = Vec::new();
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() || kept.contains(&entry.path()) {
                continue;
            }
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".aot") && !name.starts_with(&prefix) {
                other_caches.push((modified, entry.path()));
            } else if SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age > STALE_AFTER)
            {
                remove(&entry.path());
            }
        }
        other_caches.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, cache) in other_caches.into_iter().skip(OTHER_CACHES_KEPT) {
            remove(&cache);
        }
    }
}

/// Decides how this launch uses the cache. Anything unexpected turns the cache off for the launch
/// instead of stopping the application.
pub(crate) fn plan(config: Option<&AotCacheConfig>, inputs: &Inputs) -> Plan {
    let Some(config) = config else {
        return Plan::Off;
    };
    if let Some(reason) = skip_reason(inputs) {
        info!("Not using an AOT cache: {reason}");
        return Plan::Off;
    }
    match prepare(config, inputs) {
        Ok(plan) => plan,
        Err(error) => {
            warn!("Not using an AOT cache: {error}");
            Plan::Off
        }
    }
}

fn prepare(config: &AotCacheConfig, inputs: &Inputs) -> io::Result<Plan> {
    let files = CacheFiles {
        dir: cache_dir(config, inputs.executable)?,
        key: fingerprint(inputs)?,
    };
    fs::create_dir_all(&files.dir)?;
    if files.failed().exists() {
        info!(
            "Not using an AOT cache: assembling one failed before, remove {} to try again",
            files.failed().display()
        );
        return Ok(Plan::Off);
    }
    if files.take_failed_start() {
        fs::write(
            files.failed(),
            "The JVM didn't start with the AOT cache options\n",
        )?;
        warn!("Not using an AOT cache: the JVM didn't start with its options last time");
        return Ok(Plan::Off);
    }
    if is_non_empty(&files.cache()) {
        if !files.was_refused() {
            debug!("Using the AOT cache {}", files.cache().display());
            return Ok(Plan::Use {
                start: files.mark_start(),
                files,
            });
        }
        warn!(
            "The JVM refused the AOT cache {}, see {}",
            files.cache().display(),
            files.log().display()
        );
        remove(&files.cache());
        remove(&files.log());
    }
    for recording in files.pending_recordings() {
        let assembled = assemble(&files, &recording);
        remove(&recording);
        if assembled {
            files.prune();
            return Ok(Plan::Use {
                start: files.mark_start(),
                files,
            });
        }
    }
    // The count covers every session for the key, so a cache the JVM keeps refusing stops too
    if files.count_recording()? > MAX_RECORDINGS {
        fs::write(
            files.failed(),
            format!("No AOT cache after {MAX_RECORDINGS} recording sessions\n"),
        )?;
        info!("Not using an AOT cache: {MAX_RECORDINGS} recording sessions gave none");
        return Ok(Plan::Off);
    }
    let recording = files.new_recording();
    debug!("Recording this session to {}", recording.display());
    Ok(Plan::Record {
        start: files.mark_start(),
        files,
        recording,
    })
}

/// Held and locked while the JVM starts with roast's options
pub(crate) struct StartMarker {
    file: File,
    path: PathBuf,
}

fn skip_reason(inputs: &Inputs) -> Option<String> {
    if is_disabled(env::var(DISABLE_ENV).ok().as_deref()) {
        return Some(format!("{DISABLE_ENV} turns it off"));
    }
    let environment_options: Vec<String> = ENVIRONMENT_OPTIONS
        .iter()
        .filter_map(|variable| env::var(variable).ok())
        .collect();
    let conflicting = inputs
        .options
        .iter()
        .map(String::as_str)
        .chain(
            environment_options
                .iter()
                .flat_map(|it| it.split_whitespace()),
        )
        .find(|option| is_conflicting(option));
    if let Some(option) = conflicting {
        return Some(format!("the launch already sets {option}"));
    }
    match fs::read_to_string(inputs.runtime_home.join("release"))
        .ok()
        .and_then(|release| java_version(&release))
    {
        Some(version) if version >= MIN_JAVA_VERSION => {}
        Some(version) => {
            return Some(format!(
                "the runtime is Java {version}, AOT caches need Java {MIN_JAVA_VERSION} or later"
            ))
        }
        None => {
            return Some(format!(
                "no Java version in {}",
                inputs.runtime_home.join("release").display()
            ))
        }
    }
    // The JVM fails a recording with a folder on the class path, and the process with it
    inputs
        .class_path
        .iter()
        .find(|entry| !Path::new(entry).is_file())
        .map(|entry| format!("{entry} is not a jar file"))
}

fn is_disabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        ["0", "false", "off", "no"]
            .iter()
            .any(|off| value.trim().eq_ignore_ascii_case(off))
    })
}

fn is_conflicting(option: &str) -> bool {
    CONFLICTING_OPTIONS
        .iter()
        .any(|prefix| option.starts_with(prefix))
}

/// The feature version in a runtime's `release` file
fn java_version(release: &str) -> Option<u32> {
    let value = release
        .lines()
        .find_map(|line| line.strip_prefix("JAVA_VERSION="))?;
    value
        .trim()
        .trim_matches('"')
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn cache_dir(config: &AotCacheConfig, executable: &Path) -> io::Result<PathBuf> {
    if let Some(dir) = env::var_os(CACHE_DIR_ENV).filter(|dir| !dir.is_empty()) {
        // The JVM starts in the launcher's folder, so a relative path must be resolved here
        return Ok(env::current_dir()?.join(dir));
    }
    let id = config
        .id
        .as_deref()
        .and_then(folder_name)
        .or_else(|| {
            executable
                .file_stem()
                .and_then(|name| name.to_str())
                .and_then(folder_name)
        })
        .unwrap_or_else(|| "roast".to_string());
    let base = user_cache_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no per-user cache folder"))?;
    Ok(base.join(id).join("aot"))
}

/// Keeps an id usable as one folder name on every OS
fn folder_name(id: &str) -> Option<String> {
    let name: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = name.trim_start_matches('.');
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(target_os = "windows")]
fn user_cache_dir() -> Option<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn user_cache_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join("Library").join("Caches"))
}

#[cfg(target_os = "linux")]
fn user_cache_dir() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".cache")))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn fingerprint(inputs: &Inputs) -> io::Result<String> {
    let mut hash = Fingerprint::default();
    hash.field(b"roast aot cache 1");
    hash.field(env!("CARGO_PKG_VERSION").as_bytes());
    hash.field(&fs::read(inputs.runtime_home.join("release"))?);
    hash.file(&inputs.runtime_home.join("lib").join("modules"))?;
    hash.file(inputs.libjvm)?;
    for entry in inputs.class_path {
        hash.file(Path::new(entry))?;
    }
    for option in inputs.options {
        hash.field(option.as_bytes());
    }
    hash.field(inputs.main_class.as_bytes());
    for variable in ENVIRONMENT_OPTIONS {
        hash.field(env::var_os(variable).unwrap_or_default().as_encoded_bytes());
    }
    Ok(format!("{:016x}", hash.0))
}

/// FNV-1a, which is enough to tell launches apart and stays the same across Rust releases
struct Fingerprint(u64);

impl Default for Fingerprint {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fingerprint {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn field(&mut self, bytes: &[u8]) {
        self.write(&(bytes.len() as u64).to_le_bytes());
        self.write(bytes);
    }

    /// The JVM tells files apart by size and modification time, so the key does too
    fn file(&mut self, path: &Path) -> io::Result<()> {
        let metadata = fs::metadata(path)?;
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        self.field(path.as_os_str().as_encoded_bytes());
        self.write(&metadata.len().to_le_bytes());
        self.write(&modified.as_nanos().to_le_bytes());
        Ok(())
    }
}

/// Runs a copy of roast that assembles a recording, then moves the cache into place
fn assemble(files: &CacheFiles, recording: &Path) -> bool {
    let output = files.assembly_output();
    let started = Instant::now();
    let assembled = match run_assembly(recording, &output, &files.assembly_log()) {
        Ok(succeeded) => succeeded && is_non_empty(&output),
        Err(error) => {
            warn!("Failed to start assembling the AOT cache: {error}");
            false
        }
    };
    if assembled && promote(&output, &files.cache()) {
        info!(
            "Assembled the AOT cache {} in {:?}",
            files.cache().display(),
            started.elapsed()
        );
        true
    } else {
        warn!(
            "Failed to assemble an AOT cache from {}, see {}",
            recording.display(),
            files.assembly_log().display()
        );
        remove(&output);
        false
    }
}

fn run_assembly(recording: &Path, output: &Path, log: &Path) -> io::Result<bool> {
    let log = File::create(log)?;
    let mut command = Command::new(env::current_exe()?);
    command
        .env(ASSEMBLE_ENV, recording)
        .env(OUTPUT_ENV, output)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    hide_console_window(&mut command);
    let mut child = command.spawn()?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if started.elapsed() > ASSEMBLY_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "windows")]
fn hide_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_console_window(_: &mut Command) {}

/// Moves an assembled cache into place, unless another launch got there first
fn promote(output: &Path, cache: &Path) -> bool {
    if is_non_empty(cache) {
        remove(output);
        return true;
    }
    fs::rename(output, cache).is_ok() || is_non_empty(cache)
}

fn is_non_empty(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

/// Best effort: on Windows a cache that a running launch maps can't be removed
fn remove(path: &Path) {
    if let Err(error) = fs::remove_file(path) {
        if error.kind() != io::ErrorKind::NotFound {
            debug!("Failed to remove {}: {error}", path.display());
        }
    }
}

/// What the copy of roast that assembles a cache was asked to do
pub(crate) struct Assembly {
    recording: PathBuf,
    output: PathBuf,
}

impl Assembly {
    pub(crate) fn vm_options(&self) -> Vec<String> {
        vec![
            "-XX:AOTMode=create".to_string(),
            format!("-XX:AOTConfiguration={}", self.recording.display()),
            format!("-XX:AOTCache={}", self.output.display()),
        ]
    }
}

/// Set when this process is the copy of roast that assembles a cache
pub(crate) fn requested_assembly() -> Option<Assembly> {
    let recording = env::var_os(ASSEMBLE_ENV)?;
    Some(Assembly {
        recording: PathBuf::from(recording),
        output: PathBuf::from(env::var_os(OUTPUT_ENV).unwrap_or_default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    struct Fixture {
        root: PathBuf,
        runtime_home: PathBuf,
        libjvm: PathBuf,
        jar: PathBuf,
        cache_dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!("roast aot {name} {unique}"));
            let runtime_home = root.join("runtime");
            let libjvm = runtime_home.join("lib").join("server").join("libjvm");
            fs::create_dir_all(libjvm.parent().unwrap()).unwrap();
            fs::write(runtime_home.join("release"), "JAVA_VERSION=\"25.0.2\"\n").unwrap();
            fs::write(runtime_home.join("lib").join("modules"), "modules").unwrap();
            fs::write(&libjvm, "libjvm").unwrap();
            let jar = root.join("app.jar");
            fs::write(&jar, "jar").unwrap();
            let cache_dir = root.join("cache");
            fs::create_dir_all(&cache_dir).unwrap();
            Self {
                root,
                runtime_home,
                libjvm,
                jar,
                cache_dir,
            }
        }

        fn key(&self, options: &[String]) -> String {
            let class_path = [self.jar.to_str().unwrap().to_string()];
            fingerprint(&Inputs {
                executable: Path::new("game"),
                runtime_home: &self.runtime_home,
                libjvm: &self.libjvm,
                class_path: &class_path,
                options,
                main_class: "Main",
            })
            .unwrap()
        }

        fn files(&self, key: &str) -> CacheFiles {
            CacheFiles {
                dir: self.cache_dir.clone(),
                key: key.to_string(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn set_modified(path: &Path, modified: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn reads_the_feature_version_from_the_release_file() {
        assert_eq!(Some(25), java_version("JAVA_VERSION=\"25.0.2\"\n"));
        assert_eq!(
            Some(26),
            java_version("IMPLEMENTOR=x\nJAVA_VERSION=\"26\"\nOS_NAME=y")
        );
        assert_eq!(Some(27), java_version("JAVA_VERSION=\"27-ea\""));
        assert_eq!(Some(1), java_version("JAVA_VERSION=\"1.8.0_392\""));
        assert_eq!(None, java_version("OS_NAME=\"Linux\""));
    }

    #[test]
    fn steps_aside_for_options_that_manage_class_data_sharing() {
        for option in [
            "-XX:AOTCache=app.aot",
            "-XX:AOTMode=off",
            "-XX:+AOTClassLinking",
            "-Xshare:off",
            "-XX:SharedArchiveFile=app.jsa",
            "-XX:+AutoCreateSharedArchive",
            "-javaagent:agent.jar",
            "--patch-module",
        ] {
            assert!(is_conflicting(option), "{option}");
        }
        for option in [
            "-Xmx1G",
            "-XX:+UseZGC",
            "--enable-native-access=ALL-UNNAMED",
        ] {
            assert!(!is_conflicting(option), "{option}");
        }
    }

    #[test]
    fn turns_off_with_the_usual_false_values() {
        assert!(is_disabled(Some("0")));
        assert!(is_disabled(Some("Off")));
        assert!(is_disabled(Some(" false ")));
        assert!(!is_disabled(Some("1")));
        assert!(!is_disabled(None));
    }

    #[test]
    fn makes_ids_usable_as_folder_names() {
        assert_eq!(
            Some("io.github.fourlastor.Game".to_string()),
            folder_name("io.github.fourlastor.Game")
        );
        assert_eq!(Some("My_Game_".to_string()), folder_name("My Game!"));
        assert_eq!(Some("_x".to_string()), folder_name("../x"));
        assert_eq!(None, folder_name(".."));
    }

    #[test]
    fn keys_change_with_the_jar_the_runtime_and_the_options() {
        let fixture = Fixture::new("keys");
        let options = vec!["-Xmx1G".to_string()];
        let key = fixture.key(&options);
        assert_eq!(key, fixture.key(&options));
        assert_eq!(16, key.len());

        assert_ne!(key, fixture.key(&["-Xmx2G".to_string()]));

        let modified = fs::metadata(&fixture.jar).unwrap().modified().unwrap();
        set_modified(&fixture.jar, modified + Duration::from_secs(1));
        let touched = fixture.key(&options);
        assert_ne!(key, touched);

        fs::write(&fixture.jar, "jar 2").unwrap();
        assert_ne!(touched, fixture.key(&options));

        let jar_key = fixture.key(&options);
        fs::write(&fixture.libjvm, "libjvm 2").unwrap();
        assert_ne!(jar_key, fixture.key(&options));
    }

    #[test]
    fn assembles_the_newest_pending_recording_first() {
        let fixture = Fixture::new("pending");
        let files = fixture.files("0123456789abcdef");
        let old = files.dir.join("0123456789abcdef.1.aotconf");
        let new = files.dir.join("0123456789abcdef.2.aotconf");
        let empty = files.dir.join("0123456789abcdef.3.aotconf");
        let other = files.dir.join("fedcba9876543210.4.aotconf");
        for path in [&old, &new, &other] {
            fs::write(path, "recording").unwrap();
        }
        fs::write(&empty, "").unwrap();
        let now = SystemTime::now();
        set_modified(&old, now - Duration::from_secs(60));
        set_modified(&new, now);

        assert_eq!(vec![new, old], files.pending_recordings());
    }

    #[test]
    fn counts_recording_sessions() {
        let fixture = Fixture::new("count");
        let files = fixture.files("0123456789abcdef");

        assert_eq!(1, files.count_recording().unwrap());
        assert_eq!(2, files.count_recording().unwrap());
        files.prune();
        assert_eq!(3, files.count_recording().unwrap());
    }

    #[test]
    fn notices_a_refused_cache_from_errors_logged_after_assembly() {
        let fixture = Fixture::new("refused");
        let files = fixture.files("0123456789abcdef");
        let now = SystemTime::now();
        fs::write(files.cache(), "cache").unwrap();
        set_modified(&files.cache(), now - Duration::from_secs(10));

        fs::write(
            files.log(),
            "[0.010s][error  ][aot] Unable to map shared spaces\n",
        )
        .unwrap();
        set_modified(&files.log(), now - Duration::from_secs(20));
        assert!(!files.was_refused(), "a log from the recording session");

        set_modified(&files.log(), now);
        assert!(files.was_refused());

        fs::write(files.log(), "[0.007s][warning][aot] Failed to link\n").unwrap();
        assert!(!files.was_refused(), "warnings only");

        remove(&files.log());
        assert!(!files.was_refused(), "no log");
    }

    #[test]
    fn prunes_old_files_and_keeps_the_newest_other_caches() {
        let fixture = Fixture::new("prune");
        let files = fixture.files("0000000000000000");
        let now = SystemTime::now();
        let old = now - STALE_AFTER - Duration::from_secs(60);
        let path = |name: &str| files.dir.join(name);
        let entries = [
            ("0000000000000000.aot", old),
            ("0000000000000000.log", old),
            ("1111111111111111.aot", now - Duration::from_secs(1)),
            ("2222222222222222.aot", now - Duration::from_secs(2)),
            ("3333333333333333.aot", now - Duration::from_secs(3)),
            ("3333333333333333.log", old),
            ("4444444444444444.5.aotconf", now),
            ("5555555555555555.6.aot.part", old),
        ];
        for (name, modified) in entries {
            fs::write(path(name), "x").unwrap();
            set_modified(&path(name), modified);
        }

        files.prune();

        let mut left: Vec<String> = fs::read_dir(&files.dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            vec![
                "0000000000000000.aot",
                "0000000000000000.log",
                "1111111111111111.aot",
                "2222222222222222.aot",
                "4444444444444444.5.aotconf",
            ],
            left
        );
    }

    #[test]
    fn records_and_uses_with_quiet_logging() {
        let files = CacheFiles {
            dir: PathBuf::from("cache"),
            key: "0123456789abcdef".to_string(),
        };
        let recording = files.dir.join("0123456789abcdef.7.aotconf");
        let record = Plan::Record {
            files,
            recording: recording.clone(),
            start: None,
        }
        .vm_options();
        assert_eq!("-XX:AOTMode=record", record[0]);
        assert_eq!(
            format!("-XX:AOTConfiguration={}", recording.display()),
            record[1]
        );
        assert!(record.contains(&"-Xlog:aot*=off".to_string()));

        let use_options = Plan::Use {
            files: CacheFiles {
                dir: PathBuf::from("cache"),
                key: "0123456789abcdef".to_string(),
            },
            start: None,
        }
        .vm_options();
        assert_eq!(
            format!(
                "-XX:AOTCache={}",
                Path::new("cache").join("0123456789abcdef.aot").display()
            ),
            use_options[0]
        );
        assert!(Plan::Off.vm_options().is_empty());
    }

    #[test]
    fn notices_a_launch_that_died_while_its_jvm_was_starting() {
        let fixture = Fixture::new("start");
        let files = fixture.files("0123456789abcdef");
        let mut plan = Plan::Use {
            start: files.mark_start(),
            files: fixture.files("0123456789abcdef"),
        };
        assert!(
            !files.take_failed_start(),
            "a launch that is still starting"
        );

        plan.started();
        assert!(!files.take_failed_start(), "a launch whose JVM is up");

        let abandoned = files.path(".42.starting");
        fs::write(&abandoned, "").unwrap();
        assert!(files.take_failed_start());
        assert!(!abandoned.exists());
        assert!(!files.take_failed_start());
    }
}
