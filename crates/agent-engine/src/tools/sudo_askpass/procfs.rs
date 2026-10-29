//! The few `/proc` facts the askpass checks need. All of them are readable
//! for any process of the same user, including a set-uid sudo's `status`
//! and `cmdline`; a set-uid sudo's `exe` is not (see the module docs).

use std::io::Read;

/// The fields of `/proc/<pid>/status` the checks use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcStatus {
    pub(crate) name: String,
    pub(crate) ppid: u32,
    /// Real, effective, saved and filesystem uid.
    pub(crate) uids: [u32; 4],
}

/// Ancestry walks stop here; real shells nest a handful of levels deep.
const MAX_DEPTH: usize = 64;

fn read_bounded(path: &str, max: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(max).read_to_end(&mut buf)?;
    Ok(buf)
}

pub(crate) fn status(pid: u32) -> std::io::Result<ProcStatus> {
    let raw = read_bounded(&format!("/proc/{pid}/status"), 16 * 1024)?;
    parse_status(&String::from_utf8_lossy(&raw)).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "unreadable /proc status")
    })
}

fn parse_status(text: &str) -> Option<ProcStatus> {
    let (mut name, mut ppid, mut uids) = (None, None, None);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("PPid:") {
            ppid = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("Uid:") {
            let ids: Vec<u32> = v
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect();
            uids = <[u32; 4]>::try_from(ids).ok();
        }
    }
    Some(ProcStatus {
        name: name?,
        ppid: ppid?,
        uids: uids?,
    })
}

/// argv of `pid`, lossily decoded, read up to 16 KiB.
pub(crate) fn cmdline(pid: u32) -> std::io::Result<Vec<String>> {
    let raw = read_bounded(&format!("/proc/{pid}/cmdline"), 16 * 1024)?;
    let raw = raw.strip_suffix(&[0u8]).unwrap_or(&raw);
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    Ok(raw
        .split(|b| *b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect())
}

/// Metadata of the file `pid` is running. Fails with `PermissionDenied`
/// for a set-uid process of another effective user.
pub(crate) fn exe_metadata(pid: u32) -> std::io::Result<std::fs::Metadata> {
    std::fs::metadata(format!("/proc/{pid}/exe"))
}

/// Is `pid` the process `root` or one of its descendants?
pub(crate) fn descends_from(pid: u32, root: u32) -> bool {
    let mut cur = pid;
    for _ in 0..MAX_DEPTH {
        if cur == root {
            return true;
        }
        if cur <= 1 {
            return false;
        }
        match status(cur) {
            Ok(st) => cur = st.ppid,
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn parses_the_fields_the_checks_use() {
        let text = "Name:\tsudo\nUmask:\t0022\nState:\tS (sleeping)\nPPid:\t42\n\
                    Uid:\t1000\t0\t0\t0\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(
            parse_status(text),
            Some(ProcStatus {
                name: "sudo".into(),
                ppid: 42,
                uids: [1000, 0, 0, 0]
            })
        );
        assert_eq!(parse_status("Name:\tx\nPPid:\t1\n"), None, "no Uid line");
        assert_eq!(
            parse_status("Name:\tx\nPPid:\t1\nUid:\t1 2 3\n"),
            None,
            "short Uid line"
        );
    }

    #[test]
    fn reads_this_process() {
        let me = std::process::id();
        let st = status(me).unwrap();
        // SAFETY: getuid/getppid have no preconditions.
        assert_eq!(st.uids[0], unsafe { libc::getuid() });
        assert_eq!(st.ppid, unsafe { libc::getppid() } as u32);
        assert!(!cmdline(me).unwrap().is_empty());
        assert!(exe_metadata(me).unwrap().is_file());
    }

    /// Wait until `pid` has exec'd the program called `name`.
    pub(crate) fn wait_for_name(pid: u32, name: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while status(pid).map(|s| s.name != name).unwrap_or(true) {
            assert!(
                std::time::Instant::now() < deadline,
                "{pid} never became {name}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn cmdline_keeps_empty_and_spaced_arguments() {
        let script = "import time; time.sleep(30)";
        let mut child = std::process::Command::new("python3")
            .args(["-c", script, "", "a b"])
            .spawn()
            .unwrap();
        wait_for_name(child.id(), "python3");
        let argv = cmdline(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(argv.unwrap(), ["python3", "-c", script, "", "a b"]);
    }

    /// A grandchild descends from the shell that started it; the shell does
    /// not descend from it, and an unrelated process descends from neither.
    #[test]
    fn ancestry_follows_parent_links() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut shell = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "sleep 30 & echo $! > '{}'; wait",
                pidfile.display()
            ))
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let grandchild = loop {
            if let Ok(text) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(std::time::Instant::now() < deadline, "no grandchild pid");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let root = shell.id();
        let verdicts = (
            descends_from(grandchild, root),
            descends_from(root, root),
            descends_from(root, grandchild),
            descends_from(other.id(), root),
            descends_from(std::process::id(), root),
        );
        for child in [&mut shell, &mut other] {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::process::Command::new("kill")
            .arg(grandchild.to_string())
            .status();
        assert_eq!(verdicts, (true, true, false, false, false));
    }
}
