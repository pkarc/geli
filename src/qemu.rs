//! The Linux sandbox driver: images, direct kernel boot, the egress proxy and the boot watch.

#[allow(unused_imports)]
use crate::dirs_home_dir;
use crate::{guest::*, net::*};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use std::io::Read;
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where QEMU's console goes. Sessions inherit the terminal so the agent is interactive;
/// image builds are unattended and go to a log we can inspect afterwards.
pub enum QemuIo {
    Interactive,
    LogTo(PathBuf),
}

/// Size of the terminal geli was launched from, if it was launched from one at all.
pub fn host_terminal_size() -> Option<(u16, u16)> {
    let out = Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let rows: u16 = parts.next()?.parse().ok()?;
    let cols: u16 = parts.next()?.parse().ok()?;

    if rows == 0 || cols == 0 {
        return None;
    }
    Some((rows, cols))
}

pub fn images_dir() -> PathBuf {
    dirs_home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("qemu-sandbox")
}

pub fn check_host_tools() {
    let present = |bin: &str| {
        Command::new("which")
            .arg(bin)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };

    if !present("qemu-system-x86_64") || !present("genisoimage") {
        eprintln!("\n[!] Error: Missing required system virtualization utilities.");
        eprintln!("geli requires QEMU and genisoimage to create secure hardware sandboxes.");
        eprintln!("\nPlease install them by running:");
        eprintln!(
            "  sudo apt update && sudo apt install -y qemu-system-x86 qemu-utils genisoimage\n"
        );
        std::process::exit(1);
    }
}

/// Create a private staging directory. The cloud-init payload carries API keys in plaintext,
/// so it must not be world-readable.
pub fn staging_dir(name: &str) -> io::Result<PathBuf> {
    let dir = PathBuf::from(format!("/tmp/{}", name));
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

pub fn make_cloud_init_iso(
    dir: &Path,
    user_data: &str,
    instance_id: &str,
    hostname: &str,
) -> io::Result<PathBuf> {
    let user_data_path = dir.join("user-data");
    let meta_data_path = dir.join("meta-data");
    let iso_path = dir.join("cloud-init.iso");

    fs::write(&user_data_path, user_data)?;
    fs::write(
        &meta_data_path,
        format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            instance_id, hostname
        ),
    )?;

    let status = Command::new("genisoimage")
        .args([
            "-output",
            iso_path.to_str().unwrap(),
            "-volid",
            "cidata",
            "-joliet",
            "-rock",
            user_data_path.to_str().unwrap(),
            meta_data_path.to_str().unwrap(),
        ])
        .output()?;

    if !status.status.success() {
        eprintln!("Failed to generate cloud-init ISO.");
        eprintln!("{}", String::from_utf8_lossy(&status.stderr));
        return Err(io::Error::other("genisoimage failed"));
    }
    Ok(iso_path)
}

/// Copy-on-write layer over `backing`. `size` is only needed when growing past the backing
/// file's virtual size; sessions inherit the golden image's size by passing None.
pub fn create_overlay(backing: &Path, target: &Path, size: Option<&str>) -> io::Result<()> {
    let mut args: Vec<String> = vec![
        "create".into(),
        "-f".into(),
        "qcow2".into(),
        "-F".into(),
        "qcow2".into(),
        "-b".into(),
        backing.to_string_lossy().into_owned(),
        target.to_string_lossy().into_owned(),
    ];
    if let Some(size) = size {
        args.push(size.to_string());
    }

    let status = Command::new("qemu-img").args(&args).output()?;
    if !status.status.success() {
        eprintln!("Failed to create qcow2 snapshot layer.");
        eprintln!("{}", String::from_utf8_lossy(&status.stderr));
        return Err(io::Error::other("qemu-img create failed"));
    }
    Ok(())
}

/// Boots the kernel directly instead of going through the firmware and bootloader, with the
/// guest's console on a second serial port.
pub struct DirectBoot {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub cmdline: String,
    /// Where the guest's boot console goes. Never the user's terminal.
    pub console_log: PathBuf,
}

pub fn run_qemu(
    disk: &Path,
    iso: &Path,
    extra_args: Vec<String>,
    io_mode: QemuIo,
    direct: Option<&DirectBoot>,
) -> io::Result<Child> {
    let mut args: Vec<String> = vec![
        "-m".into(),
        "4G".into(),
        "-enable-kvm".into(),
        // Without this QEMU emulates `qemu64`, a deliberately conservative CPU model missing
        // most modern instruction sets. Passing the host CPU through is both much faster and
        // avoids guest userspace that feature-detects its way into bad paths.
        "-cpu".into(),
        "host".into(),
        "-smp".into(),
        "2".into(),
        "-nographic".into(),
        "-drive".into(),
        // discard=unmap lets the guest's fstrim actually return blocks to the qcow2. Without
        // it, anything written and then deleted during a build stays in the image forever:
        // npm writes ~540 MB of platform variants we delete, and the file stayed 2.1 GB.
        format!("file={},if=virtio,discard=unmap,detect-zeroes=unmap", disk.display()),
        "-drive".into(),
        format!("file={},format=raw,if=virtio", iso.display()),
        "-netdev".into(),
        "user,id=net0".into(),
    ];

    if direct.is_some() {
        // The iPXE option ROM prints a banner and is never used — nothing here network-boots.
        args.push("-device".into());
        args.push("virtio-net-pci,netdev=net0,romfile=".into());
        // Silences SeaBIOS's own serial banner.
        args.push("-fw_cfg".into());
        args.push("name=etc/sercon-port,string=0".into());
    } else {
        args.push("-device".into());
        args.push("virtio-net-pci,netdev=net0".into());
    }

    // Order matters: the first -serial is ttyS0, the second ttyS1.
    args.push("-serial".into());
    args.push("mon:stdio".into());

    if let Some(d) = direct {
        args.push("-serial".into());
        args.push(format!("file:{}", d.console_log.display()));
        args.push("-kernel".into());
        args.push(d.kernel.to_string_lossy().into_owned());
        args.push("-initrd".into());
        args.push(d.initrd.to_string_lossy().into_owned());
        args.push("-append".into());
        args.push(d.cmdline.clone());
    }

    args.extend(extra_args);

    let mut command = Command::new("qemu-system-x86_64");
    command.args(&args);

    match io_mode {
        // Inherit the terminal so the agent gets a real interactive TTY. QEMU's own stderr
        // goes to the console log when there is one: its warnings are not the user's problem,
        // and the whole point here is that nothing but geli and the command reach the screen.
        QemuIo::Interactive => {
            command.stdin(Stdio::inherit()).stdout(Stdio::inherit());
            match direct.map(|d| &d.console_log) {
                Some(log) => {
                    let f = fs::OpenOptions::new().create(true).append(true).open(log)?;
                    command.stderr(Stdio::from(f));
                }
                None => {
                    command.stderr(Stdio::inherit());
                }
            }
        }
        QemuIo::LogTo(path) => {
            let log = File::create(&path)?;
            command
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log));
        }
    }

    command.spawn()
}

/// What the proxy decided, for the end-of-session summary.
#[derive(Default)]
pub struct ProxyStats {
    pub allowed: usize,
    pub blocked: usize,
    /// Deduplicated so a retry loop against one host does not fill the summary.
    pub blocked_hosts: std::collections::BTreeSet<String>,
}

pub struct Proxy {
    pub port: u16,
    pub stats: Arc<Mutex<ProxyStats>>,
}

/// Start the egress proxy on an ephemeral localhost port.
///
/// The accept loop runs on a detached thread: it dies with the process, so there is nothing
/// to shut down or clean up. The guest reaches it only through slirp's single forwarded port.
pub fn start_proxy(allow: Vec<String>, log_path: PathBuf) -> io::Result<Proxy> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let stats = Arc::new(Mutex::new(ProxyStats::default()));

    let log = Arc::new(Mutex::new(
        fs::OpenOptions::new().create(true).append(true).open(&log_path)?,
    ));
    let allow = Arc::new(allow);
    let thread_stats = stats.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (allow, log, stats) = (allow.clone(), log.clone(), thread_stats.clone());
            std::thread::spawn(move || serve(stream, &allow, &log, &stats));
        }
    });

    Ok(Proxy { port, stats })
}

/// Error responses must say `Connection: close`.
///
/// Without it a client keeps the proxy connection in its pool, believing it reusable, then
/// sends its next CONNECT down a socket this side has already dropped — and waits out its
/// own timeout. Observed as git taking 300s on the request *after* a rejected one.
pub(crate) fn refuse(client: &mut TcpStream, status: &str) {
    let _ = client.write_all(
        format!("HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", status)
            .as_bytes(),
    );
    let _ = client.flush();
    let _ = client.shutdown(std::net::Shutdown::Both);
}

pub(crate) fn note(log: &Mutex<File>, line: &str) {
    if let Ok(mut f) = log.lock() {
        let _ = writeln!(f, "{}", line);
    }
}

pub(crate) fn serve(mut client: TcpStream, allow: &[String], log: &Mutex<File>, stats: &Mutex<ProxyStats>) {
    // A client that opens a connection and then stalls would otherwise hold a thread for the
    // life of the session. The timeout covers reading the request head only; once the tunnel
    // is established it is cleared, because an idle agent session is legitimately quiet.
    let _ = client.set_read_timeout(Some(Duration::from_secs(20)));

    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match client.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
            Err(_) => return,
        }
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        // A client that never finishes its request head is not getting unbounded memory.
        if head.len() > 16 * 1024 {
            return;
        }
    }

    let request = parse_proxy_request(&String::from_utf8_lossy(&head));
    let (host, port) = match request {
        ProxyRequest::Connect { host, port } => (host, port),
        ProxyRequest::Unsupported(line) => {
            note(log, &format!("rejected non-CONNECT: {}", line));
            refuse(&mut client, "405 Method Not Allowed");
            return;
        }
    };

    if port != ALLOWED_PORT {
        note(log, &format!("blocked {}:{} (only {} is proxied)", host, port, ALLOWED_PORT));
        deny(&mut client, stats, &host);
        return;
    }
    if !host_allowed(&host, allow) {
        note(log, &format!("blocked {}:{}", host, port));
        deny(&mut client, stats, &host);
        return;
    }

    let upstream = match TcpStream::connect_timeout(
        &match (host.as_str(), port).to_socket_addrs().ok().and_then(|mut a| a.next()) {
            Some(addr) => addr,
            None => {
                note(log, &format!("allowed {}:{} but DNS failed", host, port));
                refuse(&mut client, "502 Bad Gateway");
                return;
            }
        },
        Duration::from_secs(15),
    ) {
        Ok(s) => s,
        Err(e) => {
            note(log, &format!("allowed {}:{} but connect failed: {}", host, port, e));
            refuse(&mut client, "502 Bad Gateway");
            return;
        }
    };

    note(log, &format!("allowed {}:{}", host, port));
    if let Ok(mut s) = stats.lock() {
        s.allowed += 1;
    }
    if client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").is_err() {
        return;
    }
    // Clear the head-reading timeout: a live session can sit idle between requests.
    let _ = client.set_read_timeout(None);
    tunnel(client, upstream);
}

pub(crate) fn deny(client: &mut TcpStream, stats: &Mutex<ProxyStats>, host: &str) {
    if let Ok(mut s) = stats.lock() {
        s.blocked += 1;
        s.blocked_hosts.insert(host.to_string());
    }
    refuse(client, "403 Forbidden");
}

pub(crate) fn tunnel(client: TcpStream, upstream: TcpStream) {
    let Ok(mut client_read) = client.try_clone() else { return };
    let Ok(mut upstream_write) = upstream.try_clone() else { return };
    let mut client_write = client;
    let mut upstream_read = upstream;

    let up = std::thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(std::net::Shutdown::Write);
    });
    let _ = io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
}

/// Show which phase the guest is in and return once it reports ready.
///
/// Returns false on timeout. That matters: a guest whose cloud-init broke will never write
/// the marker, and the terminal has to be handed over regardless rather than spin forever.
pub fn track_boot(log: &Path, animate: bool, color: bool, timeout: Duration) -> bool {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let started = Instant::now();
    let deadline = started + timeout;
    let mut frame = 0usize;
    let mut announced = "";

    while Instant::now() < deadline {
        let text = fs::read_to_string(log).unwrap_or_default();
        if text.contains(READY_MARKER) {
            if animate {
                eprint!("\r\x1b[2K");
                let _ = io::stderr().flush();
            }
            return true;
        }

        let phase = boot_phase(&text);
        if animate {
            eprint!(
                "\r\x1b[2K{}",
                render_spinner_line(
                    FRAMES[frame % FRAMES.len()],
                    phase,
                    started.elapsed().as_secs_f32(),
                    color
                )
            );
            let _ = io::stderr().flush();
            frame += 1;
        } else if phase != announced {
            // Piped or logged: one line per phase instead of an animation nobody will see.
            eprintln!("  booting · {}", phase);
            announced = phase;
        }

        std::thread::sleep(Duration::from_millis(90));
    }
    false
}

