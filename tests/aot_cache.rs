//! Runs roast with `aotCache` against a real runtime. The AOT cache needs Java 25 or later, taken
//! from `ROAST_TEST_AOT_JDK` or `JAVA_HOME`; without one the tests say so and pass.

use std::{
    env,
    ffi::OsString,
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

// A launcher fails to start with "Text file busy" if another test forks while it's being copied
static LAUNCHER_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn records_the_first_launch_and_starts_the_next_ones_from_the_cache() {
    let Some(fixture) = AotFixture::new("cached") else {
        return;
    };

    assert_eq!("sharing=false version=1", fixture.launch(&[]));
    assert_eq!(1, fixture.files_ending(".aot").len());
    assert!(fixture.files_ending(".aotconf").is_empty());

    assert_eq!("sharing=true version=1", fixture.launch(&[]));
    assert_eq!("sharing=true version=1", fixture.launch(&[]));
}

#[test]
fn assembles_on_the_next_launch_when_the_application_ends_the_process() {
    let Some(fixture) = AotFixture::new("exit") else {
        return;
    };

    assert_eq!("sharing=false version=1", fixture.launch(&["exit"]));
    assert!(fixture.files_ending(".aot").is_empty());
    assert_eq!(1, fixture.files_ending(".aotconf").len());

    assert_eq!("sharing=true version=1", fixture.launch(&["exit"]));
    assert!(fixture.files_ending(".aotconf").is_empty());
}

#[test]
fn records_again_when_the_application_changes() {
    let Some(fixture) = AotFixture::new("update") else {
        return;
    };
    fixture.launch(&[]);
    assert_eq!("sharing=true version=1", fixture.launch(&[]));

    fixture.build_app("22");

    assert_eq!("sharing=false version=22", fixture.launch(&[]));
    assert_eq!("sharing=true version=22", fixture.launch(&[]));
}

#[test]
fn falls_back_to_no_cache_and_records_again_when_the_cache_is_damaged() {
    let Some(fixture) = AotFixture::new("damaged") else {
        return;
    };
    fixture.launch(&[]);
    let cache = fixture.files_ending(".aot").remove(0);
    let mut file = fs::OpenOptions::new().write(true).open(&cache).unwrap();
    file.seek(SeekFrom::Start(fs::metadata(&cache).unwrap().len() / 2))
        .unwrap();
    file.write_all(&[0; 64 * 1024]).unwrap();
    drop(file);

    assert_eq!("sharing=false version=1", fixture.launch(&[]));
    assert_eq!("sharing=false version=1", fixture.launch(&[]));
    assert_eq!("sharing=true version=1", fixture.launch(&[]));
}

#[test]
fn leaves_the_jvm_alone_when_turned_off() {
    let Some(fixture) = AotFixture::new("off") else {
        return;
    };

    let output = fixture.run(&[], &[("ROAST_AOT", "off")]);
    assert_success(&output);
    assert_eq!("sharing=false version=1", stdout(&output));
    assert!(!fixture.cache.exists());
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "Roast fixture failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The runtime release of a JDK home, when it's recent enough for AOT caches
fn aot_jdk() -> Option<PathBuf> {
    let candidates: [Option<OsString>; 2] =
        [env::var_os("ROAST_TEST_AOT_JDK"), env::var_os("JAVA_HOME")];
    candidates
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .find(|home| {
            fs::read_to_string(home.join("release"))
                .ok()
                .and_then(|release| {
                    let line = release
                        .lines()
                        .find_map(|line| line.strip_prefix("JAVA_VERSION="))?;
                    line.trim_matches('"')
                        .split(|c: char| !c.is_ascii_digit())
                        .next()?
                        .parse::<u32>()
                        .ok()
                })
                .is_some_and(|version| version >= 25)
        })
}

struct AotFixture {
    _lock: MutexGuard<'static, ()>,
    jdk: PathBuf,
    root: PathBuf,
    install: PathBuf,
    caller: PathBuf,
    cache: PathBuf,
    launcher: PathBuf,
}

impl AotFixture {
    fn new(name: &str) -> Option<Self> {
        let Some(jdk) = aot_jdk() else {
            eprintln!("Skipping the AOT cache test: set ROAST_TEST_AOT_JDK to a Java 25+ JDK");
            return None;
        };
        let lock = LAUNCHER_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("roast aot {name} {unique} ø Ж"));
        let install = root.join("install");
        let caller = root.join("caller");
        fs::create_dir_all(install.join("app")).unwrap();
        fs::create_dir_all(&caller).unwrap();

        // Like construo's runtimes, without bin/java, which the JDK's own one-step training needs
        let runtime = install.join(runtime_folder());
        let jlink = Command::new(tool(&jdk, "jlink"))
            .current_dir(&root)
            .args(["--add-modules", "java.base", "--strip-native-commands"])
            .arg("--output")
            .arg(runtime.strip_prefix(&root).unwrap())
            .output()
            .unwrap();
        assert_success(&jlink);

        let launcher_name = if cfg!(target_os = "windows") {
            "roast-aot-probe.exe"
        } else {
            "roast-aot-probe"
        };
        let launcher = install.join(launcher_name);
        fs::copy(env!("CARGO_BIN_EXE_roast"), &launcher).unwrap();
        make_executable(&launcher);
        fs::write(
            install.join("app").join(
                Path::new(launcher_name)
                    .with_extension("json")
                    .file_name()
                    .unwrap(),
            ),
            r#"{
  "classPath": ["app/probe.jar"],
  "mainClass": "roast.probe.AotProbe",
  "runOnFirstThread": true,
  "aotCache": {}
}"#,
        )
        .unwrap();

        let root = root.canonicalize().unwrap();
        let fixture = Self {
            _lock: lock,
            jdk,
            cache: root.join("cache"),
            install: root.join("install"),
            caller: root.join("caller"),
            launcher: root.join("install").join(launcher_name),
            root,
        };
        fixture.build_app("1");
        Some(fixture)
    }

    /// Builds the probe's jar, which reports the version it was built with
    fn build_app(&self, version: &str) {
        let source = self.root.join("AotProbe.java");
        fs::write(&source, AOT_PROBE.replace("@VERSION@", version)).unwrap();
        let classes = self.root.join("classes");
        let _ = fs::remove_dir_all(&classes);
        let javac = Command::new(tool(&self.jdk, "javac"))
            .current_dir(&self.root)
            .args(["-d", "classes", "AotProbe.java"])
            .output()
            .unwrap();
        assert_success(&javac);
        let jar = Command::new(tool(&self.jdk, "jar"))
            .current_dir(&self.root)
            .args(["--create", "--file"])
            .arg(
                self.install
                    .join("app")
                    .join("probe.jar")
                    .strip_prefix(&self.root)
                    .unwrap(),
            )
            .args(["-C", "classes", "."])
            .output()
            .unwrap();
        assert_success(&jar);
    }

    fn run(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        Command::new(&self.launcher)
            .args(args)
            .current_dir(&self.caller)
            .env("ROAST_AOT_CACHE_DIR", &self.cache)
            .env_remove("JAVA_TOOL_OPTIONS")
            .env_remove("_JAVA_OPTIONS")
            .envs(envs.iter().copied())
            .output()
            .expect("failed to launch Roast fixture")
    }

    /// Runs the probe and returns what it printed, which must be all that reached stdout
    fn launch(&self, args: &[&str]) -> String {
        let output = self.run(args, &[]);
        assert_success(&output);
        let stdout = stdout(&output);
        assert_eq!(1, stdout.lines().count(), "stdout:\n{stdout}");
        stdout
    }

    fn files_ending(&self, suffix: &str) -> Vec<PathBuf> {
        fs::read_dir(&self.cache)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| path.to_string_lossy().ends_with(suffix))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Drop for AotFixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn tool(jdk: &Path, name: &str) -> PathBuf {
    jdk.join("bin")
        .join(format!("{name}{}", env::consts::EXE_SUFFIX))
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_: &Path) {}

#[cfg(all(
    target_os = "macos",
    feature = "macos_universal",
    target_arch = "aarch64"
))]
fn runtime_folder() -> &'static str {
    "runtime-aarch64"
}

#[cfg(all(
    target_os = "macos",
    feature = "macos_universal",
    target_arch = "x86_64"
))]
fn runtime_folder() -> &'static str {
    "runtime-x64"
}

#[cfg(any(
    not(target_os = "macos"),
    all(target_os = "macos", not(feature = "macos_universal"))
))]
fn runtime_folder() -> &'static str {
    "runtime"
}

const AOT_PROBE: &str = r#"
package roast.probe;

import java.util.List;
import java.util.stream.Collectors;

public final class AotProbe {
    public static void main(String[] args) {
        // Gives the cache some classes and lambdas to hold
        List<String> words = List.of("roast", "aot", "probe");
        String joined = words.stream().map(String::toUpperCase).collect(Collectors.joining(","));
        if (joined.isEmpty()) {
            throw new IllegalStateException();
        }
        boolean sharing = System.getProperty("java.vm.info").contains("sharing");
        System.out.println("sharing=" + sharing + " version=@VERSION@");
        if (args.length > 0 && args[0].equals("exit")) {
            System.exit(0);
        }
    }
}
"#;
