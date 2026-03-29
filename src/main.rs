use nix::{
    libc::SIGCHLD,
    poll::{poll, PollFd, PollFlags, PollTimeout},
    pty::openpty,
    sched::{clone, CloneFlags},
    sys::wait::{waitpid, WaitPidFlag, WaitStatus},
    unistd::{chdir, chroot, dup2, execv},
};
use std::{ffi::CString, os::fd::{AsRawFd, BorrowedFd}};

// Spawn an isolated container process using Linux namespaces.
// root: path to the rootfs directory (e.g. ./alpine)
// cmd:  path to the binary to execute inside the container (e.g. /bin/sh)
// TODO: put terminal in raw mode to fix escape code leaking from slave buffer.
fn run_container(root: &str, cmd: &str) {
    // Each flag isolates a different view of the system for the child:
    // NEWPID  → own PID namespace (child appears as PID 1)
    // NEWNS   → own mount namespace (own filesystem view)
    // NEWNET  → own network stack
    // NEWUTS  → own hostname
    let flags = CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWNET
        | CloneFlags::CLONE_NEWUTS;

    // Create the pty pair before clone() so both parent and child inherit the fds
    // master → parent uses this to read/write to the shell
    // slave → child uses this as its stdin/stdout/stderr
    let pty = openpty(None, None).expect("openpty failed");
    let slave_fd = pty.slave.as_raw_fd();

    // The child closure — runs inside the container after clone().
    // Wires the slave pty to stdio, chroots into the rootfs, then execs the command.
    let child_fn = Box::new(|| {
        dup2(slave_fd, 0).expect("dup2 stdin failed");
        dup2(slave_fd, 1).expect("dup2 stdout failed");
        dup2(slave_fd, 2).expect("dup2 stderr failed");

        chroot(root).expect("chroot failed");
        chdir("/").expect("chdir failed");

        // execv requires null-terminated C strings — the kernel speaks C.
        let cmd_cstr = CString::new(cmd).unwrap();
        execv(&cmd_cstr, &[&cmd_cstr]).expect("execv failed");

        unreachable!()
    });

    // The child needs its own stack. We allocate it manually because clone()
    // hands a raw pointer to the kernel — outside Rust's safety guarantees
    let mut stack = [0u8; 1024 * 1024]; // 1MB

    unsafe {
        // SIGCHLD tells the kernel to notify us when the child exits,
        // so waitpid can reap it properly.
        let child_pid = clone(child_fn, &mut stack, flags, Some(SIGCHLD))
            .expect("clone() failed");

        let master_fd = BorrowedFd::borrow_raw(pty.master.as_raw_fd());
        let stdin_fd = BorrowedFd::borrow_raw(0);
        let stdout_fd = BorrowedFd::borrow_raw(1);

        let mut fds = [
            PollFd::new(stdin_fd, PollFlags::POLLIN),  // watch our stdin for keystrokes
            PollFd::new(master_fd, PollFlags::POLLIN), // watch master pty for shell output
        ];

        loop {
            // Check if the child exited before polling again
            match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) => {
                    println!("container [pid {}] exited with code {}", pid, code);
                    break;
                }
                Ok(WaitStatus::StillAlive) => {}
                Ok(status) => {
                    println!("container exited with status: {:?}", status);
                    break;
                }
                Err(e) => {
                    eprintln!("waitpid failed: {}", e);
                    break;
                }
            }

            poll(&mut fds, PollTimeout::NONE).expect("poll failed");

            // Keystroke from our terminal → forward to the shell via master
            if let Some(revents) = fds[0].revents() {
                if revents.contains(PollFlags::POLLIN) {
                    let mut buf = [0u8; 1024];
                    let n = nix::unistd::read(stdin_fd.as_raw_fd(), &mut buf)
                        .expect("read stdin failed");
                    nix::unistd::write(master_fd, &buf[..n]).expect("write master failed");
                }
            }

            // Shell output from master → forward to our terminal's stdout
            if let Some(revents) = fds[1].revents() {
                if revents.contains(PollFlags::POLLIN) {
                    let mut buf = [0u8; 1024];
                    let n = nix::unistd::read(master_fd.as_raw_fd(), &mut buf)
                        .expect("read master failed");
                    nix::unistd::write(stdout_fd, &buf[..n]).expect("write stdout failed");
                } else if revents.contains(PollFlags::POLLHUP) {
                    // Master hung up — shell has exited.
                    break;
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 4 {
        eprintln!("usage: rukia run <rootfs> <cmd>");
        eprintln!("example: rukia run ./alpine /bin/sh");
        std::process::exit(1);
    }

    let sub_cmd = &args[1];
    let root = &args[2];
    let cmd = &args[3];

    match sub_cmd.as_str() {
        "run" => run_container(root, cmd),
        _ => {
            eprintln!("unknown subcommand: {}", sub_cmd);
            std::process::exit(1);
        }
    }
}
