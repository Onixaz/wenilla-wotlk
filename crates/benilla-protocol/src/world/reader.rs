use anyhow::{anyhow, Result};

use crate::messages::{self, ServerPacket};
use crate::transport::{ConnReader, ReadExactAsync};

use super::cipher::{read_server_header, Decrypter};
use super::recv_packet;

/// The read half of a split [`WorldSession`](super::WorldSession): the connection's read half and
/// the decrypter; it lives on the network thread (native) or in the sequencer task (web),
/// and what the connection learned before the split, for
/// [`WorldSession::rejoin`](super::WorldSession::rejoin).
pub struct WorldReader {
    pub(super) reader: ConnReader,
    pub(super) decrypter: Decrypter,
    pub(super) link: super::session::LinkFacts,
}

impl WorldReader {
    /// Read + decrypt one server packet (blocking) — the native twin of [`Self::recv_async`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn recv(&mut self) -> Result<ServerPacket> {
        futures_lite::future::block_on(self.recv_async())
    }

    /// Read + decrypt one server packet.
    pub async fn recv_async(&mut self) -> Result<ServerPacket> {
        recv_packet(&mut self.reader, Some(&mut self.decrypter)).await
    }

    /// [`Self::poll_async`], blocking — the native net thread's read.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn poll(&mut self) -> Result<crate::Poll> {
        futures_lite::future::block_on(self.poll_async())
    }

    /// Read one packet and decode it into a [`crate::Poll`]; errors only when the socket fails. The
    /// whole body is read before parsing, so an unparseable packet is skipped, the stream aligned.
    pub async fn poll_async(&mut self) -> Result<crate::Poll> {
        let (body_len, opcode) = read_server_header(&mut self.reader, Some(&mut self.decrypter))
            .await
            .map_err(|e| anyhow!("world stream closed: {e}"))?;
        let mut body = vec![0u8; body_len];
        if let Err(e) = self.reader.read_exact_async(&mut body).await {
            return Err(anyhow!("world stream closed: {e}"));
        }
        match messages::parse_server_with_tail(opcode, &body) {
            Ok((packet, tail)) => {
                let mut events = crate::decode(packet);
                // The answer as received, ahead of what it decoded to, for the client's caches.
                if crate::PERSISTED_RESPONSES.contains(&opcode) {
                    events.insert(0, crate::SessionEvent::QueryAnswer { opcode, body });
                }
                Ok(crate::Poll::Events {
                    opcode,
                    events,
                    tail,
                })
            }
            Err(e) => Ok(crate::Poll::Skipped {
                opcode,
                reason: format!(
                    "opcode {opcode:#06x} ({}): {e} [{body_len}B: {}]",
                    messages::opcode_name(opcode).unwrap_or("?"),
                    hex_preview(&body, 64)
                ),
            }),
        }
    }
}

/// Hex of the first `max` bytes of `body`, `…` when truncated, for decoding a packet by hand.
fn hex_preview(body: &[u8], max: usize) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for b in body.iter().take(max) {
        let _ = write!(s, "{b:02x} ");
    }
    if body.len() > max {
        s.push('…');
    }
    s.trim_end().to_string()
}
