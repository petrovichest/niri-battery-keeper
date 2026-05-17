use std::collections::HashSet;
use std::fs;

/// Walk the process tree rooted at `root` and return all descendant PIDs
/// (including `root` itself). Reads `/proc/<pid>/task/<tid>/children`, which
/// requires `CONFIG_PROC_CHILDREN=y` in the kernel (default on modern distros).
///
/// Processes that detached and re-parented to PID 1 are not reachable from
/// the original parent and will be missed.
pub fn descendants(root: i32) -> HashSet<i32> {
    let mut visited: HashSet<i32> = HashSet::new();
    visited.insert(root);
    let mut frontier: Vec<i32> = vec![root];
    while let Some(pid) = frontier.pop() {
        let entries = match fs::read_dir(format!("/proc/{pid}/task")) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for task in entries.flatten() {
            let tid = task.file_name();
            let path = format!("/proc/{pid}/task/{}/children", tid.to_string_lossy());
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            for child_str in text.split_whitespace() {
                if let Ok(child) = child_str.parse::<i32>() {
                    if visited.insert(child) {
                        frontier.push(child);
                    }
                }
            }
        }
    }
    visited
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendants_includes_self() {
        let me = std::process::id() as i32;
        let set = descendants(me);
        assert!(set.contains(&me));
    }
}
