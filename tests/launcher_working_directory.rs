use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const EXIT_TIMEOUT: Duration = Duration::from_secs(60);

// A launcher fails to start with "Text file busy" if another test forks while it's being copied
static LAUNCHER_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn launches_jvm_from_install_and_restores_caller_directory() {
    let fixture = LauncherFixture::new();
    let output = Command::new(&fixture.launcher)
        .current_dir(&fixture.caller)
        .output()
        .expect("failed to launch Roast fixture");

    assert_success("Roast fixture", &output);
    let user_dir = fs::read_to_string(fixture.caller.join("roast-user-dir.txt")).unwrap();

    assert_eq!(fixture.caller, canonical_path(&user_dir));
    assert_eq!(
        "created by Java",
        fs::read_to_string(fixture.caller.join("roast-relative-marker.txt")).unwrap()
    );
    assert_eq!(
        "created by child",
        fs::read_to_string(fixture.caller.join("roast-native-marker.txt"))
            .unwrap()
            .trim()
    );
    assert!(fixture.install.join("roast-bootstrap.log").is_file());
    assert!(!fixture.caller.join("roast-bootstrap.log").exists());
    assert!(!fixture.install.join("roast-relative-marker.txt").exists());
    assert!(!fixture.install.join("roast-native-marker.txt").exists());
}

#[test]
fn finishes_worker_threads_and_shutdown_hooks_after_main_returns() {
    for run_on_first_thread in [true, false] {
        let fixture = LauncherFixture::exit_probe(run_on_first_thread);
        let output = fixture.run_exit_probe("return");

        assert_success("Roast fixture", &output);
        assert_eq!("worker\nhook\n", fixture.exit_events());
    }
}

#[test]
fn exits_with_failure_when_main_throws() {
    for run_on_first_thread in [true, false] {
        let fixture = LauncherFixture::exit_probe(run_on_first_thread);
        let output = fixture.run_exit_probe("throw");

        assert_eq!(Some(1), output.status.code());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Exception in thread \"main\""), "{stderr}");
        assert!(stderr.contains("thrown by the exit probe"), "{stderr}");
        assert_eq!("hook\n", fixture.exit_events());
    }
}

#[test]
fn passes_uncaught_exceptions_to_the_default_handler_and_ignores_its_failures() {
    for run_on_first_thread in [true, false] {
        let fixture = LauncherFixture::exit_probe(run_on_first_thread);
        let output = fixture.run_exit_probe("handler");

        assert_eq!(Some(1), output.status.code());
        assert_eq!("handler main\nhook\n", fixture.exit_events());
    }
}

#[test]
fn keeps_the_status_passed_to_system_exit() {
    for run_on_first_thread in [true, false] {
        let fixture = LauncherFixture::exit_probe(run_on_first_thread);
        let output = fixture.run_exit_probe("exit");

        assert_eq!(Some(3), output.status.code());
        assert_eq!("hook\n", fixture.exit_events());
    }
}

#[test]
fn runs_main_on_the_main_thread_with_the_system_class_loader() {
    for run_on_first_thread in [true, false] {
        let fixture = LauncherFixture::exit_probe(run_on_first_thread);
        let output = fixture.run_exit_probe("thread");

        assert_success("Roast fixture", &output);
        assert_eq!("main true\nhook\n", fixture.exit_events());
    }
}

#[test]
fn exits_with_failure_when_the_main_class_is_missing() {
    for run_on_first_thread in [true, false] {
        let fixture = LauncherFixture::exit_probe_with_main("Missing", run_on_first_thread);
        let output = fixture.run_exit_probe("return");

        assert_eq!(Some(1), output.status.code());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Missing"));
    }
}

#[test]
fn exits_with_failure_when_the_main_method_is_missing() {
    for run_on_first_thread in [true, false] {
        let fixture = LauncherFixture::exit_probe_with_main("NoMain", run_on_first_thread);
        let output = fixture.run_exit_probe("return");

        assert_eq!(Some(1), output.status.code());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Method not found: main"));
    }
}

#[test]
fn reports_a_main_class_that_fails_to_initialize_as_uncaught() {
    for run_on_first_thread in [true, false] {
        let fixture = LauncherFixture::exit_probe_with_main("BrokenInit", run_on_first_thread);
        let output = fixture.run_exit_probe("return");

        assert_eq!(Some(1), output.status.code());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Exception in thread \"main\""), "{stderr}");
        assert!(stderr.contains("thrown by the initializer"), "{stderr}");
    }
}

fn canonical_path(path: &str) -> PathBuf {
    Path::new(path)
        .canonicalize()
        .unwrap_or_else(|error| panic!("failed to canonicalize {path:?}: {error}"))
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct LauncherFixture {
    _lock: MutexGuard<'static, ()>,
    original: PathBuf,
    root: PathBuf,
    install: PathBuf,
    caller: PathBuf,
    launcher: PathBuf,
}

impl LauncherFixture {
    fn new() -> Self {
        Self::with_probe("WorkingDirectoryProbe", JAVA_PROBE, ROAST_CONFIG)
    }

    fn exit_probe(run_on_first_thread: bool) -> Self {
        Self::exit_probe_with_main("ExitProbe", run_on_first_thread)
    }

    fn exit_probe_with_main(main_class: &str, run_on_first_thread: bool) -> Self {
        let config = format!(
            r#"{{
  "classPath": ["app/classes"],
  "mainClass": "roast.probe.{main_class}",
  "runOnFirstThread": {run_on_first_thread}
}}"#
        );
        Self::with_probe("ExitProbe", EXIT_PROBE, &config)
    }

    fn with_probe(name: &str, probe: &str, roast_config: &str) -> Self {
        let lock = LAUNCHER_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let original = env::current_dir().unwrap();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Ж is missing from Western code pages and ø from Cyrillic ones, so on those Windows systems
        // these paths only reach the JVM through the UTF-8 code page that roast's manifest selects
        let root = env::temp_dir().join(format!("roast launcher {unique} ø Ж"));
        let install = root.join("install");
        let caller = root.join("caller");
        let classes = install.join("app/classes");
        fs::create_dir_all(&classes).unwrap();
        fs::create_dir_all(&caller).unwrap();

        // The JDK tools read their arguments in the system code page, so they get relative paths
        let source = root.join(format!("{name}.java"));
        fs::write(&source, probe).unwrap();
        let javac = Command::new("javac")
            .current_dir(&root)
            .arg("-d")
            .arg(classes.strip_prefix(&root).unwrap())
            .arg(source.strip_prefix(&root).unwrap())
            .output()
            .expect("javac must be installed for the launcher integration test");
        assert_success("javac", &javac);

        let runtime = install.join(runtime_folder());
        let jlink = Command::new("jlink")
            .current_dir(&root)
            .args(["--add-modules", "java.base", "--output"])
            .arg(runtime.strip_prefix(&root).unwrap())
            .output()
            .expect("jlink must be installed for the launcher integration test");
        assert_success("jlink", &jlink);

        let launcher_name = if cfg!(target_os = "windows") {
            "roast-cwd-probe.exe"
        } else {
            "roast-cwd-probe"
        };
        let launcher = install.join(launcher_name);
        fs::copy(env!("CARGO_BIN_EXE_roast"), &launcher).unwrap();
        make_executable(&launcher);

        let config = install.join("app").join(
            Path::new(launcher_name)
                .with_extension("json")
                .file_name()
                .unwrap(),
        );
        fs::write(config, roast_config).unwrap();

        let root = root.canonicalize().unwrap();
        Self {
            _lock: lock,
            original,
            install: root.join("install"),
            caller: root.join("caller"),
            launcher: root.join("install").join(launcher_name),
            root,
        }
    }

    fn run_exit_probe(&self, mode: &str) -> Output {
        let mut child = Command::new(&self.launcher)
            .arg(mode)
            .current_dir(&self.caller)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to launch Roast fixture");
        let started = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > EXIT_TIMEOUT {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("Roast fixture was still running after {mode}");
            }
            thread::sleep(Duration::from_millis(50));
        }
        child.wait_with_output().unwrap()
    }

    fn exit_events(&self) -> String {
        fs::read_to_string(self.caller.join("roast-exit-events.txt")).unwrap_or_default()
    }
}

impl Drop for LauncherFixture {
    fn drop(&mut self) {
        env::set_current_dir(&self.original).unwrap();
        fs::remove_dir_all(&self.root).unwrap();
    }
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

const ROAST_CONFIG: &str = r#"{
  "classPath": ["app/classes"],
  "mainClass": "WorkingDirectoryProbe",
  "vmArgs": ["-Duser.dir=/wrong", "-Xlog:os=info:file=roast-bootstrap.log"],
  "runOnFirstThread": true
}"#;

const EXIT_PROBE: &str = r#"
package roast.probe;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

public final class ExitProbe {
    public static void main(String[] args) {
        Runtime.getRuntime().addShutdownHook(new Thread(() -> record("hook")));
        if (args[0].equals("throw")) {
            throw new IllegalStateException("thrown by the exit probe");
        }
        if (args[0].equals("handler")) {
            Thread.setDefaultUncaughtExceptionHandler((thread, error) -> {
                record("handler " + thread.getName());
                throw new IllegalStateException("thrown by the handler");
            });
            throw new IllegalStateException("thrown by the exit probe");
        }
        if (args[0].equals("exit")) {
            System.exit(3);
        }
        if (args[0].equals("thread")) {
            ClassLoader loader = Thread.currentThread().getContextClassLoader();
            record(Thread.currentThread().getName() + " " + (loader == ClassLoader.getSystemClassLoader()));
            return;
        }
        new Thread(() -> {
            try {
                Thread.sleep(500);
            } catch (InterruptedException error) {
                throw new IllegalStateException(error);
            }
            record("worker");
        }).start();
    }

    private static void record(String event) {
        try {
            Files.writeString(
                Path.of("roast-exit-events.txt"),
                event + "\n",
                StandardOpenOption.CREATE,
                StandardOpenOption.APPEND
            );
        } catch (IOException error) {
            throw new UncheckedIOException(error);
        }
    }
}

final class NoMain {}

final class BrokenInit {
    static {
        if (true) {
            throw new IllegalStateException("thrown by the initializer");
        }
    }

    public static void main(String[] args) {}
}
"#;

const JAVA_PROBE: &str = r#"
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

public final class WorkingDirectoryProbe {
    public static void main(String[] args) throws Exception {
        boolean windows = System.getProperty("os.name").startsWith("Windows");
        Process process = windows
            ? new ProcessBuilder("cmd.exe", "/d", "/c", "echo created by child>roast-native-marker.txt").start()
            : new ProcessBuilder("sh", "-c", "printf 'created by child' > roast-native-marker.txt").start();
        int exitCode = process.waitFor();
        if (exitCode != 0) {
            throw new IllegalStateException("working-directory probe exited " + exitCode);
        }

        Files.writeString(
            Path.of("roast-user-dir.txt"),
            System.getProperty("user.dir"),
            StandardCharsets.UTF_8
        );
        Files.writeString(Path.of("roast-relative-marker.txt"), "created by Java");
    }
}
"#;
