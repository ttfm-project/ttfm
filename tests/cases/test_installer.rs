// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const INSTALLER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/ttfm_cli.sh");
const COMPLETION_DIR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/completion");

fn release_target() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "macos") => "aarch64-apple-darwin",
        (arch, os) => panic!("no release target for {arch}/{os}"),
    }
}

struct Sandbox {
    _root: TempDir,
    home: PathBuf,
    install_dir: PathBuf,
    data_home: PathBuf,
    config_home: PathBuf,
    ttfm_home: PathBuf,
    stub_bin: PathBuf,
    release_base: PathBuf,
    target: String,
}

impl Sandbox {
    fn bin(&self) -> PathBuf {
        self.install_dir.join("ttfm")
    }

    fn bash_completion(&self) -> PathBuf {
        self.data_home.join("bash-completion/completions/ttfm")
    }

    fn fish_completion(&self) -> PathBuf {
        self.config_home.join("fish/completions/ttfm.fish")
    }

    fn download_dir(&self) -> PathBuf {
        self.release_base.join("latest/download")
    }

    fn asset(&self) -> String {
        format!("ttfm-{}.tar.gz", self.target)
    }
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .unwrap();
}

fn run_ok(cmd: &mut Command) {
    let status = cmd.status().unwrap();
    assert!(status.success(), "command failed: {cmd:?}");
}

fn sandbox() -> Sandbox {
    sandbox_for(release_target())
}

fn sandbox_for(target: &str) -> Sandbox {
    let root = tempfile::tempdir().unwrap();
    let base = root.path();
    let sb = Sandbox {
        home: base.join("home"),
        install_dir: base.join("home/bin"),
        data_home: base.join("home/data"),
        config_home: base.join("home/config"),
        ttfm_home: base.join("home/ttfm"),
        stub_bin: base.join("stub_bin"),
        release_base: base.join("releases"),
        target: target.to_string(),
        _root: root,
    };
    let staging = sb._root.path().join("staging");
    for dir in [&sb.home, &sb.stub_bin, &staging, &sb.download_dir()] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::copy(env!("CARGO_BIN_EXE_ttfm"), staging.join("ttfm")).unwrap();
    run_ok(
        Command::new("tar")
            .arg("czf")
            .arg(sb.download_dir().join(sb.asset()))
            .arg("-C")
            .arg(&staging)
            .arg("ttfm"),
    );
    let checksum = Command::new("shasum")
        .args(["-a", "256", &sb.asset()])
        .current_dir(sb.download_dir())
        .output()
        .unwrap();
    assert!(checksum.status.success());
    std::fs::write(
        sb.download_dir().join(format!("{}.sha256", sb.asset())),
        checksum.stdout,
    )
    .unwrap();
    write_executable(&sb.stub_bin.join("fish"), "#!/bin/sh\n");
    sb
}

fn stub_darwin_x86_64(sb: &Sandbox, arm64_capable: bool) {
    write_executable(
        &sb.stub_bin.join("uname"),
        "#!/bin/sh\ncase \"$1\" in -s) echo Darwin ;; -m) echo x86_64 ;; esac\n",
    );
    let arm64 = if arm64_capable { "1" } else { "0" };
    write_executable(
        &sb.stub_bin.join("sysctl"),
        &format!("#!/bin/sh\necho {arm64}\n"),
    );
}

fn prepend_path(dir: &Path) -> std::ffi::OsString {
    let mut path = dir.as_os_str().to_os_string();
    if let Some(current) = std::env::var_os("PATH") {
        path.push(":");
        path.push(current);
    }
    path
}

fn run_installer(sb: &Sandbox, args: &[&str]) -> Output {
    let mut download_base = std::ffi::OsString::from("file://");
    download_base.push(&sb.release_base);
    Command::new("sh")
        .arg(INSTALLER)
        .args(args)
        .env("HOME", &sb.home)
        .env("TTFM_INSTALL_DIR", &sb.install_dir)
        .env("XDG_DATA_HOME", &sb.data_home)
        .env("XDG_CONFIG_HOME", &sb.config_home)
        .env("TTFM_HOME", &sb.ttfm_home)
        .env("TTFM_DOWNLOAD_BASE", download_base)
        .env_remove("TTFM_VERSION")
        .env("PATH", prepend_path(&sb.stub_bin))
        .output()
        .unwrap()
}

fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "installer failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn completion_script(file: &str) -> String {
    std::fs::read_to_string(Path::new(COMPLETION_DIR).join(file)).unwrap()
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[test]
fn test_install_places_binary_and_completions() {
    let sb = sandbox();
    let out = run_installer(&sb, &[]);
    assert_success(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(is_executable(&sb.bin()));
    assert_eq!(
        std::fs::read_to_string(sb.bash_completion()).unwrap(),
        completion_script("ttfm.bash")
    );
    assert_eq!(
        std::fs::read_to_string(sb.fish_completion()).unwrap(),
        completion_script("ttfm.fish")
    );
    assert!(stdout.contains("source <(ttfm completion zsh)"));
    assert!(stdout.contains(sb.install_dir.to_str().unwrap()));
    assert!(!sb.home.join(".bashrc").exists());
    assert!(!sb.home.join(".zshrc").exists());
}

#[test]
fn test_install_under_rosetta_selects_apple_silicon() {
    let sb = sandbox_for("aarch64-apple-darwin");
    stub_darwin_x86_64(&sb, true);
    let out = run_installer(&sb, &[]);
    assert_success(&out);
    assert!(is_executable(&sb.bin()));
}

#[test]
fn test_install_rejects_intel_mac_with_source_build_hint() {
    let sb = sandbox_for("aarch64-apple-darwin");
    stub_darwin_x86_64(&sb, false);
    let out = run_installer(&sb, &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cargo install --git https://github.com/coponhub/TTFM"),
        "stderr: {stderr}"
    );
    assert!(!sb.bin().exists());
}

#[test]
fn test_install_rejects_checksum_mismatch() {
    let sb = sandbox();
    std::fs::write(
        sb.download_dir().join(format!("{}.sha256", sb.asset())),
        format!("{}  {}\n", "0".repeat(64), sb.asset()),
    )
    .unwrap();
    let out = run_installer(&sb, &[]);
    assert!(!out.status.success());
    assert!(!sb.bin().exists());
}

#[test]
fn test_uninstall_removes_binary_and_completions_keeps_home() {
    let sb = sandbox();
    assert_success(&run_installer(&sb, &[]));
    std::fs::create_dir_all(sb.ttfm_home.join("db")).unwrap();
    let out = run_installer(&sb, &["--uninstall"]);
    assert_success(&out);
    assert!(!sb.bin().exists());
    assert!(!sb.bash_completion().exists());
    assert!(!sb.fish_completion().exists());
    assert!(sb.ttfm_home.join("db").exists());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(sb.ttfm_home.to_str().unwrap()));
}

#[test]
fn test_uninstall_purge_removes_ttfm_home() {
    let sb = sandbox();
    assert_success(&run_installer(&sb, &[]));
    std::fs::create_dir_all(sb.ttfm_home.join("db")).unwrap();
    assert_success(&run_installer(&sb, &["--uninstall", "--purge"]));
    assert!(!sb.ttfm_home.exists());
}

#[test]
fn test_purge_without_uninstall_is_rejected() {
    let sb = sandbox();
    let out = run_installer(&sb, &["--purge"]);
    assert!(!out.status.success());
    assert!(!sb.bin().exists());
}
