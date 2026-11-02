use super::*;

fn line(comm: &str, state: char, start: u64) -> String {
    // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt
    // majflt cmajflt utime stime cutime cstime priority nice threads
    // itrealvalue starttime vsize ...
    format!("42 ({comm}) {state} 1 42 42 0 -1 4194560 10 0 0 0 1 2 0 0 20 0 1 0 {start} 1000 5")
}

fn stat_line(comm: &str, ppid: u32, pgid: u32, sid: u32, start: u64) -> String {
    format!(
        "4242 ({comm}) S {ppid} {pgid} {sid} 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 {start} 16633856 1726 18446744073709551615"
    )
}

fn peer_line() -> String {
    let tail: Vec<String> = (5..=21).map(|n| n.to_string()).collect();
    format!("1234 (a (b) c) S 77 {} 9999 0 0\n", tail.join(" "))
}

fn linux(pid: u32, state: char, ppid: u32, pgid: u32, sid: u32, start_ticks: u64) -> ProcessIdentity {
    ProcessIdentity { pid, state, ppid, pgid, sid, start: StartKey::Linux { start_ticks } }
}

/// Every line the three earlier parsers were tested with, and what the one
/// parser makes of each: the comm can hold spaces, `(` and `)`, so the
/// fields count from the last `)`.
fn pinned_lines() -> Vec<(String, Option<ProcessIdentity>)> {
    vec![
        (line("sh", 'S', 12345), Some(linux(7, 'S', 1, 42, 42, 12345))),
        (line("a) Z 9 (b", 'R', 777), Some(linux(7, 'R', 1, 42, 42, 777))),
        (stat_line("a b) c) S 9 9 9", 77, 88, 99, 26981644), Some(linux(7, 'S', 77, 88, 99, 26981644))),
        (stat_line("cat", 1, 2, 3, 4), Some(linux(7, 'S', 1, 2, 3, 4))),
        (stat_line("sh", 1, 2, 3, 4).replacen(") S ", ") Z ", 1), Some(linux(7, 'Z', 1, 2, 3, 4))),
        (stat_line("", 1, 2, 3, 4), Some(linux(7, 'S', 1, 2, 3, 4))),
        (stat_line(") ) )", 5, 6, 7, 8), Some(linux(7, 'S', 5, 6, 7, 8))),
        (peer_line(), Some(linux(7, 'S', 77, 5, 6, 9999))),
        ("42 (sh) S 1 2".into(), None),
        ("no paren at all".into(), None),
        ("4242 (cat) S 1 2 3".into(), None),
        ("4242 cat S 1 2 3 0 -1".into(), None),
        (stat_line("cat", 1, 2, 3, 4).replace(" 4 1663", " x 1663"), None),
        ("1234 (truncated".into(), None),
        ("1234 (x) S 77".into(), None),
        (String::new(), None),
        (")".into(), None),
    ]
}

#[test]
fn the_stat_parser_keeps_every_pinned_edge_case() {
    for (text, want) in pinned_lines() {
        assert_eq!(linux::parse_stat(7, &text), want, "{text:?}");
    }
}

/// Lines the kernel never prints (a ppid or pgid that is no number) are
/// refused, as the ledger's parser refused them.
#[test]
fn a_line_the_kernel_never_prints_is_refused() {
    let bad_pgid = "1 (x) S 1 x 1 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 5 1 1";
    let bad_ppid = "1 (x) S y 1 1 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 5 1 1";
    for text in [bad_pgid, bad_ppid] {
        assert_eq!(linux::parse_stat(1, text), None, "{text}");
    }
}

// Moved from proc_identity.rs (before the module), with their assertions.

#[test]
fn parses_state_and_start_ticks_after_the_last_paren() {
    let read = |text: &str| linux::parse_stat(42, text).map(|p| (p.state, p.start_ticks().unwrap()));
    assert_eq!(read(&line("sh", 'S', 12345)), Some(('S', 12345)));
    let tricky = line("a) Z 9 (b", 'R', 777);
    assert_eq!(read(&tricky), Some(('R', 777)));
    assert_eq!(read("42 (sh) S 1 2"), None, "too short");
    assert_eq!(read("no paren at all"), None);
}

// Moved from session/children.rs, with their assertions.

#[test]
fn stat_fields_come_from_after_the_last_paren() {
    let read = |text: &str| linux::parse_stat(4242, text);
    let line = stat_line("a b) c) S 9 9 9", 77, 88, 99, 26981644);
    assert_eq!(read(&line), Some(linux(4242, 'S', 77, 88, 99, 26981644)));
    assert_eq!(read(&stat_line("cat", 1, 2, 3, 4)), Some(linux(4242, 'S', 1, 2, 3, 4)));
    let zombie = stat_line("sh", 1, 2, 3, 4).replacen(") S ", ") Z ", 1);
    assert_eq!(read(&zombie).map(|s| (s.state, s.is_live())), Some(('Z', false)));
    assert!(read(&stat_line("sh", 1, 2, 3, 4)).unwrap().is_live());
}

#[test]
fn a_short_or_broken_stat_line_is_none() {
    let read = |text: &str| linux::parse_stat(4242, text);
    assert_eq!(read("4242 (cat) S 1 2 3"), None);
    assert_eq!(read("4242 cat S 1 2 3 0 -1"), None);
    assert_eq!(read(&stat_line("cat", 1, 2, 3, 4).replace(" 4 1663", " x 1663")), None);
    assert_eq!(ProcessIdentity::identify(u32::MAX - 7).ok(), None, "no such pid");
}

// Moved from events/peer.rs, with its assertions.

#[test]
fn stat_fields_are_counted_after_the_last_parenthesis() {
    let read = |text: &str| linux::parse_stat(1234, text).map(|p| (p.ppid, p.start_ticks().unwrap()));
    assert_eq!(read(&peer_line()), Some((77, 9999)));
    assert_eq!(read("1234 (truncated"), None);
    assert_eq!(read("1234 (x) S 77"), None);
}

#[test]
fn zombie_and_dead_states_count_as_exited() {
    for (state, exited) in [('Z', true), ('X', true), ('x', true), ('S', false), ('R', false), ('D', false)] {
        let p = ProcessIdentity { state, ..linux(1, 'S', 0, 1, 1, 1) };
        assert_eq!(p.exited(), exited, "{state}");
        assert_eq!(p.is_live(), !exited, "{state}");
    }
}

/// Keys compare within their platform only; the instance key is the boot
/// plus the start.
#[test]
fn keys_compare_per_platform() {
    let a = linux(10, 'S', 1, 10, 10, 5);
    assert!(a.same_instance(&ProcessIdentity { state: 'Z', ppid: 1, ..a }));
    assert!(!a.same_instance(&linux(10, 'S', 1, 10, 10, 6)), "another start");
    assert!(!a.same_instance(&linux(11, 'S', 1, 10, 10, 5)), "another pid");
    let darwin = StartKey::Darwin { unique_id: 5 };
    assert_ne!(a.start, darwin);
    assert_eq!(ProcessIdentity { start: darwin, ..a }.start_ticks(), None);
    assert_eq!(a.start_ticks(), Some(5));

    let boot = BootKey::Linux { boot_id: "b".into() };
    let key = InstanceKey::new(&boot, a.start).unwrap();
    assert_eq!(key, InstanceKey::Linux { boot_id: "b".into(), start_ticks: 5 });
    assert_eq!((key.boot(), key.start()), (boot.clone(), Some(a.start)));
    assert_ne!(key, InstanceKey::new(&BootKey::Linux { boot_id: "c".into() }, a.start).unwrap(), "another boot");
    assert_eq!(InstanceKey::new(&boot, darwin), None, "platforms never mix");
    let session = BootKey::Darwin { boot_session: "s".into() };
    let mac = InstanceKey::new(&session, StartKey::Darwin { unique_id: 9 }).unwrap();
    assert_eq!(mac, InstanceKey::Darwin { boot_session: "s".into(), unique_id: 9 });
    assert_eq!((mac.boot(), mac.start()), (session.clone(), Some(StartKey::Darwin { unique_id: 9 })));
    assert_ne!(mac, InstanceKey::Darwin { boot_session: "t".into(), unique_id: 9 }, "another boot session");
    assert_ne!(mac, InstanceKey::Darwin { boot_session: "s".into(), unique_id: 10 }, "another unique id");
    assert_eq!((boot.as_str(), session.as_str()), ("b", "s"));
}

/// A start key as the one number older shapes carry under `start_ticks`:
/// the Linux start ticks or the Darwin unique id, read back as this
/// platform's kind.
#[test]
fn a_start_key_is_one_number_read_back_per_platform() {
    assert_eq!(StartKey::Linux { start_ticks: 26981644 }.number(), 26981644);
    assert_eq!(StartKey::Darwin { unique_id: 4_000_000_123 }.number(), 4_000_000_123);
    let back = StartKey::from_number(77);
    if cfg!(target_os = "macos") {
        assert_eq!(back, StartKey::Darwin { unique_id: 77 });
    } else {
        assert_eq!(back, StartKey::Linux { start_ticks: 77 });
    }
    assert_eq!(back.number(), 77);
}

/// The tagged keys as text, exactly: the known kinds as before, and a kind
/// this build doesn't know decodes (its other fields dropped) to `Unknown`,
/// which matches nothing, itself included.
#[test]
fn an_unknown_kind_decodes_and_never_matches() {
    let linux = InstanceKey::Linux { boot_id: "b".into(), start_ticks: 5 };
    let text = r#"{"kind":"linux","boot_id":"b","start_ticks":5}"#;
    assert_eq!(serde_json::to_string(&linux).unwrap(), text);
    assert_eq!(serde_json::from_str::<InstanceKey>(text).unwrap(), linux);
    let host = BootKey::Linux { boot_id: "b".into() };
    assert_eq!(serde_json::to_string(&host).unwrap(), r#"{"kind":"linux","boot_id":"b"}"#);
    assert_eq!(serde_json::from_str::<BootKey>(r#"{"kind":"linux","boot_id":"b"}"#).unwrap(), host);

    let future: InstanceKey =
        serde_json::from_str(r#"{"kind":"future","boot_epoch":"b","serial":5,"nested":{"a":[1]}}"#).unwrap();
    assert!(matches!(future, InstanceKey::Unknown), "{future:?}");
    assert_ne!(future, future.clone(), "not even itself");
    assert_ne!(future, linux);
    assert_ne!(linux, future);
    assert_eq!((future.start(), future.boot().as_str()), (None, ""));
    assert!(matches!(future.boot(), BootKey::Unknown));
    assert_eq!(serde_json::to_string(&future).unwrap(), r#"{"kind":"unknown"}"#);
    let back: InstanceKey = serde_json::from_str(r#"{"kind":"unknown"}"#).unwrap();
    assert!(matches!(back, InstanceKey::Unknown));
    assert_ne!(back, future);

    let boot: BootKey = serde_json::from_str(r#"{"kind":"future","boot_epoch":"b"}"#).unwrap();
    assert!(matches!(boot, BootKey::Unknown), "{boot:?}");
    assert_ne!(boot, boot.clone(), "not even itself");
    assert_ne!(boot, host);
    assert_eq!(InstanceKey::new(&boot, StartKey::Linux { start_ticks: 5 }), None);
    assert_eq!(InstanceKey::new(&boot, StartKey::Darwin { unique_id: 5 }), None);
    assert!(serde_json::from_str::<InstanceKey>(r#"{"boot_id":"b","start_ticks":5}"#).is_err(), "no kind");
}

#[test]
fn the_platform_answers_as_before() {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        return;
    }
    let error = ProcessIdentity::identify(std::process::id()).unwrap_err();
    assert_eq!(
        (error.kind(), error.to_string()),
        (io::ErrorKind::Unsupported, "process identity needs Linux or macOS".into())
    );
    assert_eq!(ProcessIdentity::exit_of(1, None).unwrap_err().kind(), io::ErrorKind::Unsupported);
    assert!(ProcessIdentity::current().is_err() && ProcessIdentity::parent(1).is_err());
    assert!(ProcessIdentity::all().is_empty());
    assert!(!ProcessIdentity::supported() && ProcessIdentity::boot_key().is_none());
    assert!(ProcessIdentity::clk_tck() > 0);
}

/// The one reader reads this process as the kernel prints it.
#[cfg(target_os = "linux")]
#[test]
fn identify_reads_this_process_as_the_kernel_prints_it() {
    let me = std::process::id();
    let id = ProcessIdentity::identify(me).expect("own identity");
    let text = std::fs::read_to_string("/proc/self/stat").unwrap();
    let after = &text[text.rfind(')').unwrap() + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    assert_eq!(id.pid, me);
    assert_eq!(id.start_ticks().unwrap().to_string(), fields[19], "field 22");
    assert_eq!(id.sid.to_string(), fields[3], "field 6");
    assert_eq!(id.pgid.to_string(), fields[2], "field 5");
    assert_eq!(id.ppid, std::os::unix::process::parent_id(), "field 4");
    assert!(id.is_live());
    let current = ProcessIdentity::current().unwrap();
    assert!(current.same_instance(&id));
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    assert_eq!(ProcessIdentity::boot_key().map(BootKey::as_str), Some(boot.trim()));
    assert!(ProcessIdentity::supported());
    assert!(ProcessIdentity::clk_tck() > 0);
    assert_eq!(
        id.instance(),
        Some(InstanceKey::Linux { boot_id: boot.trim().into(), start_ticks: id.start_ticks().unwrap() })
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_parent_and_the_walk_name_live_processes() {
    let me = std::process::id();
    let parent = ProcessIdentity::parent(me).unwrap().expect("a parent");
    assert_eq!(parent.pid, std::os::unix::process::parent_id());
    assert_eq!(Some(parent.start), ProcessIdentity::identify(parent.pid).ok().map(|p| p.start));
    let all = ProcessIdentity::all();
    assert!(all.iter().any(|p| p.pid == me && p.start == ProcessIdentity::identify(me).unwrap().start));
    assert!(all.iter().any(|p| p.pid == parent.pid));
    assert_eq!(ProcessIdentity::identify(u32::MAX - 7).unwrap_err().kind(), io::ErrorKind::NotFound);
}

#[cfg(target_os = "linux")]
#[test]
fn the_own_process_runs_and_a_wrong_start_time_reads_reused() {
    let me = std::process::id();
    let now = ProcessIdentity::identify(me).expect("own stat");
    assert!(!now.exited());
    let ticks = |start_ticks| Some(StartKey::Linux { start_ticks });
    assert_eq!(ProcessIdentity::exit_of(me, Some(now.start)).unwrap(), None);
    assert_eq!(ProcessIdentity::exit_of(me, None).unwrap(), None);
    assert_eq!(ProcessIdentity::exit_of(me, ticks(now.start_ticks().unwrap() + 1)).unwrap(), Some(Exit::Reused));
    assert!(ProcessIdentity::boot_key().is_some_and(|b| b.as_str().len() >= 32));
}

#[cfg(target_os = "linux")]
#[test]
fn a_reaped_child_is_gone_and_an_unreaped_one_a_zombie() {
    let mut child = std::process::Command::new("/bin/true").spawn().expect("spawn");
    let pid = child.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while ProcessIdentity::exit_of(pid, None).unwrap() != Some(Exit::Zombie) {
        assert!(std::time::Instant::now() < deadline, "never a zombie");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(matches!(ProcessIdentity::pidfd(pid, None).unwrap(), Err(Exit::Zombie)), "zombie at entry");
    child.wait().unwrap();
    assert_eq!(ProcessIdentity::exit_of(pid, None).unwrap(), Some(Exit::Gone));
    assert!(matches!(ProcessIdentity::pidfd(pid, None).unwrap(), Err(Exit::Gone)));
}

/// macOS, live: this process, a child, its parent link, the walk, and the
/// exit answers, as the kernel gives them (proc_pidinfo flavor 18).
#[cfg(target_os = "macos")]
#[test]
fn the_darwin_reader_names_this_process_and_a_child() {
    assert!(ProcessIdentity::supported(), "the boot session and a 192-byte self read");
    let boot = ProcessIdentity::boot_key().expect("the boot session");
    assert!(matches!(boot, BootKey::Darwin { boot_session } if !boot_session.is_empty()), "{boot:?}");
    let me = std::process::id();
    let id = ProcessIdentity::identify(me).expect("own identity");
    println!("self: {id:?} in {boot:?}");
    assert!(matches!(id.start, StartKey::Darwin { unique_id } if unique_id > 0), "{id:?}");
    assert_eq!(id.pid, me);
    assert_eq!(id.ppid, std::os::unix::process::parent_id());
    // SAFETY: getsid(0) and getpgrp() read this process's own ids.
    let (sid, pgid) = unsafe { (libc::getsid(0), libc::getpgrp()) };
    assert_eq!((id.sid, id.pgid), (sid as u32, pgid as u32));
    assert!(id.is_live() && id.start_ticks().is_none());
    assert!(ProcessIdentity::current().unwrap().same_instance(&id));
    assert_eq!(id.instance(), InstanceKey::new(boot, id.start));
    assert_eq!(id.instance().map(|i| i.boot()), Some(boot.clone()));

    let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().expect("spawn");
    let pid = child.id();
    let now = ProcessIdentity::identify(pid).unwrap();
    assert_eq!(now.ppid, me);
    assert_ne!(now.start, id.start);
    assert_eq!(ProcessIdentity::parent(pid).unwrap(), Some(Parent { pid: me, start: id.start }), "its parent is me");
    assert!(ProcessIdentity::all().iter().any(|p| p.same_instance(&now)), "the walk lists it");
    assert_eq!(ProcessIdentity::exit_of(pid, Some(now.start)).unwrap(), None);
    let StartKey::Darwin { unique_id } = now.start else { unreachable!() };
    let other = StartKey::Darwin { unique_id: unique_id + 1 };
    assert_eq!(ProcessIdentity::exit_of(pid, Some(other)).unwrap(), Some(Exit::Reused));
    assert_eq!(ProcessIdentity::exit_of(pid, Some(StartKey::Linux { start_ticks: unique_id })).unwrap(), Some(Exit::Reused));
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(ProcessIdentity::exit_of(pid, Some(now.start)).unwrap(), Some(Exit::Gone));
    assert_eq!(ProcessIdentity::identify(u32::MAX - 7).unwrap_err().kind(), io::ErrorKind::NotFound);

    // Exited and not reaped: read with arg 0, so gone, not a zombie.
    let mut done = std::process::Command::new("/usr/bin/true").spawn().expect("spawn true");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let unreaped = ProcessIdentity::exit_of(done.id(), None);
    println!("an exited, unreaped child reads {unreaped:?}");
    assert!(matches!(unreaped, Ok(Some(Exit::Gone | Exit::Zombie))), "{unreaped:?}");
    done.wait().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn a_pidfd_opens_for_a_running_instance_only() {
    let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().expect("spawn");
    let pid = child.id();
    let now = ProcessIdentity::identify(pid).unwrap();
    assert!(ProcessIdentity::pidfd(pid, Some(now.start)).unwrap().is_ok());
    let other = StartKey::Linux { start_ticks: now.start_ticks().unwrap() + 1 };
    assert!(matches!(ProcessIdentity::pidfd(pid, Some(other)).unwrap(), Err(Exit::Reused)));
    child.kill().unwrap();
    child.wait().unwrap();
}
