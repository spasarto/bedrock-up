//! Identifies which running server belongs to a given server directory.
//!
//! Each instance runs its own `bedrock_server` binary out of its own directory,
//! so the executable path identifies the instance exactly. That matters when
//! several servers share a machine: the directory being updated is enough to
//! tell which process is affected, with no PID guessing.

use std::path::{Path, PathBuf};

pub struct ServerProcess {
    pub pid: u32,
    pub exe: PathBuf,
}

/// Finds the running processes whose executable lives under `server_path`.
pub fn find_server_processes(server_path: &Path) -> Vec<ServerProcess> {
    let Ok(server_path) = server_path.canonicalize() else {
        return Vec::new();
    };

    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let mut found: Vec<ServerProcess> = system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let exe = process.exe()?;
            // A staged binary may no longer resolve, so fall back to the raw path.
            let exe = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
            exe.starts_with(&server_path).then(|| ServerProcess {
                pid: pid.as_u32(),
                exe,
            })
        })
        .collect();

    found.sort_by_key(|process| process.pid);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_server_processes_finds_current_exe() {
        // The test binary is itself a process running out of its own directory,
        // so pointing the search at that directory must find it.
        let current_exe = std::env::current_exe().unwrap();
        let exe_dir = current_exe.parent().unwrap();

        let found = find_server_processes(exe_dir);

        let this_pid = std::process::id();
        assert!(
            found.iter().any(|process| process.pid == this_pid),
            "expected to find the test process {} under {}",
            this_pid,
            exe_dir.display()
        );
    }

    #[test]
    fn test_find_server_processes_ignores_unrelated_directory() {
        let temp_dir = tempfile::TempDir::new().unwrap();

        let found = find_server_processes(temp_dir.path());

        assert!(found.is_empty());
    }

    #[test]
    fn test_find_server_processes_nonexistent_path() {
        let found = find_server_processes(Path::new("/no/such/directory/anywhere"));

        assert!(found.is_empty());
    }
}
