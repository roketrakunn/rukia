# rukia

A container runtime built from scratch in Rust. No Docker, no containerd — just Linux syscalls.

## What it does

Spawns an isolated process with its own:
- **PID namespace** — appears as PID 1, can't see host processes
- **Mount namespace** — chrooted into its own filesystem (Alpine Linux)
- **Network namespace** — own network stack, connected to the host via a virtual ethernet pair
- **UTS namespace** — own hostname

Full internet access via NAT. Interactive shell via pseudo-terminal.

## How it works

```
rukia run ./alpine /bin/sh
```

1. Creates a `veth` pair (`veth0` host, `veth1` container) and sets up NAT via iptables
2. Calls `clone()` with namespace flags to spawn the child in isolation
3. Moves `veth1` into the container's network namespace, assigns IPs, adds default route and DNS
4. Child calls `setsid()` + `TIOCSCTTY` to become session leader with a controlling terminal
5. Child `chroot()`s into the rootfs, then `exec()`s the command
6. Parent runs a `poll()` loop forwarding keystrokes and output between the terminal and the pty master
7. On exit, cleans up the veth pair

## Requirements

- Linux
- Root (`sudo`)
- `iproute2`, `iptables`, `util-linux` (for `nsenter`)
- An Alpine rootfs at `./alpine`

## Getting the rootfs

```bash
mkdir -p alpine
curl -L https://dl-cdn.alpinelinux.org/alpine/v3.19/releases/x86_64/alpine-minirootfs-3.19.1-x86_64.tar.gz \
  | tar -xz -C alpine
```

## Build and run

```bash
cargo build
sudo ./target/debug/rukia run ./alpine /bin/sh
```

## What you get

```
/ # ping google.com
PING google.com (142.251.47.46): 56 data bytes
64 bytes from 142.251.47.46: seq=0 ttl=115 time=20.1 ms
/ # whoami
root
/ # ls /
bin  dev  etc  home  lib  media  mnt  opt  proc  root  run  sbin  srv  sys  tmp  usr  var
```
