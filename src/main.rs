//! hax-herdr: runs hax and reports its status to Herdr as OSC 7501 (Program Status Protocol).
//!
//! hax has no hooks, so the status comes from its session log, a documented append-only JSONL
//! file (hax `docs/sessions.md`): a turn's `turn_boundary` (with the prompt, if one was typed)
//! is written before the provider call, and `type: user_turn` when the turn ends. This process
//! becomes hax (same PID, so Herdr sees hax and gets its exit code); a forked watcher finds the
//! log hax has open, follows it and writes the reports to the terminal. Outside Herdr, or when
//! stdout isn't a terminal, it only runs hax.

use std::ffi::OsString;
use std::fs::File;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::unix::process::{CommandExt, parent_id};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

const TICK: Duration = Duration::from_millis(250);

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if std::env::var_os("HC_TERMINAL_ID").is_some() && std::io::stdout().is_terminal() {
        let started = SystemTime::now();
        // Before hax draws anything, so this report can't split one of its escape sequences.
        report("idle");
        let hax = std::process::id();
        // SAFETY: single-threaded here; the child only runs this crate's code before exiting.
        if unsafe { libc::fork() } == 0 {
            watch(hax, started);
            // SAFETY: leaves without running the parent's atexit handlers or flushing its stdio.
            unsafe { libc::_exit(0) }
        }
    }
    let err = Command::new("hax").args(&args).exec();
    eprintln!("hax-herdr: cannot run hax: {err}");
    std::process::exit(127);
}

/// Follows hax's session log until hax exits (this process is then reparented).
fn watch(hax: u32, started: SystemTime) {
    // Ctrl-C and Ctrl-\ reach the whole foreground group; they are meant for hax.
    // SAFETY: setting a disposition to SIG_IGN has no handler to race with.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
    let mut log: Option<Log> = None;
    let mut turn = Turn::default();
    let mut last = "idle";
    while parent_id() == hax {
        let open = open_files(hax).into_iter().find(|p| is_session_log(p));
        if open.as_deref() != log.as_ref().map(|l| l.path.as_path()) {
            log = open.and_then(|p| Log::open(p, started));
        }
        let mut states = Vec::new();
        if let Some(log) = &mut log {
            log.each_line(|line| {
                let record = serde_json::from_slice(line).ok();
                states.extend(record.and_then(|r| turn.apply(&r)));
            });
        }
        states.extend(turn.announce());
        for state in states {
            if state != last {
                report(state);
                last = state;
            }
        }
        std::thread::sleep(TICK);
    }
}

/// One OSC 7501 report as a single write, so it can't interleave with hax's own output.
fn report(state: &str) {
    // SAFETY: fd 1 stays open for the process's life; ManuallyDrop keeps it from being closed.
    let mut out = ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
    let _ = out.write_all(format!("\x1b]7501;state={state}:app=hax\x1b\\").as_bytes());
}

/// `<state>/hax/sessions/<encoded-cwd>/<timestamp>_<uuid>.jsonl`
fn is_session_log(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
        && path
            .parent()
            .and_then(Path::parent)
            .is_some_and(|p| p.ends_with("hax/sessions"))
}

/// The fields of a session record that mark turns; the rest is skipped unparsed.
#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    ty: Option<String>,
    kind: Option<String>,
    origin: Option<String>,
    #[serde(default)]
    inherited: bool,
}

#[derive(Default)]
struct Turn {
    /// A turn is open in the log.
    working: bool,
    /// ... and has been reported as `working`.
    announced: bool,
    /// ... and was cut short by Esc.
    interrupted: bool,
}

impl Turn {
    /// The status a record moves to, if it changes one.
    fn apply(&mut self, r: &Record) -> Option<&'static str> {
        if r.inherited {
            return None; // copied by /fork from another session
        }
        match (r.ty.as_deref(), r.kind.as_deref(), r.origin.as_deref()) {
            // Every turn opens with a boundary: a typed prompt, a continuation after Esc Esc, or
            // Enter at a paused prompt (a boundary alone).
            (_, Some("turn_boundary"), _) if !self.working => {
                *self = Turn {
                    working: true,
                    ..Turn::default()
                };
                None
            }
            // A compaction writes a boundary too, with its summary right behind it: not a turn.
            (_, Some("user"), Some("compact_seed")) => {
                std::mem::take(self).announced.then_some("idle")
            }
            (Some("user_turn"), _, _) if self.working => {
                // Esc Esc means the person is already here: nothing waits on them.
                Some(if std::mem::take(self).interrupted {
                    "idle"
                } else {
                    "done"
                })
            }
            // Esc's marker, or a tool call it cut off before it ran.
            (_, _, Some("interrupted" | "skipped")) => {
                self.interrupted = self.working;
                self.announce()
            }
            _ => self.announce(),
        }
    }

    /// `working` for an open turn not reported yet: any record after its boundary confirms it,
    /// and so does the end of a read with nothing after it.
    fn announce(&mut self) -> Option<&'static str> {
        (self.working && !self.announced).then(|| {
            self.announced = true;
            "working"
        })
    }
}

struct Log {
    path: PathBuf,
    file: File,
    pending: Vec<u8>,
}

impl Log {
    /// A log hax created after we started is read from the top; an older one (resumed) from its end.
    fn open(path: PathBuf, started: SystemTime) -> Option<Log> {
        let mut file = File::open(&path).ok()?;
        let born = file.metadata().and_then(|m| m.created()).ok();
        if born.is_some_and(|t| t < started) {
            file.seek(SeekFrom::End(0)).ok()?;
        }
        Some(Log {
            path,
            file,
            pending: Vec::new(),
        })
    }

    /// Calls `f` with each complete line appended since the last call.
    fn each_line(&mut self, mut f: impl FnMut(&[u8])) {
        if self.file.read_to_end(&mut self.pending).is_err() {
            return;
        }
        let Some(end) = self.pending.iter().rposition(|&b| b == b'\n') else {
            return;
        };
        self.pending[..end].split(|&b| b == b'\n').for_each(&mut f);
        self.pending.drain(..=end);
    }
}

#[cfg(target_os = "linux")]
fn open_files(pid: u32) -> Vec<PathBuf> {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    fds.flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .collect()
}

/// `struct proc_fileinfo` from <sys/proc_info.h>; libc doesn't define it.
#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcFileInfo {
    openflags: u32,
    status: u32,
    offset: i64,
    kind: i32,
    guardflags: u32,
}

/// `struct vnode_fdinfowithpath`, the `PROC_PIDFDVNODEPATHINFO` answer.
#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: libc::vnode_info_path,
}

#[cfg(target_os = "macos")]
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

#[cfg(target_os = "macos")]
fn open_files(pid: u32) -> Vec<PathBuf> {
    use std::ffi::OsStr;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    let pid = pid as libc::c_int;
    let entry = size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer asks only for the size the list needs.
    let need =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if need <= 0 {
        return Vec::new();
    }
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        need as usize / entry + 8
    ];
    let bytes = (fds.len() * entry) as libc::c_int;
    // SAFETY: the buffer holds `bytes` bytes of proc_fdinfo.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            bytes,
        )
    };
    fds.truncate(got.max(0) as usize / entry);

    let size = size_of::<VnodeFdInfoWithPath>() as libc::c_int;
    fds.iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        .filter_map(|fd| {
            let mut info = MaybeUninit::<VnodeFdInfoWithPath>::zeroed();
            // SAFETY: the buffer is `size` bytes; the kernel fills it only when it answers in full.
            let n = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            if n != size {
                return None;
            }
            // SAFETY: zeroed, then filled by the kernel; plain integers throughout.
            let info = unsafe { info.assume_init() };
            // SAFETY: `[[c_char; 32]; 32]` is MAXPATHLEN bytes, NUL-terminated.
            let path: &[u8; 1024] = unsafe { &*(&raw const info.pvip.vip_path).cast() };
            let len = path.iter().position(|&b| b == 0)?;
            Some(PathBuf::from(OsStr::from_bytes(&path[..len])))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDARY: &str = r#"{"kind":"turn_boundary"}"#;
    const PROMPT: &str = r#"{"kind":"user","text":"hi"}"#;
    const REPLY: &str = r#"{"kind":"assistant","text":"hello"}"#;
    const USAGE: &str = r#"{"kind":"turn_usage","usage":{"input":1}}"#;
    const END: &str = r#"{"type":"user_turn","elapsed_ms":12}"#;

    /// The statuses reported for `reads`, each a batch of lines the watcher reads in one tick.
    fn states(reads: &[&[&str]]) -> Vec<&'static str> {
        let mut turn = Turn::default();
        let mut out = Vec::new();
        for read in reads {
            for line in *read {
                out.extend(turn.apply(&serde_json::from_str(line).unwrap()));
            }
            out.extend(turn.announce());
        }
        out
    }

    #[test]
    fn a_typed_prompt_works_until_its_turn_ends() {
        let header = r#"{"type":"session","cwd":"/x"}"#;
        assert_eq!(
            states(&[&[header, BOUNDARY, PROMPT], &[REPLY, USAGE, END]]),
            ["working", "done"]
        );
        assert_eq!(
            states(&[&[BOUNDARY], &[PROMPT, REPLY, END]]),
            ["working", "done"],
            "split reads"
        );
        assert_eq!(
            states(&[&[BOUNDARY, PROMPT, REPLY, USAGE, END]]),
            ["working", "done"],
            "one read"
        );
        let rounds = [BOUNDARY, PROMPT, REPLY, USAGE, BOUNDARY, REPLY, USAGE, END];
        assert_eq!(
            states(&[&rounds]),
            ["working", "done"],
            "a boundary mid-turn changes nothing"
        );
    }

    #[test]
    fn enter_at_a_paused_prompt_is_a_turn_too() {
        assert_eq!(
            states(&[&[BOUNDARY], &[REPLY, USAGE, END]]),
            ["working", "done"]
        );
    }

    #[test]
    fn esc_esc_ends_the_turn_idle() {
        let marker = r#"{"kind":"assistant","origin":"interrupted"}"#;
        assert_eq!(
            states(&[&[BOUNDARY, PROMPT], &[marker, END]]),
            ["working", "idle"]
        );
        let skipped = r#"{"kind":"tool_result","origin":"skipped"}"#;
        assert_eq!(
            states(&[&[BOUNDARY, PROMPT], &[skipped, END]]),
            ["working", "idle"],
            "a cut-off tool call"
        );
        let continuation = r#"{"kind":"user","origin":"continuation"}"#;
        let again = states(&[
            &[BOUNDARY, PROMPT],
            &[marker, END],
            &[BOUNDARY, continuation],
            &[REPLY, END],
        ]);
        assert_eq!(
            again,
            ["working", "idle", "working", "done"],
            "the next turn starts clean"
        );
    }

    #[test]
    fn a_compaction_is_not_a_turn() {
        let usage = r#"{"kind":"turn_usage","origin":"compaction"}"#;
        let seed = r#"{"kind":"user","origin":"compact_seed"}"#;
        assert!(states(&[&[usage, BOUNDARY, seed]]).is_empty());
        assert_eq!(
            states(&[&[usage, BOUNDARY], &[seed]]),
            ["working", "idle"],
            "split: corrected"
        );
        let after_turn = states(&[&[BOUNDARY, PROMPT], &[REPLY, END, usage, BOUNDARY, seed]]);
        assert_eq!(
            after_turn,
            ["working", "done"],
            "auto-compaction right after a turn"
        );
    }

    #[test]
    fn inherited_and_control_records_start_nothing() {
        let forked = [
            r#"{"kind":"turn_boundary","inherited":true}"#,
            r#"{"kind":"user","inherited":true}"#,
        ];
        assert!(states(&[&forked, &[END, r#"{"type":"undo","keep_user_turns":0}"#]]).is_empty());
    }

    #[test]
    fn only_session_logs_match() {
        assert!(is_session_log(Path::new(
            "/u/.local/state/hax/sessions/proj.ab12/2026-10-30T09-51-46Z_x.jsonl"
        )));
        assert!(!is_session_log(Path::new(
            "/u/.local/state/hax/sessions/proj.ab12/history"
        )));
        assert!(!is_session_log(Path::new("/u/transcript/hax/x.jsonl")));
    }

    #[test]
    fn each_line_holds_back_a_partial_line() {
        let dir = std::env::temp_dir().join(format!("hax-herdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "a\nb").unwrap();
        let mut log = Log::open(path.clone(), SystemTime::UNIX_EPOCH).unwrap();
        let mut seen = Vec::new();
        log.each_line(|l| seen.push(l.to_vec()));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"c\n")
            .unwrap();
        log.each_line(|l| seen.push(l.to_vec()));
        assert_eq!(seen, [b"a".to_vec(), b"bc".to_vec()]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_struct_matches_the_kernel() {
        assert_eq!(
            size_of::<VnodeFdInfoWithPath>(),
            1200,
            "sizeof(struct vnode_fdinfowithpath)"
        );
    }

    #[test]
    fn finds_its_own_open_file() {
        let path = std::env::temp_dir().join(format!("hax-herdr-open-{}", std::process::id()));
        let _file = File::create(&path).unwrap();
        let found = open_files(std::process::id());
        std::fs::remove_file(&path).unwrap();
        let want = path.canonicalize().unwrap_or(path);
        assert!(
            found.iter().any(|p| p.file_name() == want.file_name()),
            "{found:?}"
        );
    }
}
