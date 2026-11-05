//! `clone`'s check for an existing destination, the one place besides the code-graph database
//! and `log_path` where the tool itself looks at the filesystem.

use murmur_host_roots::{locate, Located};

use crate::env::Git;

/// Whether `dest` already exists — a file, or a folder empty or not — where the component can
/// look: a relative value in the work folder, an absolute one through the held host folder
/// covering it. A value it cannot look at (an absolute path no held folder covers, or a relative
/// one starting with `murmur-host` while a host folder is held) counts as absent, and git's own
/// check decides.
pub fn dest_exists(git: &Git, dest: &str) -> bool {
    let env = git.env();
    match locate(dest, env.roots()) {
        Ok(Located::Relative(open)) | Ok(Located::Host { open, .. }) => env.fs_path(&open).exists(),
        Err(_) => false,
    }
}
