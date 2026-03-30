use nix::{
    libc::SIGCHLD,
    poll::{poll, PollFd, PollFlags, PollTimeout},
    pty::openpty,
    sched::{clone, CloneFlags},
    sys::{
        termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::{chdir, chroot, dup2, execv, setsid},
};
use std::{ffi::CString, os::fd::{AsRawFd, BorrowedFd}, process::Command};

// Creates the veth pair on the host and assigns the host-side IP.
// veth0 stays on the host, veth1 will be moved into the container.
fn setup_network() {
    Command::new("ip")
        .args(&["link", "add", "veth0", "type", "veth", "peer", "name", "veth1"])
        .status()
        .expect("failed to create veth pair");

    Command::new("ip")
        .args(&["link", "set", "veth0", "up"])
        .status()
        .expect("failed to bring veth0 up");

    Command::new("ip")
        .args(&["addr", "add", "10.0.0.1/24", "dev", "veth0"])
        .status()
        .expect("failed to assign host IP");
}

// Moves veth1 into the container's network namespace (identified by pid),
// then configures it with an IP from within that namespace using nsenter.
fn move_veth_to_container(pid: i32) {
    Command::new("ip")
        .args(&["link", "set", "veth1", "netns", &pid.to_string()])
        .status()
        .expect("failed to move veth1 into container");

    let netns_flag = format!("--net=/proc/{}/ns/net", pid);

    Command::new("nsenter")
        .args(&[&netns_flag, "ip", "link", "set", "veth1", "up"])
        .status()
        .expect("failed to bring veth1 up");

    Command::new("nsenter")
        .args(&[&netns_flag, "ip", "addr", "add", "10.0.0.2/24", "dev", "veth1"])
        .status()
        .expect("failed to assign container IP");
}

// Spawns an isolated container process using Linux namespaces.
// root: path to the rootfs directory (e.g. ./alpine)
// cmd:  path to the binary to execute inside the container (e.g. /bin/sh)
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

    // Set up the host-side veth pair before clone() so the child inherits
    // the network namespace we're about to configure.
    setup_network();

    // Create the pty pair before clone() so both parent and child inherit the fds.
    // master → parent reads/writes here to talk to the shell
    // slave  → child uses this as its stdin/stdout/stderr
    let pty = openpty(None, None).expect("openpty failed");
    let slave_fd = pty.slave.as_raw_fd();

    // The child closure — runs inside the container after clone().
    // Becomes a session leader, wires the slave pty to stdio,
    // chroots into the rootfs, sets PATH, then execs the command.
    let child_fn = Box::new(|| {
        setsid().expect("setsid failed");

        // Attach the slave pty as the controlling terminal for this session,
        // enabling job control (ctrl+c, ctrl+z, etc.)
        unsafe { nix::libc::ioctl(slave_fd, nix::libc::TIOCSCTTY, 0) };

        dup2(slave_fd, 0).expect("dup2 stdin failed");
        dup2(slave_fd, 1).expect("dup2 stdout failed");
        dup2(slave_fd, 2).expect("dup2 stderr failed");

        chroot(root).expect("chroot failed");
        chdir("/").expect("chdir failed");

        // execv requires null-terminated C strings — the kernel speaks C.
        let cmd_cstr = CString::new(cmd).unwrap();
        unsafe { std::env::set_var("PATH", "/bin:/sbin:/usr/bin:/usr/sbin") };
        execv(&cmd_cstr, &[&cmd_cstr]).expect("execv failed");

        unreachable!()
    });

    // The child needs its own stack. We allocate it manually because clone()
    // hands a raw pointer to the kernel — outside Rust's safety guarantees.
    let mut stack = [0u8; 1024 * 1024]; // 1MB

    unsafe {
        // SIGCHLD tells the kernel to notify us when the child exits,
        // so waitpid can reap it properly.
        let child_pid = clone(child_fn, &mut stack, flags, Some(SIGCHLD))
            .expect("clone() failed");

        // Move veth1 into the container's network namespace and configure it.
        move_veth_to_container(child_pid.as_raw());

        let master_fd = BorrowedFd::borrow_raw(pty.master.as_raw_fd());
        let stdin_fd = BorrowedFd::borrow_raw(0);
        let stdout_fd = BorrowedFd::borrow_raw(1);

        let mut fds = [
            PollFd::new(stdin_fd, PollFlags::POLLIN),  // watch our stdin for keystrokes
            PollFd::new(master_fd, PollFlags::POLLIN), // watch master pty for shell output
        ];

        // Put our terminal in raw mode so keystrokes go straight through
        // without line buffering or echo processing.
        let orig = tcgetattr(stdin_fd).expect("tcgetattr failed");
        let mut raw = orig.clone();
        cfmakeraw(&mut raw);
        tcsetattr(stdin_fd, SetArg::TCSANOW, &raw).expect("tcsetattr failed");

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

        // Restore original terminal settings before exiting.
        tcsetattr(stdin_fd, SetArg::TCSANOW, &orig).expect("tcsetattr restore failed");
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
