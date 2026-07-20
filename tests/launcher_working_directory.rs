use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

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
    original: PathBuf,
    root: PathBuf,
    install: PathBuf,
    caller: PathBuf,
    launcher: PathBuf,
}

impl LauncherFixture {
    fn new() -> Self {
        let original = env::current_dir().unwrap();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("roast launcher {unique} ø"));
        let install = root.join("install");
        let caller = root.join("caller");
        let classes = install.join("app/classes");
        fs::create_dir_all(&classes).unwrap();
        fs::create_dir_all(&caller).unwrap();

        let source = root.join("WorkingDirectoryProbe.java");
        fs::write(&source, JAVA_PROBE).unwrap();
        let javac = Command::new("javac")
            .arg("-d")
            .arg(&classes)
            .arg(&source)
            .output()
            .expect("javac must be installed for the launcher integration test");
        assert_success("javac", &javac);

        let runtime = install.join(runtime_folder());
        let jlink = Command::new("jlink")
            .args(["--add-modules", "java.base", "--output"])
            .arg(&runtime)
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
        fs::write(config, ROAST_CONFIG).unwrap();

        let root = root.canonicalize().unwrap();
        Self {
            original,
            install: root.join("install"),
            caller: root.join("caller"),
            launcher: root.join("install").join(launcher_name),
            root,
        }
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
