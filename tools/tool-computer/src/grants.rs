//! What the capsule grants (`capabilities.shell.allow`), as murmur stages it in the session's
//! runtime directory, and the lines for programs that are not granted or not installed.

#[cfg(test)]
use std::collections::BTreeSet;
use std::path::PathBuf;

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

/// The answer when the runtime directory is not preopened, which murmur does for an entry that
/// declares `capabilities.filesystem.scope`.
pub const SCOPE_LINE: &str = "tool-computer cannot see which programs capabilities.shell.allow grants, because its entry declares capabilities.filesystem.scope; tool-computer runs only from an entry without one";

/// The capsule's grants. A program counts as granted exactly when murmur staged
/// `tools/<name>/murmur.yaml` as a regular file in the runtime directory.
#[derive(Debug, Clone)]
pub enum Grants {
    /// The runtime directory, as the tool opens it.
    Runtime(PathBuf),
    /// No runtime directory is preopened, so no grant can be seen.
    Unknown,
    #[cfg(test)]
    Names(BTreeSet<String>),
}

impl Grants {
    pub fn new(runtime: Option<PathBuf>) -> Self {
        runtime.map_or(Grants::Unknown, Grants::Runtime)
    }

    #[cfg(test)]
    pub fn from_names(names: &[&str]) -> Self {
        Grants::Names(names.iter().map(|s| s.to_string()).collect())
    }

    /// Whether the grants can be seen at all.
    pub fn known(&self) -> bool {
        !matches!(self, Grants::Unknown)
    }

    pub fn granted(&self, name: &str) -> bool {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return false;
        }
        match self {
            Grants::Unknown => false,
            Grants::Runtime(dir) => dir
                .join("tools")
                .join(name)
                .join("murmur.yaml")
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_file()),
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

pub use murmur_process_host::is_not_found;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_are_regular_files_under_the_runtime_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tools/Xvfb")).unwrap();
        std::fs::write(dir.path().join("tools/Xvfb/murmur.yaml"), "name: Xvfb\n").unwrap();
        std::fs::create_dir_all(dir.path().join("tools/xkbcomp/murmur.yaml")).unwrap();
        let grants = Grants::new(Some(dir.path().to_path_buf()));
        assert!(grants.known());
        assert!(grants.granted("Xvfb"));
        assert!(!grants.granted("xkbcomp"), "a directory is no grant");
        assert!(!grants.granted("../tools/Xvfb"));
        assert_eq!(
            missing_display_grants(&grants).unwrap(),
            "tool-computer needs a display server: add xkbcomp to capabilities.shell.allow"
        );
        let unknown = Grants::new(None);
        assert!(!unknown.known() && !unknown.granted("Xvfb"));
        assert_eq!(
            missing_display_grants(&Grants::from_names(&[])).unwrap(),
            "tool-computer needs a display server: add Xvfb and xkbcomp to capabilities.shell.allow"
        );
        assert_eq!(
            missing_display_grants(&Grants::from_names(&["Xvfb", "xkbcomp"])),
            None
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
