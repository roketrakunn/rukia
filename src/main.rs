use nix::{
    sched::{clone, CloneFlags},
    sys::wait::{waitpid, WaitStatus},
};

// Spawn an isolated container process using Linux namespaces.
// root: path to the rootfs directory (e.g. alpine/)
// cmd:  path to the binary to execute inside the container (e.g. /bin/sh)
fn run_container(root: &str, cmd: &str) {
    // Namespace flags — each one isolates a different view of the system:
    // NEWPID  → container gets its own PID namespace (appears as PID 1)
    // NEWNS   → container gets its own mount namespace (own filesystem view)
    // NEWNET  → container gets its own network stack
    // NEWUTS  → container gets its own hostname
    let flags = CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWNET
        | CloneFlags::CLONE_NEWUTS;

    // The child closure — this is what runs inside the container.
    // chroot + exec will go here next.
    let child_fn = Box::new(|| {
        0
    });

    // Stack for the child process. clone() requires us to allocate this manually
    // since Rust's safety guarantees don't extend across the kernel boundary.
    let mut stack = [0u8; 1024 * 1024]; // 1MB

    unsafe {
        let child_pid = clone(child_fn, &mut stack, flags, None)
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
