//! Stage 5 groundwork — the GSP doorbell and command-side signalling.
//!
//! [`crate::msgq`] is a ring, [`crate::frame`] carries variable-length messages
//! over it, and [`crate::rpc`] gives those messages the meaning of a
//! remote-procedure call. All three are *polling*: the host advances a write
//! index and the GSP, some time later, notices. A **doorbell** is how the host
//! says *now* — on hardware, a write to a register the GSP watches, which wakes
//! the firmware to drain the command queue. This module is that doorbell: the
//! trait a driver rings, and the one piece of discipline that is worth settling
//! off-hardware — *when* to ring it.
//!
//! ## Ring only on a real publish
//!
//! The command queue is single-producer: only the host writes it, and it writes
//! a whole frame ([`crate::frame::Framer::send`]) or nothing at all (a refused
//! send leaves the queue untouched). The doorbell must follow suit — ring once
//! the frame is published, and **never** when the send was refused
//! ([`crate::frame::FrameError::WouldBlock`] because the ring is full,
//! [`TooLarge`](crate::frame::FrameError::TooLarge) because the payload cannot
//! fit, or a bad layout). A doorbell rung for a command that was not posted
//! wakes the GSP to find nothing, or — worse, against a half-built queue —
//! races it. [`CommandChannel::send_command`] expresses exactly that rule: the
//! `?` on the send returns before the ring on every error path, so the bell is
//! rung if and only if a frame reached the queue.
//!
//! This pairs with [`crate::rpc`]: that layer builds and correlates the *bytes*
//! of a command; this one signals the GSP once those bytes are on the ring. On
//! hardware the two meet — an [`crate::rpc::RpcEndpoint`] encodes a request, the
//! command framer publishes it, and a doorbell wakes the GSP — with the concrete
//! register supplied then (see below).
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Deciding *when* to ring a bell is ordinary logic — correct or incorrect on
//! any machine — so it is written and proven here against
//! [`crate::mock::MockSharedMem`] and a [`crate::mock::MockDoorbell`] that merely
//! counts rings, with no pending-hardware caveat, like the ring, the framing and
//! the RPC envelope. What only a card can fix is the *concrete* doorbell: the
//! register offset the host writes to wake the GSP and the value it writes (and,
//! in the other direction, the interrupt by which the GSP wakes the host for
//! status). Those are supplied by the hardware [`Doorbell`] implementation over
//! FREEHOLD's BAR0 accessor, recorded and confirmed under `validation/stage-5/`;
//! the trait and the ringing discipline above do not depend on them.
//!
//! This module also carries [`rings_disjoint`]: a command ring and a status ring
//! live side by side in one shared region (as an [`crate::rpc::RpcEndpoint`]
//! holds both), and they must own disjoint bytes, or one direction's slots or
//! index would corrupt the other's. Checking two byte extents do not intersect
//! is pure arithmetic, settled and tested here.
//!
//! Clean-room: the doorbell discipline and the extent check are FREEHOLD's own
//! code; the only NVIDIA-specific fact this path will ever need — the doorbell
//! register — enters through the supplied [`Doorbell`], never as lifted code.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: a [`CommandChannel`] owns no
//! buffer, only a [`Framer`] and a doorbell; every message is carried in
//! caller-provided slices and the [`crate::msgq::SharedMem`] region.

use crate::frame::{FrameError, FrameHeader, Framer};
use crate::msgq::{free_slots, QueueLayout, SharedMem};

/// A signal to the GSP that a command is waiting on the command queue.
///
/// On hardware the host's implementation writes the GSP's doorbell register
/// (through FREEHOLD's BAR0 accessor) so the firmware wakes and drains the
/// queue; the concrete register offset and written value are hardware facts
/// recorded under `validation/stage-5/`. On the host it is
/// [`crate::mock::MockDoorbell`], which counts rings so a test can assert the
/// bell was rung exactly when a command was published and never when a send was
/// refused. The trait is deliberately minimal — *signal the peer* — because the
/// only thing the transport logic needs to settle is **when** to signal, not how.
pub trait Doorbell {
    /// Tell the GSP that a command is waiting.
    fn signal(&mut self);
}

/// A doorbell that does nothing — for a caller that drives the queue purely by
/// polling and has no peer to wake (host tests of the queue logic, or a
/// bring-up path before the real doorbell register is wired).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NullDoorbell;

impl Doorbell for NullDoorbell {
    fn signal(&mut self) {}
}

/// The host's command ring paired with the doorbell it rings to wake the GSP.
///
/// The command ring is the host→GSP direction (the host is its producer); the
/// status ring the GSP posts replies on is drained by polling and carries no
/// host-side doorbell (the GSP signals the host by interrupt instead), so it is
/// not held here. `command` is the same [`Framer`] an
/// [`crate::rpc::RpcEndpoint`] uses for its command direction; this wraps it
/// with the signalling the RPC layer leaves out.
pub struct CommandChannel<D: Doorbell> {
    /// The host→GSP ring the host posts commands on.
    pub command: Framer,
    /// Rung after a command is published, to wake the GSP.
    pub doorbell: D,
}

impl<D: Doorbell> CommandChannel<D> {
    /// Build a command channel over a command framer and a doorbell.
    ///
    /// The framer layout is not checked here; call [`validate`](Self::validate)
    /// once before use.
    pub fn new(command: Framer, doorbell: D) -> Self {
        CommandChannel { command, doorbell }
    }

    /// Check the command framer's ring and header layouts are usable. See
    /// [`Framer::validate`].
    pub fn validate(&self) -> Result<(), FrameError> {
        self.command.validate()
    }

    /// Publish a command of `payload` bytes with sequence number `seq`, then
    /// ring the GSP's doorbell.
    ///
    /// The doorbell is rung **only** once [`Framer::send`] has published the
    /// whole frame; on any send error (`?` returns first) it is left un-rung, so
    /// the GSP is never woken for a command that was not posted. Returns the
    /// published [`FrameHeader`] on success, or the send's [`FrameError`]
    /// unchanged (the command queue is left untouched in that case).
    pub fn send_command<M: SharedMem>(
        &mut self,
        mem: &mut M,
        seq: u32,
        payload: &[u8],
    ) -> Result<FrameHeader, FrameError> {
        let header = self.command.send(mem, seq, payload)?;
        self.doorbell.signal();
        Ok(header)
    }

    /// How many command slots are free right now — the back-pressure a caller
    /// checks before a large [`send_command`](Self::send_command).
    pub fn command_free<M: SharedMem>(&self, mem: &M) -> u32 {
        free_slots(&self.command.queue, mem)
    }
}

/// The half-open byte range `[start, end)` a ring occupies in a shared region:
/// from the lowest of its two index words and its ring base, to the highest end
/// of the same three.
///
/// This is the span another ring in the same region must not intersect. The
/// index words may sit anywhere, so the extent is taken over all three parts
/// rather than assumed contiguous.
pub fn ring_extent(q: &QueueLayout) -> (usize, usize) {
    let start = q.write_index_off.min(q.read_index_off).min(q.ring_off);
    let end = (q.write_index_off + 4)
        .max(q.read_index_off + 4)
        .max(q.ring_off + q.ring_len());
    (start, end)
}

/// Whether two rings sharing one region occupy disjoint bytes.
///
/// A command ring and a status ring live side by side in one shared region (an
/// [`crate::rpc::RpcEndpoint`] holds both); if their byte extents
/// ([`ring_extent`]) overlapped, one direction's slots or index word could
/// corrupt the other's. A caller checks this once when it lays the two rings out
/// in the region.
pub fn rings_disjoint(a: &QueueLayout, b: &QueueLayout) -> bool {
    let (a0, a1) = ring_extent(a);
    let (b0, b1) = ring_extent(b);
    // Two half-open ranges are disjoint unless each starts before the other ends.
    !(a0 < b1 && b0 < a1)
}
