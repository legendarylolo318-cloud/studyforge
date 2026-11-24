//! Client side of the studyd protocol. A dedicated thread polls the helper
//! once a second (status + events) and executes queued requests; results are
//! forwarded to the backend as commands. Nothing here touches the UI thread.

use crate::backend::Cmd;
use interprocess::local_socket::{prelude::*, Stream};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;
use studyforge_core::ipc::*;

#[cfg(unix)]
fn to_name(endpoint: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    use interprocess::local_socket::GenericFilePath;
    endpoint.to_fs_name::<GenericFilePath>()
}

#[cfg(windows)]
fn to_name(endpoint: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    use interprocess::local_socket::GenericNamespaced;
    endpoint.trim_start_matches(r"\\.\pipe\").to_ns_name::<GenericNamespaced>()
}

fn read_line(r: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    let n = r.take(MAX_LINE_BYTES as u64 + 1).read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "helper closed the connection"));
    }
    Ok(line)
}

/// One request on a fresh connection (including the version handshake).
pub fn request(endpoint: &str, req: &Request) -> io::Result<Response> {
    let stream = Stream::connect(to_name(endpoint)?)?;
    let mut reader = BufReader::new(&stream);
    let mut send = &stream;
    send.write_all(encode(&Request::Hello { protocol: PROTOCOL_VERSION, client: "studyforge".into() }).as_bytes())?;
    match decode::<Response>(&read_line(&mut reader)?).map_err(io::Error::other)? {
        Response::Hello { .. } => {}
        Response::Error { message } => return Err(io::Error::other(message)),
        other => return Err(io::Error::other(format!("unexpected handshake reply: {other:?}"))),
    }
    send.write_all(encode(req).as_bytes())?;
    send.flush()?;
    decode(&read_line(&mut reader)?).map_err(io::Error::other)
}

pub enum LinkMsg {
    /// Execute now; the result comes back as `Cmd::DaemonReply { tag, .. }`.
    Request { req: Request, tag: &'static str },
    Shutdown,
}

#[derive(Clone)]
pub struct DaemonLink {
    tx: Sender<LinkMsg>,
}

impl DaemonLink {
    pub fn spawn(endpoint: String, backend: Sender<Cmd>) -> DaemonLink {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new().name("daemon-link".into()).spawn(move || run(endpoint, rx, backend)).expect("spawn daemon link");
        DaemonLink { tx }
    }
    pub fn send(&self, req: Request, tag: &'static str) {
        let _ = self.tx.send(LinkMsg::Request { req, tag });
    }
    pub fn shutdown(&self) {
        let _ = self.tx.send(LinkMsg::Shutdown);
    }
}

fn run(endpoint: String, rx: Receiver<LinkMsg>, backend: Sender<Cmd>) {
    loop {
        match rx.recv_timeout(Duration::from_millis(1000)) {
            Ok(LinkMsg::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(LinkMsg::Request { req, tag }) => {
                let result = request(&endpoint, &req).map_err(|e| e.to_string());
                if backend.send(Cmd::DaemonReply { tag, result }).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let status = request(&endpoint, &Request::Status);
                match status {
                    Ok(Response::Status(s)) => {
                        let _ = backend.send(Cmd::DaemonStatus(Ok(s)));
                        if let Ok(Response::Events { events }) = request(&endpoint, &Request::PollEvents) {
                            if !events.is_empty() {
                                let _ = backend.send(Cmd::DaemonEvents(events));
                            }
                        }
                    }
                    Ok(Response::Error { message }) => {
                        let _ = backend.send(Cmd::DaemonStatus(Err(message)));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let _ = backend.send(Cmd::DaemonStatus(Err(e.to_string())));
                    }
                }
            }
        }
    }
}
