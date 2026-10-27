//! RStudio's memory indicator: what the R session and the system use. It's
//! read from the operating system rather than asked of R, so it keeps
//! updating while R is busy.

use collections::HashMap;
use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, RefreshKind, System};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryUsage {
    /// Bytes resident in the R session's processes.
    pub session: u64,
    pub system_used: u64,
    pub system_total: u64,
}

pub struct MemorySampler {
    system: System,
}

impl MemorySampler {
    pub fn new() -> Self {
        Self {
            system: System::new(),
        }
    }

    /// The memory of process `root` and every process under it, which for R
    /// is the shell supervising Ark, Ark itself, and processes R started.
    /// This refreshes every process, so it blocks and belongs on a background
    /// thread.
    pub fn sample(&mut self, root: u32) -> MemoryUsage {
        self.system.refresh_specifics(
            RefreshKind::nothing()
                .with_processes(ProcessRefreshKind::nothing().without_tasks().with_memory())
                .with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        let processes = self.system.processes();
        let parents: HashMap<Pid, Pid> = processes
            .iter()
            .filter_map(|(&pid, process)| Some((pid, process.parent()?)))
            .collect();
        let root = Pid::from_u32(root);
        let session = processes
            .iter()
            .filter(|(pid, _)| is_descendant(**pid, root, &parents))
            .map(|(_, process)| process.memory())
            .sum();
        MemoryUsage {
            session,
            system_used: self.system.used_memory(),
            system_total: self.system.total_memory(),
        }
    }
}

/// Whether `pid` is `root` or below it.
fn is_descendant(mut pid: Pid, root: Pid, parents: &HashMap<Pid, Pid>) -> bool {
    // Bounded, since a process table read mid-change could hold a cycle.
    for _ in 0..64 {
        if pid == root {
            return true;
        }
        match parents.get(&pid) {
            Some(&parent) => pid = parent,
            None => return false,
        }
    }
    false
}

/// Bytes the way the pane shows sizes.
pub fn format_memory(bytes: u64) -> String {
    crate::tree::format_size(i64::try_from(bytes).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    #[test]
    fn test_session_memory_includes_child_processes() {
        // A shell standing in for Ark's supervisor, with a child below it.
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & wait"]);
        let mut shell =
            util::process::Child::spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
                .unwrap();
        let mut sampler = MemorySampler::new();
        let started = Instant::now();
        let mut found_child = false;
        while started.elapsed() < Duration::from_secs(10) {
            let usage = sampler.sample(shell.id());
            let root = Pid::from_u32(shell.id());
            found_child = sampler
                .system
                .processes()
                .values()
                .any(|process| process.parent() == Some(root));
            if found_child {
                assert!(usage.session > 0);
                assert!(usage.system_total >= usage.system_used);
                assert!(usage.system_total > 0);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        shell.kill().unwrap();
        assert!(found_child, "the shell's child never appeared");
    }

    #[test]
    fn test_is_descendant() {
        let pid = Pid::from_u32;
        let parents: HashMap<Pid, Pid> = [(pid(3), pid(2)), (pid(2), pid(1)), (pid(5), pid(4))]
            .into_iter()
            .collect();
        assert!(is_descendant(pid(3), pid(1), &parents));
        assert!(is_descendant(pid(1), pid(1), &parents));
        assert!(!is_descendant(pid(5), pid(1), &parents));

        let cycle: HashMap<Pid, Pid> = [(pid(1), pid(2)), (pid(2), pid(1))].into_iter().collect();
        assert!(!is_descendant(pid(1), pid(3), &cycle));
    }

    #[test]
    fn test_format_memory() {
        assert_eq!(format_memory(152 * 1024 * 1024), "152.0 MB");
    }
}
