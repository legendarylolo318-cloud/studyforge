//! Local socket / named pipe transport for the daemon side (and the tiny
//! client used by `studyd status`).
//!
//! Security:
//! * Unix: the socket is world-connectable (mode 0666) but every connection is
//!   checked with SO_PEERCRED (via `peer_creds`) against `allowed_users`
//!   (root is always allowed) before a single byte is read.
//! * Windows: the pipe gets a DACL that grants access only to SYSTEM,
//!   Administrators and the installing user's SID (from `allowed_users`).

use interprocess::local_socket::{prelude::*, ListenerNonblockingMode, ListenerOptions, Stream};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use studyforge_core::ipc::*;

pub const MAX_CONNECTIONS: usize = 16;

#[cfg(unix)]
fn to_name(endpoint: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    use interprocess::local_socket::GenericFilePath;
    endpoint.to_fs_name::<GenericFilePath>()
}

#[cfg(windows)]
fn to_name(endpoint: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    use interprocess::local_socket::GenericNamespaced;
    // `\\.\pipe\studyd` -> `studyd`
    endpoint.trim_start_matches(r"\\.\pipe\").to_ns_name::<GenericNamespaced>()
}

/// SDDL for the Windows pipe: SYSTEM plus each allowed SID, nobody else
/// (protected DACL, no inheritance). SIDs are validated so a bad
/// `allowed_users` line cannot inject extra ACEs. Pure string logic, so it is
/// unit tested on every platform.
pub fn sddl_for(allowed: &[String]) -> String {
    let mut s = String::from("D:P(A;;GA;;;SY)");
    for sid in allowed {
        let valid = sid.starts_with("S-1-")
            && sid.len() < 200
            && sid[4..].split('-').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
        if valid {
            s.push_str(&format!("(A;;GA;;;{sid})"));
        } else {
            tracing::warn!("ignoring malformed SID in allowed_users");
        }
    }
    s
}

/// Is this peer allowed to talk to us? Root (uid 0) always is.
#[cfg(unix)]
pub fn peer_allowed(stream: &Stream, allowed: &[String]) -> bool {
    match stream.peer_creds().ok().and_then(|c| c.euid()) {
        Some(0) => true,
        Some(uid) => allowed.iter().any(|a| a == &uid.to_string()),
        None => false,
    }
}
#[cfg(windows)]
pub fn peer_allowed(_stream: &Stream, _allowed: &[String]) -> bool {
    true // enforced by the pipe's DACL
}

fn read_line_limited(r: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = String::new();
    let n = r.take(MAX_LINE_BYTES as u64 + 1).read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_LINE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request too large"));
    }
    Ok(Some(line))
}

/// Anything that can answer requests (the engine behind a mutex).
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: Request) -> Response;
}

fn serve_connection(stream: Stream, handler: &dyn Handler) -> io::Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut send = &stream;
    let mut greeted = false;
    while let Some(line) = read_line_limited(&mut reader)? {
        let resp = match decode::<Request>(&line) {
            Err(e) => Response::Error { message: format!("bad request: {e}") },
            Ok(req) => {
                if !greeted && !matches!(req, Request::Hello { .. }) {
                    Response::Error { message: "expected Hello handshake first".into() }
                } else {
                    let r = handler.handle(req);
                    if matches!(r, Response::Hello { .. }) {
                        greeted = true;
                    }
                    r
                }
            }
        };
        send.write_all(encode(&resp).as_bytes())?;
        send.flush()?;
    }
    Ok(())
}

pub fn serve(
    endpoint: &str,
    allowed: Arc<Mutex<Vec<String>>>,
    handler: Arc<dyn Handler>,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        let p = std::path::Path::new(endpoint);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _ = std::fs::remove_file(p); // stale socket from a previous run
    }
    #[allow(unused_mut)]
    let mut opts = ListenerOptions::new().name(to_name(endpoint)?);
    #[cfg(unix)]
    {
        use interprocess::os::unix::local_socket::ListenerOptionsExt;
        opts = opts.mode(0o666);
    }
    #[cfg(windows)]
    {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        let sddl = sddl_for(&allowed.lock().unwrap());
        let wide = widestring::U16CString::from_str(&sddl).map_err(io::Error::other)?;
        opts = opts.security_descriptor(SecurityDescriptor::deserialize(&wide)?);
    }
    let listener = opts.create_sync()?;
    listener.set_nonblocking(ListenerNonblockingMode::Accept)?;
    tracing::info!("listening on {endpoint}");
    let active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok(stream) => {
                if !peer_allowed(&stream, &allowed.lock().unwrap()) {
                    tracing::warn!("rejected connection from a user that is not in allowed_users");
                    continue; // dropping the stream closes it
                }
                if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    active.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let (h, a) = (handler.clone(), active.clone());
                std::thread::spawn(move || {
                    if let Err(e) = serve_connection(stream, h.as_ref()) {
                        tracing::debug!("connection ended: {e}");
                    }
                    a.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                tracing::warn!("accept error: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    #[cfg(unix)]
    let _ = std::fs::remove_file(endpoint);
    Ok(())
}

/// One-shot blocking request (used by `studyd status` and the tests).
pub fn request(endpoint: &str, req: &Request) -> io::Result<Response> {
    let stream = Stream::connect(to_name(endpoint)?)?;
    let mut reader = BufReader::new(&stream);
    let mut send = &stream;
    send.write_all(encode(&Request::Hello { protocol: PROTOCOL_VERSION, client: "studyd-cli".into() }).as_bytes())?;
    let hello =
        read_line_limited(&mut reader)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no hello"))?;
    if let Response::Error { message } = decode::<Response>(&hello).map_err(io::Error::other)? {
        return Err(io::Error::other(message));
    }
    send.write_all(encode(req).as_bytes())?;
    let line =
        read_line_limited(&mut reader)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no response"))?;
    decode(&line).map_err(io::Error::other)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct Echo;
    impl Handler for Echo {
        fn handle(&self, req: Request) -> Response {
            match req {
                Request::Hello { protocol, .. } if protocol != PROTOCOL_VERSION => {
                    Response::Error { message: "mismatch".into() }
                }
                Request::Hello { .. } => Response::Hello { protocol: PROTOCOL_VERSION, version: "test".into() },
                Request::Ping => Response::Pong,
                _ => Response::Ok,
            }
        }
    }

    fn start(allowed: Vec<String>) -> (tempfile::TempDir, String, Arc<AtomicBool>) {
        let d = tempfile::tempdir().unwrap();
        let ep = d.path().join("s.sock").to_string_lossy().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let (e2, s2) = (ep.clone(), stop.clone());
        std::thread::spawn(move || serve(&e2, Arc::new(Mutex::new(allowed)), Arc::new(Echo), s2).unwrap());
        for _ in 0..100 {
            if std::path::Path::new(&ep).exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        (d, ep, stop)
    }

    fn my_uid() -> String {
        // SAFETY: getuid is always safe to call
        unsafe { libc::getuid() }.to_string()
    }

    #[test]
    fn handshake_and_request_for_allowed_uid() {
        let (_d, ep, stop) = start(vec![my_uid()]);
        assert_eq!(request(&ep, &Request::Ping).unwrap(), Response::Pong);
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn peer_not_in_allowed_users_is_rejected_unless_root() {
        let (_d, ep, stop) = start(vec!["424242".into()]);
        let r = request(&ep, &Request::Ping);
        if my_uid() == "0" {
            assert!(r.is_ok(), "root is always allowed");
        } else {
            assert!(r.is_err(), "unexpected success: {r:?}");
        }
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn first_message_must_be_hello_and_garbage_is_survivable() {
        let (_d, ep, stop) = start(vec![my_uid()]);
        let stream = Stream::connect(to_name(&ep).unwrap()).unwrap();
        let mut reader = BufReader::new(&stream);
        let mut send = &stream;
        send.write_all(b"{\"type\":\"Ping\"}\n").unwrap();
        let mut l = String::new();
        reader.read_line(&mut l).unwrap();
        assert!(matches!(decode::<Response>(&l).unwrap(), Response::Error { .. }));
        send.write_all(b"this is not json\n").unwrap();
        l.clear();
        reader.read_line(&mut l).unwrap();
        assert!(matches!(decode::<Response>(&l).unwrap(), Response::Error { .. }));
        send.write_all(encode(&Request::Hello { protocol: PROTOCOL_VERSION, client: "t".into() }).as_bytes()).unwrap();
        l.clear();
        reader.read_line(&mut l).unwrap();
        assert!(matches!(decode::<Response>(&l).unwrap(), Response::Hello { .. }));
        send.write_all(encode(&Request::Ping).as_bytes()).unwrap();
        l.clear();
        reader.read_line(&mut l).unwrap();
        assert_eq!(decode::<Response>(&l).unwrap(), Response::Pong);
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn protocol_mismatch_is_reported() {
        let (_d, ep, stop) = start(vec![my_uid()]);
        let stream = Stream::connect(to_name(&ep).unwrap()).unwrap();
        let mut reader = BufReader::new(&stream);
        let mut send = &stream;
        send.write_all(encode(&Request::Hello { protocol: 0, client: "old".into() }).as_bytes()).unwrap();
        let mut l = String::new();
        reader.read_line(&mut l).unwrap();
        assert!(matches!(decode::<Response>(&l).unwrap(), Response::Error { .. }));
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn oversized_request_closes_the_connection() {
        let (_d, ep, stop) = start(vec![my_uid()]);
        let stream = Stream::connect(to_name(&ep).unwrap()).unwrap();
        let mut reader = BufReader::new(&stream);
        let mut send = &stream;
        let big = vec![b'a'; MAX_LINE_BYTES + 10];
        let _ = send.write_all(&big);
        let _ = send.write_all(b"\n");
        let mut l = String::new();
        let n = reader.read_line(&mut l).unwrap_or(0);
        assert_eq!(n, 0, "server hung up without a reply");
        stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod sddl_tests {
    use super::sddl_for;

    #[test]
    fn only_system_and_listed_sids() {
        assert_eq!(sddl_for(&[]), "D:P(A;;GA;;;SY)");
        let sid = "S-1-5-21-1004336348-1177238915-682003330-1001";
        assert_eq!(sddl_for(&[sid.into()]), format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"));
    }

    #[test]
    fn malformed_sids_cannot_inject_aces() {
        for bad in ["WD", "S-1-1-0)(A;;GA;;;WD", "S-1-", "S-1-5-x", "S-1-5-21-1 (A;;GA;;;WD)", "", "S-1-5-21--1"] {
            assert_eq!(sddl_for(&[bad.to_string()]), "D:P(A;;GA;;;SY)", "{bad:?}");
        }
    }
}
