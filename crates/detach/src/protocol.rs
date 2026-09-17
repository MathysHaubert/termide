//! Framing for the client ↔ daemon socket.
//!
//! One byte of tag, a big-endian `u32` length, then the payload. Deliberately
//! hand-rolled: the daemon sits on the hot path between a keystroke and the
//! PTY, and a serde round-trip per keypress buys nothing over four bytes of
//! header.

use anyhow::{bail, Result};
use std::io::{Read, Write};

/// Upper bound on a single frame's payload.
///
/// The length prefix arrives from the socket, so it is attacker-controlled in
/// the sense that a confused peer can name any size; refusing to allocate more
/// than this keeps a garbage header from turning into a 4 GiB allocation.
const MAX_PAYLOAD: usize = 1 << 20;

const TAG_ATTACH: u8 = 0x01;
const TAG_INPUT: u8 = 0x02;
const TAG_RESIZE: u8 = 0x03;
const TAG_DETACH: u8 = 0x04;
const TAG_REQUEST_DETACH: u8 = 0x05;

const TAG_OUTPUT: u8 = 0x81;
const TAG_EXITED: u8 = 0x82;
const TAG_BUSY: u8 = 0x83;
const TAG_ATTACHED: u8 = 0x84;

/// Sent by an attaching client, or by the hosted termide asking to be released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFrame {
    /// Take over the instance at this terminal size.
    ///
    /// `term` is the client's `$TERM` and `caps` its keyboard capabilities,
    /// both of which the hosted process adopts. It cannot determine either
    /// for itself: its `$TERM` came from whichever terminal started the
    /// instance, and a capability probe sent down its PTY reaches the daemon,
    /// which does not answer one.
    Attach {
        cols: u16,
        rows: u16,
        term: String,
        caps: ClientCaps,
    },
    /// Raw bytes from the client's stdin, forwarded to the PTY unchanged.
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// The client is leaving; the instance stays alive.
    Detach,
    /// The hosted termide asking the daemon to drop the current client.
    /// This is how the in-app "detach instance" action works without the
    /// client having to intercept a chord of its own.
    RequestDetach,
}

/// What the client's terminal can do, as probed by the client itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientCaps {
    /// The terminal answered the Kitty keyboard-protocol query.
    pub kitty: bool,
    /// The client is running over SSH, where the probe is skipped entirely.
    pub via_ssh: bool,
    /// The terminal widens a text-presentation emoji after U+FE0F.
    pub vs16_wide: bool,
}

impl ClientCaps {
    fn to_bits(self) -> u8 {
        u8::from(self.kitty) | (u8::from(self.via_ssh) << 1) | (u8::from(self.vs16_wide) << 2)
    }

    fn from_bits(bits: u8) -> Self {
        Self {
            kitty: bits & 0b001 != 0,
            via_ssh: bits & 0b010 != 0,
            vs16_wide: bits & 0b100 != 0,
        }
    }
}

/// Sent by the daemon to the attached client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerFrame {
    /// Raw PTY output.
    Output(Vec<u8>),
    /// The hosted termide exited; the instance is over.
    Exited(i32),
    /// Another client is already attached.
    Busy,
    /// Attach accepted.
    Attached,
}

fn write_frame<W: Write>(w: &mut W, tag: u8, payload: &[u8]) -> Result<()> {
    let mut header = [0u8; 5];
    header[0] = tag;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&header)?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

fn read_frame<R: Read>(r: &mut R) -> Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        // A peer that goes away mid-frame is a detach or a crash, not a
        // protocol error: both are reported as a clean end of stream.
        Err(e)
            if e.kind() == std::io::ErrorKind::UnexpectedEof
                || e.kind() == std::io::ErrorKind::ConnectionReset =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e.into()),
    }

    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_PAYLOAD {
        bail!("Frame payload of {len} bytes exceeds the {MAX_PAYLOAD} byte limit");
    }

    let mut payload = vec![0u8; len];
    if len > 0 {
        match r.read_exact(&mut payload) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::UnexpectedEof
                    || e.kind() == std::io::ErrorKind::ConnectionReset =>
            {
                return Ok(None)
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some((header[0], payload)))
}

impl ClientFrame {
    pub fn write_to<W: Write>(&self, w: &mut W) -> Result<()> {
        match self {
            ClientFrame::Attach {
                cols,
                rows,
                term,
                caps,
            } => {
                let mut payload = Vec::with_capacity(5 + term.len());
                payload.extend_from_slice(&cols.to_be_bytes());
                payload.extend_from_slice(&rows.to_be_bytes());
                payload.push(caps.to_bits());
                payload.extend_from_slice(term.as_bytes());
                write_frame(w, TAG_ATTACH, &payload)
            }
            ClientFrame::Input(bytes) => write_frame(w, TAG_INPUT, bytes),
            ClientFrame::Resize { cols, rows } => {
                let mut payload = [0u8; 4];
                payload[..2].copy_from_slice(&cols.to_be_bytes());
                payload[2..].copy_from_slice(&rows.to_be_bytes());
                write_frame(w, TAG_RESIZE, &payload)
            }
            ClientFrame::Detach => write_frame(w, TAG_DETACH, &[]),
            ClientFrame::RequestDetach => write_frame(w, TAG_REQUEST_DETACH, &[]),
        }
    }

    /// Read the next frame, or `None` once the peer has closed the socket.
    pub fn read_from<R: Read>(r: &mut R) -> Result<Option<Self>> {
        let Some((tag, payload)) = read_frame(r)? else {
            return Ok(None);
        };
        let frame = match tag {
            TAG_ATTACH => {
                if payload.len() < 5 {
                    bail!("Attach frame is truncated");
                }
                ClientFrame::Attach {
                    cols: u16::from_be_bytes([payload[0], payload[1]]),
                    rows: u16::from_be_bytes([payload[2], payload[3]]),
                    caps: ClientCaps::from_bits(payload[4]),
                    term: String::from_utf8_lossy(&payload[5..]).into_owned(),
                }
            }
            TAG_INPUT => ClientFrame::Input(payload),
            TAG_RESIZE => {
                if payload.len() < 4 {
                    bail!("Resize frame is truncated");
                }
                ClientFrame::Resize {
                    cols: u16::from_be_bytes([payload[0], payload[1]]),
                    rows: u16::from_be_bytes([payload[2], payload[3]]),
                }
            }
            TAG_DETACH => ClientFrame::Detach,
            TAG_REQUEST_DETACH => ClientFrame::RequestDetach,
            other => bail!("Unknown client frame tag {other:#04x}"),
        };
        Ok(Some(frame))
    }
}

impl ServerFrame {
    pub fn write_to<W: Write>(&self, w: &mut W) -> Result<()> {
        match self {
            ServerFrame::Output(bytes) => write_frame(w, TAG_OUTPUT, bytes),
            ServerFrame::Exited(code) => write_frame(w, TAG_EXITED, &code.to_be_bytes()),
            ServerFrame::Busy => write_frame(w, TAG_BUSY, &[]),
            ServerFrame::Attached => write_frame(w, TAG_ATTACHED, &[]),
        }
    }

    /// Read the next frame, or `None` once the daemon has closed the socket.
    pub fn read_from<R: Read>(r: &mut R) -> Result<Option<Self>> {
        let Some((tag, payload)) = read_frame(r)? else {
            return Ok(None);
        };
        let frame = match tag {
            TAG_OUTPUT => ServerFrame::Output(payload),
            TAG_EXITED => {
                if payload.len() < 4 {
                    bail!("Exited frame is truncated");
                }
                ServerFrame::Exited(i32::from_be_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]))
            }
            TAG_BUSY => ServerFrame::Busy,
            TAG_ATTACHED => ServerFrame::Attached,
            other => bail!("Unknown server frame tag {other:#04x}"),
        };
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_frames_round_trip() {
        let frames = vec![
            ClientFrame::Attach {
                cols: 120,
                rows: 40,
                term: "xterm-256color".to_string(),
                caps: ClientCaps {
                    kitty: true,
                    via_ssh: false,
                    vs16_wide: true,
                },
            },
            ClientFrame::Input(vec![0x1b, b'[', b'A']),
            ClientFrame::Resize { cols: 80, rows: 24 },
            ClientFrame::Detach,
            ClientFrame::RequestDetach,
        ];

        let mut buf = Vec::new();
        for frame in &frames {
            frame.write_to(&mut buf).unwrap();
        }

        let mut cursor = std::io::Cursor::new(buf);
        for expected in &frames {
            let got = ClientFrame::read_from(&mut cursor).unwrap().unwrap();
            assert_eq!(&got, expected);
        }
        assert_eq!(ClientFrame::read_from(&mut cursor).unwrap(), None);
    }

    #[test]
    fn server_frames_round_trip() {
        let frames = vec![
            ServerFrame::Output(b"hello".to_vec()),
            ServerFrame::Exited(-1),
            ServerFrame::Busy,
            ServerFrame::Attached,
        ];

        let mut buf = Vec::new();
        for frame in &frames {
            frame.write_to(&mut buf).unwrap();
        }

        let mut cursor = std::io::Cursor::new(buf);
        for expected in &frames {
            let got = ServerFrame::read_from(&mut cursor).unwrap().unwrap();
            assert_eq!(&got, expected);
        }
        assert_eq!(ServerFrame::read_from(&mut cursor).unwrap(), None);
    }

    // A half-written frame is what a killed client leaves behind; it must read
    // as end-of-stream rather than propagating an error that would look like a
    // protocol bug in the log.
    #[test]
    fn a_truncated_frame_reads_as_end_of_stream() {
        let mut buf = Vec::new();
        ClientFrame::Input(vec![1, 2, 3, 4])
            .write_to(&mut buf)
            .unwrap();
        buf.truncate(6);

        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(ClientFrame::read_from(&mut cursor).unwrap(), None);
    }

    #[test]
    fn client_capabilities_survive_the_wire() {
        for caps in [
            ClientCaps::default(),
            ClientCaps {
                kitty: true,
                via_ssh: false,
                vs16_wide: false,
            },
            ClientCaps {
                kitty: false,
                via_ssh: true,
                vs16_wide: false,
            },
            ClientCaps {
                kitty: true,
                via_ssh: true,
                vs16_wide: true,
            },
        ] {
            let mut buf = Vec::new();
            ClientFrame::Attach {
                cols: 80,
                rows: 24,
                term: "screen".to_string(),
                caps,
            }
            .write_to(&mut buf)
            .unwrap();

            let mut cursor = std::io::Cursor::new(buf);
            let Some(ClientFrame::Attach { caps: got, .. }) =
                ClientFrame::read_from(&mut cursor).unwrap()
            else {
                panic!("expected an Attach frame");
            };
            assert_eq!(got, caps);
        }
    }

    #[test]
    fn an_oversized_length_is_refused_without_allocating() {
        let mut buf = vec![TAG_INPUT];
        buf.extend_from_slice(&u32::MAX.to_be_bytes());

        let mut cursor = std::io::Cursor::new(buf);
        assert!(ClientFrame::read_from(&mut cursor).is_err());
    }
}
