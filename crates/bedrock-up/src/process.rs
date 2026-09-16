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
///
/// Excludes the calling process itself: a supervisor binary staged inside
/// its own server directory would otherwise always find itself and refuse
/// to ever start the server it's supervising.
pub fn find_server_processes(server_path: &Path) -> Vec<ServerProcess> {
    let Ok(server_path) = server_path.canonicalize() else {
        return Vec::new();
    };

    let own_pid = std::process::id();

    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let mut found: Vec<ServerProcess> = system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            if pid.as_u32() == own_pid {
                return None;
            }
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
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_find_server_processes_excludes_self() {
        // The test binary is itself a process running out of its own directory.
        // A supervisor staged inside its own server directory must not find
        // itself and mistake that for an already-running server.
        let current_exe = std::env::current_exe().unwrap();
        let exe_dir = current_exe.parent().unwrap();

        let found = find_server_processes(exe_dir);

        let this_pid = std::process::id();
        assert!(
            !found.iter().any(|process| process.pid == this_pid),
            "expected the calling process {} to be excluded from its own search under {}",
            this_pid,
            exe_dir.display()
        );
    }

    #[test]
    fn test_find_server_processes_finds_other_process_under_path() {
        let mut child = if cfg!(windows) {
            std::process::Command::new("ping")
                .args(["127.0.0.1", "-n", "3"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        } else {
            std::process::Command::new("sleep")
                .arg("2")
                .spawn()
                .unwrap()
        };
        let child_pid = child.id();

        let mut system = sysinfo::System::new();
        let mut exe_path = None;
        for _ in 0..20 {
            system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            if let Some(exe) = system
                .process(sysinfo::Pid::from_u32(child_pid))
                .and_then(|process| process.exe())
            {
                exe_path = Some(exe.to_path_buf());
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let exe_path = exe_path.expect("child process exe path should resolve");
        let exe_dir = exe_path.parent().unwrap();

        let found = find_server_processes(exe_dir);

        assert!(
            found.iter().any(|process| process.pid == child_pid),
            "expected to find the spawned child process under {}",
            exe_dir.display()
        );

        let _ = child.kill();
        let _ = child.wait();
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
