//! Stage 5 groundwork — establishing the GSP channel over a planned carveout.
//!
//! [`crate::carveout`] answers *where* the two queues sit: given each queue's
//! internal shape and alignment it packs them into one region and reports the
//! region's size and the two absolute [`QueueLayout`]s. [`crate::rpc`] is the
//! conversation that runs *over* those two queues: an [`RpcEndpoint`] pairs a
//! command [`Framer`] and a status [`Framer`] under one [`RpcLayout`] envelope.
//! Between the two there is a step nothing yet performs: taking the carveout
//! plan plus the firmware's frame/envelope geometry and a freshly mapped region
//! and producing the **ready, validated, initialised** endpoint — the queues
//! zeroed to their empty start state and every cross-layer invariant checked
//! *before* the GSP is ever signalled. This module is that step.
//!
//! On FREEHOLD this is what `NVRTX.EXE` runs immediately after the Stage-3
//! `DRIVER_MAP` kernel call hands back the mapped shared region: it already
//! holds a [`Carveout`] (from [`crate::carveout::plan`]) and the firmware's
//! [`ChannelGeometry`]; [`establish`] turns those and the mapped region into the
//! [`RpcEndpoint`] the client ([`crate::client::RpcClient`]) and the channel
//! health-check ([`crate::health`]) drive.
//!
//! ## What it checks that no single layer does
//!
//! Each layer beneath validates *itself*: [`QueueLayout::validate`] that a ring
//! is internally consistent, [`Framer::validate`] that a header fits a slot,
//! [`RpcLayout::validate`] that the envelope words fit the envelope header. What
//! none of them checks is that the pieces fit *each other and the region*:
//!
//! * the carveout actually fits the region the kernel mapped
//!   ([`EstablishError::RegionTooSmall`]);
//! * the two rings are disjoint within that region
//!   ([`EstablishError::RingsOverlap`] — guaranteed by [`crate::carveout::plan`]
//!   but re-checked here so a hand-built [`Carveout`] cannot slip an overlapping
//!   pair through);
//! * the RPC envelope header fits in a single message on *each* ring — a ring so
//!   small that even a zero-argument call could not be framed is caught here
//!   ([`EstablishError::EnvelopeExceedsCapacity`]), not at the first failed send.
//!
//! Gathering those checks in one place means a caller learns a geometry is
//! unworkable from the plan and the geometry alone, before any doorbell is rung
//! against a half-built queue.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Composing validated layouts, comparing a planned size against a mapped size
//! and zeroing two pairs of index words is ordinary logic — correct or incorrect
//! on any machine — so it is written and proven here against a
//! [`MockSharedMem`](crate::mock::MockSharedMem), with no pending-hardware
//! caveat, like the ring, the framing and the carveout planner beneath it. The
//! firmware-dictated data stays data: the frame and envelope offsets and the
//! transport checksum arrive in a [`ChannelGeometry`], the queue placement in a
//! [`Carveout`]; this module invents none of them. What only a card settles is
//! unchanged from the layers it composes — the concrete offsets, the checksum
//! the real GSP uses, and that a real mapped region behaves as the mock does
//! (`validation/stage-5/`).
//!
//! Clean-room: deciding that a set of FREEHOLD-chosen layouts are mutually
//! consistent and zeroing the indices of queues FREEHOLD itself allocated is
//! FREEHOLD's own code; no driver source is copied or translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: [`establish`] builds an
//! [`RpcEndpoint`] (which owns only layout descriptions, no buffers) and writes
//! four index words into the caller's region; it holds no memory of its own.

use crate::carveout::Carveout;
use crate::channel::rings_disjoint;
use crate::frame::{FrameLayout, Framer};
use crate::msgq::{init, QueueError, SharedMem};
use crate::rpc::{RpcEndpoint, RpcLayout};

/// The firmware-dictated wire geometry a channel needs *above* the carveout.
///
/// The carveout fixes where each queue sits; this fixes how a message is shaped
/// within a queue. All three fields are data supplied by whoever knows the GSP's
/// published message format — the per-message [`FrameLayout`] (where the frame
/// header words sit in a message's first slot), the [`RpcLayout`] envelope
/// (where the RPC header words sit in the framed payload), and the `checksum`
/// both ends compute over a payload. The two framers share one `frame` layout
/// and one `checksum`, exactly as a real command/status pair does.
///
/// A reference [`checksum32`](crate::frame::checksum32) ships for
/// FREEHOLD-to-FREEHOLD transport and host tests; the GSP's own algorithm is
/// plugged in here once confirmed (`validation/stage-5/`).
#[derive(Clone, Copy)]
pub struct ChannelGeometry {
    /// Where the per-message frame header sits in a message's first slot. Shared
    /// by the command and status framers.
    pub frame: FrameLayout,
    /// Where the RPC envelope header sits in a framed message's payload.
    pub envelope: RpcLayout,
    /// The checksum both ends compute over a payload. See
    /// [`checksum32`](crate::frame::checksum32).
    pub checksum: fn(&[u8]) -> u32,
}

/// Which ring an [`EstablishError`] refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ring {
    /// The command ring (host → GSP).
    Command,
    /// The status ring (GSP → host).
    Status,
}

/// Why a channel could not be established from a plan and a geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EstablishError {
    /// The carveout plan does not fit the region the kernel mapped: the plan
    /// needs [`needed`](EstablishError::RegionTooSmall::needed) bytes but only
    /// [`got`](EstablishError::RegionTooSmall::got) were mapped.
    RegionTooSmall {
        /// Bytes the plan spans ([`Carveout::total_len`]).
        needed: usize,
        /// Bytes the mapped region has.
        got: usize,
    },
    /// A ring plus the frame layout is not a self-consistent framer (a bad ring
    /// layout, a frame header that does not fit a slot, or overlapping header
    /// words). `which` names the ring; the underlying check is
    /// [`Framer::validate`].
    BadFramer {
        /// Which ring failed to validate.
        which: Ring,
    },
    /// The RPC envelope layout is not internally consistent (a header word that
    /// does not fit `header_len`, or two words that overlap). The underlying
    /// check is [`RpcLayout::validate`].
    BadEnvelope,
    /// The two rings' byte extents overlap within the region, so one direction's
    /// slots or index word could corrupt the other's. [`crate::carveout::plan`]
    /// guarantees disjoint rings; this catches a hand-built [`Carveout`] that
    /// does not.
    RingsOverlap,
    /// The RPC envelope header is larger than a single message can carry on
    /// `which` ring: even a zero-argument call needs
    /// [`header_len`](EstablishError::EnvelopeExceedsCapacity::header_len)
    /// payload bytes, but the ring can carry at most
    /// [`capacity`](EstablishError::EnvelopeExceedsCapacity::capacity). The
    /// channel could never send its smallest message, so it is rejected now
    /// rather than at the first send.
    EnvelopeExceedsCapacity {
        /// Which ring is too small.
        which: Ring,
        /// Bytes the envelope header occupies ([`RpcLayout::header_len`]).
        header_len: usize,
        /// The most payload bytes one message can carry on this ring
        /// ([`Framer::capacity`]).
        capacity: usize,
    },
}

/// Build a validated, initialised [`RpcEndpoint`] over a mapped carveout.
///
/// `carveout` is the plan from [`crate::carveout::plan`] (or an equivalent
/// hand-built one); `region_len` is the size of the region the kernel actually
/// mapped (on the host, a [`MockSharedMem`](crate::mock::MockSharedMem)'s
/// `len`); `geom` carries the firmware's frame and envelope offsets and the
/// transport checksum. On success the two queues in `mem` are reset to empty
/// (both index words zeroed) and the returned endpoint is ready for a
/// [`RpcClient`](crate::client::RpcClient) or the
/// [`health`](crate::health) check.
///
/// The steps, in order, so the first inconsistency is the one reported:
///
/// 1. the plan fits the mapped region ([`EstablishError::RegionTooSmall`]);
/// 2. each ring + the frame layout is a valid [`Framer`]
///    ([`EstablishError::BadFramer`]);
/// 3. the envelope layout is self-consistent ([`EstablishError::BadEnvelope`]);
/// 4. the two rings are disjoint ([`EstablishError::RingsOverlap`]);
/// 5. the envelope header fits one message on each ring
///    ([`EstablishError::EnvelopeExceedsCapacity`]).
///
/// Only once all five hold are the queues initialised and the endpoint
/// returned — so a rejected geometry never leaves the region half-written.
///
/// # Errors
///
/// Returns the first [`EstablishError`] above whose check fails; `mem` is left
/// untouched on every error (the index words are zeroed only after all checks
/// pass).
pub fn establish<M: SharedMem>(
    mem: &mut M,
    carveout: &Carveout,
    region_len: usize,
    geom: &ChannelGeometry,
) -> Result<RpcEndpoint, EstablishError> {
    // 1. The plan must fit the region the kernel mapped.
    if !carveout.fits(region_len) {
        return Err(EstablishError::RegionTooSmall {
            needed: carveout.total_len,
            got: region_len,
        });
    }

    // 2. Each ring, paired with the frame layout and checksum, must be a valid
    //    framer. Build them now so capacity (step 5) can be measured.
    let command = Framer::with_checksum(carveout.command, geom.frame, geom.checksum);
    let status = Framer::with_checksum(carveout.status, geom.frame, geom.checksum);
    command.validate().map_err(|_| EstablishError::BadFramer {
        which: Ring::Command,
    })?;
    status.validate().map_err(|_| EstablishError::BadFramer {
        which: Ring::Status,
    })?;

    // 3. The envelope layout must be self-consistent.
    geom.envelope
        .validate()
        .map_err(|_| EstablishError::BadEnvelope)?;

    // 4. The two rings must own disjoint bytes within the region.
    if !rings_disjoint(&carveout.command, &carveout.status) {
        return Err(EstablishError::RingsOverlap);
    }

    // 5. The smallest possible message — a zero-argument envelope, `header_len`
    //    bytes — must fit a single framed message on each ring, or the channel
    //    could never carry even a keepalive.
    let header_len = geom.envelope.header_len;
    let cmd_cap = command.capacity();
    if header_len > cmd_cap {
        return Err(EstablishError::EnvelopeExceedsCapacity {
            which: Ring::Command,
            header_len,
            capacity: cmd_cap,
        });
    }
    let status_cap = status.capacity();
    if header_len > status_cap {
        return Err(EstablishError::EnvelopeExceedsCapacity {
            which: Ring::Status,
            header_len,
            capacity: status_cap,
        });
    }

    // All checks passed: bring both queues to their empty start state. `init`
    // re-validates each layout, which cannot fail here (the framers validated
    // the same layouts above); a defensive mapping keeps the function total.
    init(&carveout.command, mem).map_err(map_init(Ring::Command))?;
    init(&carveout.status, mem).map_err(map_init(Ring::Status))?;

    Ok(RpcEndpoint::new(command, status, geom.envelope))
}

/// Map a late `init` validation failure back to the ring it came from. Only a
/// layout that already passed [`Framer::validate`] reaches `init`, so this never
/// fires for inputs that got this far; it keeps [`establish`] total without a
/// panic.
fn map_init(which: Ring) -> impl Fn(QueueError) -> EstablishError {
    move |_| EstablishError::BadFramer { which }
}
