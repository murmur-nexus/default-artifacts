//! What the capsule grants (`capabilities.shell.allow`) and what the host has on `PATH`.

#[cfg(test)]
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The programs the display needs, each with the Debian/Ubuntu package that ships it.
pub const DISPLAY_PROGRAMS: [(&str, &str); 2] = [("Xvfb", "xvfb"), ("xkbcomp", "x11-xkb-utils")];

/// Window managers this tool starts when one is granted, in order of preference.
pub const WINDOW_MANAGERS: [&str; 5] = [
    "openbox",
    "fluxbox",
    "icewm",
    "twm",
    "matchbox-window-manager",
];

/// The capsule's grants. A program counts as granted exactly when the runtime staged
/// `$MURMUR_RUNTIME_DIR/tools/<name>/murmur.yaml` as a regular file.
#[derive(Debug, Clone)]
pub enum Grants {
    RuntimeDir(Option<PathBuf>),
    #[cfg(test)]
    Names(BTreeSet<String>),
}

impl Grants {
    /// From `MURMUR_RUNTIME_DIR`; unset means nothing is granted.
    pub fn from_env() -> Self {
        Grants::RuntimeDir(std::env::var_os("MURMUR_RUNTIME_DIR").map(PathBuf::from))
    }

    #[cfg(test)]
    pub fn from_names(names: &[&str]) -> Self {
        Grants::Names(names.iter().map(|s| s.to_string()).collect())
    }

    pub fn granted(&self, name: &str) -> bool {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return false;
        }
        match self {
            Grants::RuntimeDir(None) => false,
            Grants::RuntimeDir(Some(dir)) => {
                std::fs::symlink_metadata(dir.join("tools").join(name).join("murmur.yaml"))
                    .is_ok_and(|meta| meta.file_type().is_file())
            }
            #[cfg(test)]
            Grants::Names(names) => names.contains(name),
        }
    }
}

/// The exact line for display programs the capsule does not grant, or `None` when both are.
pub fn missing_display_grants(grants: &Grants) -> Option<String> {
    let missing: Vec<&str> = DISPLAY_PROGRAMS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !grants.granted(name))
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "tool-computer needs a display server: add {} to capabilities.shell.allow",
        missing.join(" and ")
    ))
}

/// The first executable `name` on `path`.
pub fn which(name: &str, path: Option<&OsString>) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The line for a granted program that is not on `PATH`.
pub fn not_installed_line(name: &str, package: Option<&str>) -> String {
    let install = match package {
        Some(package) => format!("install the {package} package on this host first"),
        None => format!("install {name} on this host first"),
    };
    format!(
        "tool-computer: capabilities.shell.allow grants {name}, but no {name} is on PATH; {install} \
         (mur deploy run does not upload host programs)"
    )
}

/// The host programs a display start runs, resolved on `PATH`.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub xvfb: PathBuf,
    /// The window manager's name and path, when one is granted and installed.
    pub window_manager: Option<(String, PathBuf)>,
    /// Each `launch` entry's program path, in order.
    pub launch: Vec<PathBuf>,
}

/// Resolves the display programs, each `launch` program and the window manager on `path`.
pub fn resolve(
    grants: &Grants,
    launch: &[Vec<String>],
    path: Option<&OsString>,
) -> Result<Resolved, String> {
    let mut xvfb = None;
    for (name, package) in DISPLAY_PROGRAMS {
        match which(name, path) {
            Some(found) => {
                if name == "Xvfb" {
                    xvfb = Some(found);
                }
            }
            None => return Err(not_installed_line(name, Some(package))),
        }
    }
    let mut programs = Vec::with_capacity(launch.len());
    for argv in launch {
        let name = &argv[0];
        let found = which(name, path).ok_or_else(|| not_installed_line(name, None))?;
        programs.push(found);
    }
    let window_manager = WINDOW_MANAGERS.iter().find_map(|name| {
        if !grants.granted(name) {
            return None;
        }
        which(name, path).map(|found| (name.to_string(), found))
    });
    Ok(Resolved {
        xvfb: xvfb.expect("Xvfb resolved above"),
        window_manager,
        launch: programs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_are_regular_files_under_the_runtime_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tools/Xvfb")).unwrap();
        std::fs::write(dir.path().join("tools/Xvfb/murmur.yaml"), "name: Xvfb\n").unwrap();
        std::fs::create_dir_all(dir.path().join("tools/xkbcomp/murmur.yaml")).unwrap();
        let grants = Grants::RuntimeDir(Some(dir.path().to_path_buf()));
        assert!(grants.granted("Xvfb"));
        assert!(!grants.granted("xkbcomp"), "a directory is no grant");
        assert!(!grants.granted("../tools/Xvfb"));
        assert_eq!(
            missing_display_grants(&grants).unwrap(),
            "tool-computer needs a display server: add xkbcomp to capabilities.shell.allow"
        );
        assert_eq!(
            missing_display_grants(&Grants::RuntimeDir(None)).unwrap(),
            "tool-computer needs a display server: add Xvfb and xkbcomp to capabilities.shell.allow"
        );
    }

    #[test]
    fn the_not_installed_lines_name_the_package() {
        assert_eq!(
            not_installed_line("Xvfb", Some("xvfb")),
            "tool-computer: capabilities.shell.allow grants Xvfb, but no Xvfb is on PATH; install \
             the xvfb package on this host first (mur deploy run does not upload host programs)"
        );
        assert!(not_installed_line("zenity", None).contains("install zenity on this host first"));
    }
}
