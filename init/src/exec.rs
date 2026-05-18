use std::env;
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::mem;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::process;

#[cfg(target_os = "linux")]
use nix::fcntl::{self, OFlag};
#[cfg(target_os = "linux")]
use nix::sys::reboot::{self, RebootMode};
#[cfg(target_os = "linux")]
use nix::sys::stat::Mode;
#[cfg(target_os = "linux")]
use nix::sys::statfs::{self, FsType};
use nix::sys::wait::{self, WaitStatus};
use nix::unistd::{self, ForkResult};

#[cfg(target_os = "linux")]
const KRUN_EXIT_CODE_IOCTL: libc::c_ulong = 0x7602;
#[cfg(target_os = "linux")]
// 0x6573_5546 fits in i32, so the cast to FsType's inner c_long is safe on
// both 32-bit (c_long = i32) and 64-bit (c_long = i64) targets.
const VIRTIOFS_MAGIC: libc::c_long = 0x6573_5546;

#[cfg(target_os = "linux")]
pub fn setup_redirects() {
    let Ok(ports_dir) = fs::read_dir("/sys/class/virtio-ports") else {
        eprintln!("Unable to open virtio-ports directory");
        process::exit(125);
    };
    for entry in ports_dir.flatten() {
        let name_path = entry.path().join("name");
        let Ok(port_name) = fs::read_to_string(&name_path) else {
            continue;
        };
        let (fd, oflag) = match port_name.trim_end_matches('\n') {
            "krun-stdin" => (libc::STDIN_FILENO, OFlag::O_RDONLY),
            "krun-stdout" => (libc::STDOUT_FILENO, OFlag::O_WRONLY),
            "krun-stderr" => (libc::STDERR_FILENO, OFlag::O_WRONLY),
            _ => continue,
        };
        let dev_path = format!("/dev/{}", entry.file_name().to_string_lossy());
        let Ok(new_fd) = fcntl::open(Path::new(&dev_path), oflag, Mode::empty()) else {
            continue;
        };
        if new_fd.as_raw_fd() == fd {
            // Device opened directly onto the target fd (happens when
            // the target was already closed); prevent OwnedFd from closing it.
            mem::forget(new_fd);
            continue;
        }
        let _ = match fd {
            libc::STDIN_FILENO => unistd::dup2_stdin(&new_fd),
            libc::STDOUT_FILENO => unistd::dup2_stdout(&new_fd),
            _ => unistd::dup2_stderr(&new_fd),
        };
    }
}

#[cfg(target_os = "linux")]
pub fn set_exit_code(code: i32) {
    let Ok(fs) = statfs::statfs(Path::new("/")) else {
        return;
    };
    if fs.filesystem_type() != FsType(VIRTIOFS_MAGIC as _) {
        return;
    }
    if let Ok(fd) = fcntl::open(Path::new("/"), OFlag::O_RDONLY, Mode::empty()) {
        unsafe { libc::ioctl(fd.as_raw_fd(), KRUN_EXIT_CODE_IOCTL as _, code) };
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_exit_code(_code: i32) {}

pub fn run_workload(argv: &[String]) -> ! {
    // Match the C init which checked *env_init_pid1 == '1' (first-byte prefix),
    // accepting "1", "10", "1\n", etc.  Exact equality with "1" would reject
    // values that arrive with a trailing newline.
    if env::var("KRUN_INIT_PID1").is_ok_and(|v| v.starts_with('1')) {
        exec_workload(argv);
    }

    match unsafe { unistd::fork() } {
        Err(_) => {
            set_exit_code(125);
            process::exit(125);
        }
        Ok(ForkResult::Child) => exec_workload(argv),
        Ok(ForkResult::Parent { child }) => {
            let code = loop {
                match wait::waitpid(None, None) {
                    Ok(WaitStatus::Exited(pid, c)) if pid == child => break c,
                    Ok(WaitStatus::Signaled(pid, sig, _)) if pid == child => {
                        break sig as i32 + 128;
                    }
                    Err(nix::errno::Errno::ECHILD) => break 125,
                    _ => continue,
                }
            };
            set_exit_code(code);
            unistd::sync();
            #[cfg(target_os = "linux")]
            let _ = reboot::reboot(RebootMode::RB_AUTOBOOT);
            process::exit(code)
        }
    }
}

/// Fast-path exec: copy the workload ELF from virtiofs into a memfd and
/// `fexecve` from there.
///
/// On macOS hosts, Apple Hypervisor Framework charges ~5.5ms per guest page
/// fault.  Mapping a multi-MB ELF straight from the virtiofs DAX window faults
/// in every page (one HVF exit each), adding ~1.9s to agent startup.  Reading
/// the binary once into a guest-RAM-backed memfd turns those into minor faults
/// (no HVF exit), cutting agent load to ~50ms.
///
/// Returns only on failure; the caller then falls back to a plain `execvp`.
#[cfg(target_os = "linux")]
fn exec_via_memfd(c_argv: &[CString]) {
    use nix::sys::memfd::{MFdFlags, memfd_create};
    use std::os::unix::ffi::OsStrExt;

    // Large sequential read — virtiofs FUSE handles this in a few round-trips
    // (each trip = 2 HVF exits), far fewer than per-page DAX fault exits.
    let path = std::ffi::OsStr::from_bytes(c_argv[0].as_bytes());
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return,
    };

    // memfd_create — pages land in guest RAM, not the DAX window.
    let mem_fd = match memfd_create("agent", MFdFlags::MFD_CLOEXEC) {
        Ok(fd) => fd,
        Err(_) => return,
    };

    // Fill the memfd.
    let mut off = 0;
    while off < bytes.len() {
        match unistd::write(&mem_fd, &bytes[off..]) {
            Ok(0) | Err(_) => return,
            Ok(n) => off += n,
        }
    }

    // fexecve maps ELF segments from the memfd: faults hit guest RAM (minor
    // faults, no HVF exits).  Only returns on failure.
    let env: Vec<CString> = env::vars()
        .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
        .collect();
    let _ = unistd::fexecve(&mem_fd, c_argv, &env);
}

fn exec_workload(argv: &[String]) -> ! {
    #[cfg(target_os = "linux")]
    setup_redirects();
    #[cfg(target_os = "freebsd")]
    crate::freebsd::open_console();

    let c_argv: Vec<CString> = argv
        .iter()
        .map(|s| CString::new(s.as_str()).expect("argv contains null byte"))
        .collect();

    // Fast path: exec the workload from a memfd to avoid per-page DAX faults
    // (see `exec_via_memfd`).  Returns only on failure, then we fall back to
    // the plain execvp below.
    #[cfg(target_os = "linux")]
    exec_via_memfd(&c_argv);

    let Err(e) = unistd::execvp(&c_argv[0], &c_argv);
    let code = if e == nix::errno::Errno::ENOENT {
        127
    } else {
        126
    };
    eprintln!("Couldn't execute '{}': {e}", argv[0]);
    process::exit(code);
}
