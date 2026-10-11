//! Client side of the daemon socket.
//!
//! A reader thread parses incoming lines and hands them to the GTK main loop
//! through an async channel; requests are written straight from the main
//! thread (they are small and the socket is local).

use std::cell::{Cell, RefCell};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use whatkroft_ipc::{Request, RequestEnvelope, ServerMessage, decode_line, encode_line};

pub struct Conn {
    writer: RefCell<UnixStream>,
    next_id: Cell<u64>,
}

impl Conn {
    /// Connect to the daemon. The receiver closes when the daemon goes away.
    pub fn connect() -> io::Result<(Self, async_channel::Receiver<ServerMessage>)> {
        let stream = UnixStream::connect(whatkroft_ipc::socket_path())?;
        let reader = BufReader::new(stream.try_clone()?);
        let (tx, rx) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("daemon-reader".into())
            .spawn(move || {
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    match decode_line::<ServerMessage>(&line) {
                        Ok(message) => {
                            if tx.send_blocking(message).is_err() {
                                break;
                            }
                        }
                        Err(e) => log::warn!("unreadable line from daemon: {e}"),
                    }
                }
            })?;
        Ok((
            Self {
                writer: RefCell::new(stream),
                next_id: Cell::new(1),
            },
            rx,
        ))
    }

    /// Send a request; the reply arrives on the channel with the returned id.
    pub fn request(&self, request: Request) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let line = encode_line(&RequestEnvelope { id, request });
        if let Err(e) = self.writer.borrow_mut().write_all(line.as_bytes()) {
            // The reader thread sees the same failure and closes the channel,
            // which is what triggers the reconnect.
            log::warn!("writing to daemon: {e}");
        }
        id
    }
}

/// Start `whatkroftd`, preferring the copy installed next to this binary so a
/// build tree or a relocated install runs its own matching daemon.
///
/// The daemon is not tied to the window: it keeps running after the window
/// closes so messages keep arriving. The handle is only kept to tell whether
/// the copy we started is still alive.
pub fn spawn_daemon() -> io::Result<Child> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("whatkroftd")))
        .filter(|path| path.is_file());
    let program = sibling.unwrap_or_else(|| PathBuf::from("whatkroftd"));
    log::info!("starting daemon {}", program.display());
    Command::new(program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
}
