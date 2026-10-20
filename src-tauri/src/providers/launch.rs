//! Platform-aware harness launch without putting model settings through a batch shell.
use std::{
    ffi::OsString,
    io::Read,
    path::{Component, Path, PathBuf},
};

struct Launch {
    program: PathBuf,
    args: Vec<OsString>,
}

/// Canonical Windows paths are useful for native file operations, but Node and
/// ACP servers may interpret the extended namespace as part of the drive name.
/// Only simplify paths with ordinary Win32 semantics; device/long/special paths
/// must retain their prefix rather than silently refer to a different file.
pub(crate) fn external_path(path: &Path) -> PathBuf {
    external_path_for(path, cfg!(windows))
}

fn external_path_for(path: &Path, windows: bool) -> PathBuf {
    if windows {
        if let Some(text) = path.to_str() {
            if let Some(path) = ordinary_windows_path(text) {
                return path.into();
            }
        }
    }
    path.to_owned()
}

fn ordinary_windows_path(path: &str) -> Option<String> {
    let extended = path.strip_prefix(r"\\?\")?;
    let ordinary = if let Some(unc) = extended.strip_prefix(r"UNC\") {
        if unc.split('\\').filter(|part| !part.is_empty()).count() < 2 {
            return None;
        }
        format!(r"\\{unc}")
    } else if extended.as_bytes().get(1..3) == Some(b":\\")
        && extended.as_bytes()[0].is_ascii_alphabetic()
    {
        extended.to_owned()
    } else {
        return None;
    };
    if ordinary.encode_utf16().count() >= 260 {
        return None;
    }
    for part in ordinary.split('\\').filter(|part| !part.is_empty()) {
        let name = part.split('.').next().unwrap_or_default().to_ascii_uppercase();
        if part.ends_with([' ', '.'])
            || part.contains(['/', '*', '?'])
            || matches!(name.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
            || (name.len() == 4
                && (name.starts_with("COM") || name.starts_with("LPT"))
                && matches!(name.as_bytes()[3], b'1'..=b'9'))
            || ["COM¹", "COM²", "COM³", "LPT¹", "LPT²", "LPT³"].contains(&name.as_str())
        {
            return None;
        }
    }
    Some(ordinary)
}

fn resolve(path: &Path, windows: bool) -> Result<Launch, String> {
    let batch = path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
    });
    if !windows || !batch {
        return Ok(Launch {
            program: external_path_for(path, windows),
            args: Vec::new(),
        });
    }
    // npm shims forward arguments through cmd.exe. Launch their declared bin
    // directly instead, so paths, JSON and model names never become shell syntax.
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let package =
        match name.as_str() {
            "codex" => "@openai/codex",
            "claude" => "@anthropic-ai/claude-code",
            "opencode" => "opencode-ai",
            "pi" => "@earendil-works/pi-coding-agent",
            _ => return Err(
                "This batch launcher is unsupported. Select the native harness executable instead."
                    .into(),
            ),
        };
    let directory = path
        .parent()
        .ok_or("The npm launcher has no installation folder.")?;
    let modules = if directory.file_name().is_some_and(|name| name == ".bin") {
        directory
            .parent()
            .ok_or("The npm launcher has no package folder.")?
            .to_path_buf()
    } else {
        directory.join("node_modules")
    };
    let package = modules.join(package).canonicalize().map_err(|_| "The npm harness package could not be found. Reinstall the harness or select its native executable.")?;
    let file = std::fs::File::open(package.join("package.json"))
        .map_err(|_| "Could not read the npm harness package metadata.")?;
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the npm harness package metadata.")?;
    if bytes.len() > 64 * 1024 {
        return Err("The npm harness metadata is too large.".into());
    }
    let metadata: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| "The npm harness package metadata is invalid.")?;
    let bin = metadata["bin"]
        .as_str()
        .or_else(|| metadata["bin"][&name].as_str())
        .ok_or("The npm harness package has no matching executable entry.")?
        .replace('\\', "/");
    if bin.is_empty()
        || bin.chars().any(char::is_control)
        || Path::new(&bin)
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err("The npm harness executable must stay inside its package.".into());
    }
    let script = package
        .join(bin)
        .canonicalize()
        .map_err(|_| "The npm harness executable could not be found.")?;
    if !script.starts_with(&package) || !script.is_file() {
        return Err("The npm harness executable must stay inside its package.".into());
    }
    if script
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return Ok(Launch {
            program: external_path_for(&script, windows),
            args: Vec::new(),
        });
    }
    let adjacent_node = directory.join("node.exe");
    let node = adjacent_node.is_file().then_some(adjacent_node)
        .or_else(|| find_native_node(search_directories(true, &|key| std::env::var_os(key))))
        .ok_or("Node.js is required by this npm-installed harness. Install Node.js or use the native harness installer.")?;
    Ok(Launch {
        program: external_path_for(&node, windows),
        args: vec![external_path_for(&script, windows).into_os_string()],
    })
}

fn find_native_node(directories: Vec<PathBuf>) -> Option<PathBuf> {
    directories
        .into_iter()
        .map(|dir| dir.join("node.exe"))
        .find(|path| path.is_file())
}
pub(crate) fn command(path: &Path) -> Result<tokio::process::Command, String> {
    let launch = resolve(path, cfg!(windows))?;
    let mut command = tokio::process::Command::new(launch.program);
    command.args(launch.args);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW: stdio adapters have no console UI.
    Ok(command)
}

pub(super) fn std_command(path: &Path) -> Result<std::process::Command, String> {
    let launch = resolve(path, cfg!(windows))?;
    let mut command = std::process::Command::new(launch.program);
    command.args(launch.args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    Ok(command)
}

pub(crate) fn pty_command(path: &Path) -> Result<portable_pty::CommandBuilder, String> {
    let launch = resolve(path, cfg!(windows))?;
    let mut command = portable_pty::CommandBuilder::new(launch.program);
    command.args(launch.args);
    Ok(command)
}

fn home_directory(windows: bool, env: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let first = if windows { "USERPROFILE" } else { "HOME" };
    let second = if windows { "HOME" } else { "USERPROFILE" };
    env(first)
        .filter(|value| !value.is_empty())
        .or_else(|| env(second).filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .or_else(|| {
            windows
                .then(|| {
                    let mut drive = env("HOMEDRIVE")?;
                    drive.push(env("HOMEPATH")?);
                    Some(PathBuf::from(drive))
                })
                .flatten()
        })
}

fn search_directories(windows: bool, env: &impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = env("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();
    let home = home_directory(windows, env);
    if let Some(home) = &home {
        for dir in [
            ".local/bin",
            ".kimi-code/bin",
            ".kimi-code/Scripts",
            ".opencode/bin",
            ".cargo/bin",
            ".bun/bin",
            ".volta/bin",
            ".npm-global/bin",
        ] {
            dirs.push(home.join(dir));
        }
    }
    if windows {
        if let Some(appdata) = env("APPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join("AppData/Roaming")))
        {
            dirs.push(appdata.join("npm"));
        }
        if let Some(local) = env("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join("AppData/Local")))
        {
            dirs.push(local.join("Microsoft/WinGet/Links"));
        }
        if let Some(programs) = env("ProgramFiles").filter(|value| !value.is_empty()) {
            dirs.push(PathBuf::from(programs).join("nodejs"));
        }
        if let Some(system) = env("SystemRoot").filter(|value| !value.is_empty()) {
            dirs.push(PathBuf::from(system).join("System32/WindowsPowerShell/v1.0"));
        }
    } else {
        for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
            dirs.push(PathBuf::from(dir));
        }
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|dir| seen.insert(dir.clone()));
    dirs
}

/// GUI launches inherit a minimal PATH. Use the same ordered locations as CLI
/// discovery for scripts, without evaluating a user's shell startup files.
pub(crate) fn executable_path() -> Result<OsString, String> {
    executable_path_for(cfg!(windows), &|name| std::env::var_os(name))
}

/// Widens this process's PATH to the installed-tool locations, once, at startup. Apps opened
/// from Finder or the Dock start with PATH=/usr/bin:/bin, and every agent, script and helper
/// PandaCode launches inherits it: without this, an agent cannot run `gh`, `node` or anything
/// else installed with Homebrew, even though the user's terminal can. Existing entries keep
/// their order; missing standard locations are appended.
pub(crate) fn widen_process_path() {
    if let Ok(path) = executable_path() {
        std::env::set_var("PATH", path);
    }
}

fn executable_path_for(
    windows: bool,
    env: &impl Fn(&str) -> Option<OsString>,
) -> Result<OsString, String> {
    std::env::join_paths(search_directories(windows, env))
        .map_err(|_| "Could not prepare the installed tools search path.".into())
}

fn find_in_directories(name: &str, dirs: Vec<PathBuf>, windows: bool) -> Option<PathBuf> {
    let extensions: &[&str] = if windows {
        &[".exe", ".com", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    for dir in dirs {
        for extension in extensions {
            let path = dir.join(format!("{name}{extension}"));
            let Ok(metadata) = path.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            #[cfg(unix)]
            if !windows {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            return Some(path);
        }
    }
    None
}

pub(super) fn find(name: &str) -> Option<PathBuf> {
    find_in_directories(
        name,
        search_directories(cfg!(windows), &|key| std::env::var_os(key)),
        cfg!(windows),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn windows_external_paths_remove_only_safe_namespace_prefixes() {
        for (extended, ordinary) in [
            (r"\\?\C:\project", r"C:\project"),
            (r"\\?\D:\npm path & spaces\node_modules\@openai\codex\bin\codex.js", r"D:\npm path & spaces\node_modules\@openai\codex\bin\codex.js"),
            (r"\\?\UNC\server\share\project", r"\\server\share\project"),
            (r"\\?\C:\", r"C:\"),
        ] {
            assert_eq!(ordinary_windows_path(extended).as_deref(), Some(ordinary));
            assert_eq!(external_path_for(Path::new(extended), true), Path::new(ordinary));
            assert_eq!(external_path_for(Path::new(extended), false), Path::new(extended));
        }
        for path in [r"C:\project", "/tmp/project", r"\\.\pipe\agent", r"\\?\Volume{abc}\project", r"\\?\C:\trailing.", r"\\?\C:\trailing ", r"\\?\C:\NUL.txt", r"\\?\C:\COM1", r"\\?\C:\a\..\b", r"\\?\UNC\server"] {
            assert_eq!(ordinary_windows_path(path), None, "{path}");
            assert_eq!(external_path_for(Path::new(path), true), Path::new(path));
        }
        assert!(ordinary_windows_path(&format!(r"\\?\C:\{}", "x".repeat(260))).is_none());
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("pandacode-launch-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn windows_npm_launches_node_entry_without_a_batch_shell() {
        let f = Fixture::new();
        let shim = f.file("npm path & spaces/codex.cmd", "@echo off\r\n");
        let node = f.file("npm path & spaces/node.exe", "");
        f.file(
            "npm path & spaces/node_modules/@openai/codex/package.json",
            r#"{"bin":{"codex":"bin/codex.js"}}"#,
        );
        let script = f.file(
            "npm path & spaces/node_modules/@openai/codex/bin/codex.js",
            "#!/usr/bin/env node\n",
        );
        let launch = resolve(&shim, true).unwrap();
        assert_eq!(launch.program, external_path_for(&node, true));
        assert_eq!(launch.args, [external_path_for(&script, true).into_os_string()]);
    }

    #[test]
    fn rejects_unknown_batch_files_and_npm_entries_outside_the_package() {
        let f = Fixture::new();
        let unknown = f.file("custom.cmd", "@echo off\r\n");
        assert!(resolve(&unknown, true).is_err());
        let shim = f.file("codex.cmd", "");
        f.file(
            "node_modules/@openai/codex/package.json",
            r#"{"bin":{"codex":"../../outside.js"}}"#,
        );
        assert!(resolve(&shim, true).is_err());
    }

    #[test]
    fn native_and_unix_program_paths_remain_direct() {
        for (path, windows) in [
            ("C:/tools/codex.exe", true),
            ("/opt/homebrew/bin/codex", false),
            ("/home/user/.local/bin/claude", false),
        ] {
            let launch = resolve(Path::new(path), windows).unwrap();
            assert_eq!(launch.program, Path::new(path));
            assert!(launch.args.is_empty());
        }
    }

    #[test]
    fn node_discovery_never_falls_back_to_a_batch_shell() {
        let f = Fixture::new();
        f.file("first/node.cmd", "@echo off");
        f.file("first/node.bat", "@echo off");
        f.file("first/node", "");
        assert!(find_native_node(vec![f.0.join("first")]).is_none());
        let node = f.file("second/node.exe", "");
        assert_eq!(
            find_native_node(vec![f.0.join("first"), f.0.join("second")]),
            Some(node)
        );
    }

    #[test]
    fn modern_native_bins_and_local_npm_shims_are_supported() {
        let f = Fixture::new();
        for (name, package) in [
            ("claude", "@anthropic-ai/claude-code"),
            ("opencode", "opencode-ai"),
            ("pi", "@earendil-works/pi-coding-agent"),
        ] {
            let shim = f.file(&format!("node_modules/.bin/{name}.cmd"), "@echo off");
            f.file(
                &format!("node_modules/{package}/package.json"),
                &serde_json::json!({"bin":{name:format!("bin/{name}.exe")}}).to_string(),
            );
            let binary = f.file(&format!("node_modules/{package}/bin/{name}.exe"), "");
            let launch = resolve(&shim, true).unwrap();
            assert_eq!(launch.program, external_path_for(&binary, true));
            assert!(launch.args.is_empty());
        }
    }

    #[test]
    fn platform_search_uses_real_windows_profile_and_appdata_locations() {
        let env = |name: &str| {
            Some(OsString::from(match name {
                "USERPROFILE" => "C:/Users/Person",
                "HOME" => "/git/home/person",
                "APPDATA" => "D:/Roaming Data",
                "LOCALAPPDATA" => "D:/Local Data",
                "ProgramFiles" => "C:/Program Files",
                "SystemRoot" => "C:/Windows",
                _ => return None,
            }))
        };
        let dirs = search_directories(true, &env);
        for path in [
            "C:/Users/Person/.local/bin",
            "C:/Users/Person/.kimi-code/Scripts",
            "D:/Roaming Data/npm",
            "D:/Local Data/Microsoft/WinGet/Links",
            "C:/Program Files/nodejs",
            "C:/Windows/System32/WindowsPowerShell/v1.0",
        ] {
            assert!(dirs.contains(&PathBuf::from(path)), "missing {path}");
        }
        assert!(!dirs.contains(&PathBuf::from("/git/home/person/.local/bin")));
        assert_eq!(
            home_directory(false, &env),
            Some(PathBuf::from("/git/home/person"))
        );
        let env = |name: &str| match name {
            "HOMEDRIVE" => Some("C:".into()),
            "HOMEPATH" => Some("/Users/Person".into()),
            _ => None,
        };
        assert_eq!(
            home_directory(true, &env),
            Some(PathBuf::from("C:/Users/Person"))
        );
    }

    #[test]
    fn script_path_preserves_existing_order_and_adds_installed_tool_locations() {
        let inherited = std::env::join_paths(["/custom/bin", "/usr/bin", "/custom/bin"]).unwrap();
        let env = |name: &str| match name {
            "PATH" => Some(inherited.clone()),
            "HOME" => Some("/Users/Person".into()),
            _ => None,
        };
        let path = executable_path_for(false, &env).unwrap();
        let dirs: Vec<_> = std::env::split_paths(&path).collect();
        assert_eq!(&dirs[..2], &[PathBuf::from("/custom/bin"), PathBuf::from("/usr/bin")]);
        for path in ["/Users/Person/.local/bin", "/Users/Person/.volta/bin", "/Users/Person/.bun/bin", "/opt/homebrew/bin", "/usr/local/bin", "/bin"] {
            assert!(dirs.contains(&PathBuf::from(path)), "missing {path}");
        }
        assert_eq!(dirs.iter().filter(|path| path.as_path() == Path::new("/custom/bin")).count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn scripts_find_installed_user_tools_with_a_minimal_gui_path() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let tool = f.file(".local/bin/pandacode-test-tool", "#!/bin/sh\nprintf 'found installed tool'\n");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let script = f.file("script.sh", "#!/bin/sh\npandacode-test-tool\n");
        let env = |name: &str| match name {
            "PATH" => Some("/usr/bin:/bin".into()),
            "HOME" => Some(f.0.clone().into_os_string()),
            _ => None,
        };
        let mut command = std::process::Command::new("/bin/sh");
        command.arg(script).current_dir(&f.0).env("PATH", executable_path_for(false, &env).unwrap());
        let output = crate::workspace::run_process(command, "GUI Run Script PATH test", 1024, std::time::Duration::from_secs(3)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.bytes, b"found installed tool");
    }

    #[test]
    fn native_executables_win_over_shims_and_missing_directories_do_not_stop_search() {
        let f = Fixture::new();
        f.file("tools/codex.cmd", "");
        let exe = f.file("tools/codex.exe", "");
        assert_eq!(
            find_in_directories("codex", vec![f.0.join("missing"), f.0.join("tools")], true),
            Some(exe)
        );
    }
}
