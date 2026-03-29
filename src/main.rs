use nix::{
    libc::SIGCHLD,
    sched::{clone, CloneFlags},
    sys::wait::{waitpid, WaitStatus},
    unistd::{chdir, chroot, execv},
};
use std::ffi::CString;

// Spawn an isolated container process using Linux namespaces.
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

    // The child closure — runs inside the container after clone().
    // chroot jails the process into the rootfs, then exec replaces it with cmd.
    let child_fn = Box::new(|| {
        chroot(root).expect("chroot failed");
        chdir("/").expect("chdir failed");

        // execv requires null-terminated C strings — the kernel speaks C.
        let cmd_cstr = CString::new(cmd).unwrap();
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

        match waitpid(child_pid, None) {
            Ok(WaitStatus::Exited(pid, code)) => {
                println!("container [pid {}] exited with code {}", pid, code);
            }
            Ok(status) => {
                println!("container exited with status: {:?}", status);
            }
            Err(e) => {
                eprintln!("waitpid failed: {}", e);
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
