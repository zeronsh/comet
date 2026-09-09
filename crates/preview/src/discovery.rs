//! Observe only this user's listeners. HTTP probes run only after project
//! ownership has been established from the process's actual working directory.
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct Listener {
    pub pid: u32,
    pub parent: u32,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub started_at: u64,
    pub address: SocketAddr,
    pub zeron_owned: bool,
}

impl Listener {
    pub fn belongs_to(&self, root: &Path) -> bool {
        self.cwd.starts_with(root)
    }
}

/// Preserve a useful framework label without advertising process arguments,
/// which may contain credentials. Rank frontends above generic HTTP services.
pub fn framework(args: &[String]) -> (&'static str, u8) {
    for (needle, name) in [
        ("vite", "Vite"),
        ("next", "Next.js"),
        ("astro", "Astro"),
        ("miniflare", "Miniflare"),
    ] {
        if args.iter().any(|arg| {
            let arg = arg.to_lowercase();
            Path::new(&arg).file_stem().and_then(|s| s.to_str()) == Some(needle)
                || arg.contains(&format!("/{needle}/"))
                || arg.starts_with(&format!("{needle}-server"))
        }) {
            return (name, if needle == "miniflare" { 80 } else { 100 });
        }
    }
    if args.first().is_some_and(|s| {
        Path::new(s)
            .file_name()
            .is_some_and(|n| n == "node" || n == "bun" || n == "deno")
    }) {
        ("Node HTTP server", 50)
    } else {
        ("HTTP server", 40)
    }
}

/// Port flags are operational settings, never part of a service identity.
pub fn command_identity(args: &[String]) -> String {
    let mut skip = false;
    let python_http = args.iter().any(|arg| arg == "http.server");
    args.iter()
        .filter_map(|arg| {
            if skip {
                skip = false;
                return None;
            }
            if matches!(arg.as_str(), "--port" | "-p" | "--inspect-port") {
                skip = true;
                return None;
            }
            if arg.starts_with("--port=")
                || arg.starts_with("PORT=")
                || arg.starts_with("--inspect-port=")
            {
                return None;
            }
            if python_http && arg.parse::<u16>().is_ok() {
                return None;
            }
            Some(arg.as_str())
        })
        .collect::<Vec<_>>()
        .join("\0")
}

pub async fn is_http(address: SocketAddr) -> bool {
    tokio::time::timeout(Duration::from_millis(800), async move {
        let mut socket = tokio::net::TcpStream::connect(address).await.ok()?;
        socket
            .write_all(
                format!(
                    "HEAD / HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
                    address.port()
                )
                .as_bytes(),
            )
            .await
            .ok()?;
        let mut prefix = [0; 12];
        socket.read_exact(&mut prefix).await.ok()?;
        let version = &prefix[..9];
        let status = std::str::from_utf8(&prefix[9..])
            .ok()?
            .parse::<u16>()
            .ok()?;
        ((version == b"HTTP/1.1 " || version == b"HTTP/1.0 ") && (100..600).contains(&status))
            .then_some(())
    })
    .await
    .ok()
    .flatten()
    .is_some()
}

fn mark_descendants(listeners: &mut [Listener], parents: &HashMap<u32, u32>, owner: u32) {
    for listener in listeners {
        let mut pid = listener.pid;
        for _ in 0..128 {
            if pid == owner {
                listener.zeron_owned = true;
                break;
            }
            let Some(&parent) = parents.get(&pid) else {
                break;
            };
            if parent == pid || parent == 0 {
                break;
            }
            pid = parent;
        }
    }
}

#[cfg(target_os = "linux")]
pub fn listeners() -> Vec<Listener> {
    use std::{fs, os::unix::fs::MetadataExt};
    let mut sockets = HashMap::new();
    for (file, ipv6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        if let Ok(text) = fs::read_to_string(file) {
            for line in text.lines().skip(1) {
                let parts: Vec<_> = line.split_whitespace().collect();
                if parts.len() <= 9 || parts[3] != "0A" {
                    continue;
                }
                if let Some(address) = proc_address(parts[1], ipv6) {
                    sockets.insert(parts[9].to_string(), address);
                }
            }
        }
    }
    let boot = fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("btime ")
                    .and_then(|v| v.parse::<u64>().ok())
            })
        })
        .unwrap_or(0);
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let uid = unsafe { libc::geteuid() };
    let mut result = Vec::new();
    let mut parents = HashMap::new();
    let Ok(processes) = fs::read_dir("/proc") else {
        return result;
    };
    for process in processes.flatten() {
        let Ok(pid) = process.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let path = process.path();
        if !fs::metadata(&path).is_ok_and(|m| m.uid() == uid) {
            continue;
        }
        let Ok(stat) = fs::read_to_string(path.join("stat")) else {
            continue;
        };
        let Some((_, tail)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = tail.split_whitespace().collect();
        let Some(parent) = fields.get(1).and_then(|v| v.parse().ok()) else {
            continue;
        };
        parents.insert(pid, parent);
        let Ok(cwd) = fs::read_link(path.join("cwd")) else {
            continue;
        };
        let started_at = boot * 1000
            + fields
                .get(19)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                * 1000
                / ticks;
        let args = fs::read(path.join("cmdline"))
            .unwrap_or_default()
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>();
        let Ok(fds) = fs::read_dir(path.join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(link) = fs::read_link(fd.path()) else {
                continue;
            };
            let link = link.to_string_lossy();
            let Some(inode) = link
                .strip_prefix("socket:[")
                .and_then(|s| s.strip_suffix(']'))
            else {
                continue;
            };
            if let Some(&address) = sockets.get(inode) {
                result.push(Listener {
                    pid,
                    parent,
                    cwd: cwd.clone(),
                    args: args.clone(),
                    started_at,
                    address,
                    zeron_owned: false,
                });
            }
        }
    }
    mark_descendants(&mut result, &parents, std::process::id());
    result.sort_by_key(|l| (l.address, l.pid));
    result.dedup_by_key(|l| (l.address, l.pid));
    result
}

#[cfg(target_os = "linux")]
fn proc_address(value: &str, ipv6: bool) -> Option<SocketAddr> {
    let (host, port) = value.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = if ipv6 {
        if host.len() != 32 {
            return None;
        }
        let mut bytes = [0; 16];
        for (i, chunk) in host.as_bytes().chunks_exact(8).enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(
                &u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16)
                    .ok()?
                    .to_le_bytes(),
            );
        }
        IpAddr::V6(Ipv6Addr::from(bytes))
    } else {
        IpAddr::V4(Ipv4Addr::from(
            u32::from_str_radix(host, 16).ok()?.to_le_bytes(),
        ))
    };
    if !ip.is_loopback() && !ip.is_unspecified() {
        return None;
    }
    Some(SocketAddr::new(
        if ip.is_unspecified() {
            if ipv6 {
                Ipv6Addr::LOCALHOST.into()
            } else {
                Ipv4Addr::LOCALHOST.into()
            }
        } else {
            ip
        },
        port,
    ))
}

#[cfg(target_os = "macos")]
pub fn listeners() -> Vec<Listener> {
    use chrono::TimeZone;
    use std::process::Command;
    let uid = unsafe { libc::geteuid() }.to_string();
    let fields = |arguments: &[&str]| -> Vec<(u32, String)> {
        let Ok(output) = Command::new("/usr/sbin/lsof").args(arguments).output() else {
            return Vec::new();
        };
        let mut pid = 0;
        let mut values = Vec::new();
        for field in output.stdout.split(|b| *b == 0) {
            let field = String::from_utf8_lossy(field);
            let field = field.trim_start_matches('\n');
            if let Some(value) = field.strip_prefix('p') {
                pid = value.parse().unwrap_or(0);
            }
            if let Some(value) = field.strip_prefix('n') {
                values.push((pid, value.to_owned()));
            }
        }
        values
    };
    let cwds: HashMap<_, _> = fields(&["-nP", "-a", "-u", &uid, "-d", "cwd", "-F0pn"])
        .into_iter()
        .collect();
    let mut parents = HashMap::new();
    let mut processes = HashMap::new();
    if let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,lstart=,command="])
        .env("LC_ALL", "C")
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() < 8 {
                continue;
            }
            let (Ok(pid), Ok(parent)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) else {
                continue;
            };
            parents.insert(pid, parent);
            let started_at =
                chrono::NaiveDateTime::parse_from_str(&parts[2..7].join(" "), "%a %b %e %T %Y")
                    .ok()
                    .and_then(|d| chrono::Local.from_local_datetime(&d).earliest())
                    .map(|d| d.timestamp_millis().max(0) as u64)
                    .unwrap_or(0);
            processes.insert(
                pid,
                (
                    parent,
                    started_at,
                    parts[7..].iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                ),
            );
        }
    }
    let mut result = Vec::new();
    for (pid, socket) in fields(&["-nP", "-a", "-u", &uid, "-iTCP", "-sTCP:LISTEN", "-F0pn"]) {
        let Some(cwd) = cwds.get(&pid).and_then(|s| std::fs::canonicalize(s).ok()) else {
            continue;
        };
        let Some((parent, started_at, args)) = processes.get(&pid) else {
            continue;
        };
        let addresses = if let Some(port) = socket
            .strip_prefix("*:")
            .and_then(|p| p.parse::<u16>().ok())
        {
            vec![
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
            ]
        } else {
            socket
                .parse::<SocketAddr>()
                .ok()
                .filter(|a| a.ip().is_loopback())
                .into_iter()
                .collect()
        };
        for address in addresses {
            result.push(Listener {
                pid,
                parent: *parent,
                cwd: cwd.clone(),
                args: args.clone(),
                started_at: *started_at,
                address,
                zeron_owned: false,
            });
        }
    }
    mark_descendants(&mut result, &parents, std::process::id());
    result
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn listeners() -> Vec<Listener> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_identity_ignores_port_configuration() {
        let args = |port: &str| vec!["node".into(), "api.js".into(), "--port".into(), port.into()];
        assert_eq!(
            command_identity(&args("3000")),
            command_identity(&args("3001"))
        );
        assert_ne!(
            command_identity(&args("3000")),
            command_identity(&["node".into(), "docs.js".into()])
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn accepts_only_loopback_reachable_listeners() {
        assert_eq!(
            proc_address("0100007F:1435", false).unwrap(),
            "127.0.0.1:5173".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            proc_address("00000000000000000000000001000000:1435", true).unwrap(),
            "[::1]:5173".parse::<SocketAddr>().unwrap()
        );
        assert!(proc_address("0101A8C0:1435", false).is_none());
        assert!(proc_address("malformed:00", true).is_none());
    }
    #[tokio::test]
    async fn requires_an_http_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for response in [
                b"SSH-2.0-test\r\n".as_slice(),
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
            ] {
                let (mut client, _) = listener.accept().await.unwrap();
                let mut input = [0; 512];
                let _ = client.read(&mut input).await;
                client.write_all(response).await.unwrap();
            }
        });
        assert!(!is_http(address).await);
        assert!(is_http(address).await);
        task.await.unwrap();
    }
}
