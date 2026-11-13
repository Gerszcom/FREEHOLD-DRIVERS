//! Stage 5 groundwork — planning the GSP shared-memory carveout.
//!
//! Once a card's GSP firmware is verified ([`crate::firmware`]) and started, the
//! host and the GSP talk over a **pair** of ring buffers in one region of memory
//! both of them can see: a command queue the host fills and the GSP drains, and a
//! status queue the reverse ([`crate::msgq`]). That region — the *carveout* — is
//! a single block of system memory the host allocates and maps for the GSP (on
//! FREEHOLD, through the Stage-3 `DRIVER_MAP` kernel call; `ROADMAP.md`). Before
//! it can be allocated, two things have to be worked out: **how big** it must be,
//! and **where inside it each queue sits**.
//!
//! The transport below ([`crate::msgq`]) already takes the *where* as data — a
//! [`QueueLayout`] per queue, with the byte offsets of the two index words and
//! the ring — and [`crate::channel::rings_disjoint`] already *checks* that two
//! such layouts do not overlap. What is missing, and what this module adds, is
//! the step before both: given each queue's own internal shape and the alignment
//! its storage needs, **compute** the two absolute [`QueueLayout`]s packed into
//! one region and the region's total size, so the two queues are disjoint *by
//! construction* and the caller has one number to hand the allocator.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Packing several aligned sub-regions end to end inside one block, summing their
//! sizes and rounding to an alignment is ordinary arithmetic — correct or
//! incorrect on any machine — so it is written and proven here with no hardware
//! caveat, exactly as the ring ([`crate::msgq`]) and the SHA-256
//! ([`crate::sha256`]) are. The planner *consumes* firmware-dictated data (each
//! queue's internal offsets and slot geometry, and the alignment a real GSP
//! requires of its queue storage); it *invents* none. Which concrete values a
//! real card uses stays the firmware's to supply and a card's to confirm
//! (`validation/stage-5/`); this module only lays out whatever geometry it is
//! handed, and refuses a geometry that is internally inconsistent or that
//! overflows the address arithmetic.
//!
//! Clean-room: deciding where to place two buffers in a block of memory FREEHOLD
//! itself allocates is FREEHOLD's own code; no driver source is copied or
//! translated. The only NVIDIA-specific facts this will eventually need — a
//! queue's internal offsets, its slot size and count, and the required
//! alignment — are a documented interface, recorded as data in a [`QueueRequest`]
//! and a [`QueueLayout`], not lifted as code.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: the planner holds no memory
//! of its own, only a description of where memory should go. It composes with
//! [`crate::channel::rings_disjoint`] (which it re-checks the result against) and
//! feeds the two layouts straight into an [`crate::rpc::RpcEndpoint`]'s command
//! and status [`Framer`](crate::frame::Framer)s once the region is mapped.

use crate::channel::{ring_extent, rings_disjoint};
use crate::msgq::{QueueError, QueueLayout};

/// One queue's contribution to the carveout: its internal shape and the
/// alignment its storage base must satisfy.
///
/// `layout` carries the queue's offsets **relative to the queue's own base** —
/// where its write index, read index and ring sit measured from byte 0 of the
/// space this queue is given, not from the start of the whole region. The
/// planner chooses each queue's base within the region and adds it to these
/// relative offsets to produce the absolute [`QueueLayout`] the transport uses.
///
/// `align` is the alignment (a power of two, in bytes) the queue's base must
/// meet — the GSP may require its queue storage aligned to a cache line or a
/// page; that requirement is firmware data, supplied here, never baked in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueRequest {
    /// The queue's layout with offsets measured from the queue's own base
    /// (`0`). Validated by the planner via [`QueueLayout::validate`].
    pub layout: QueueLayout,
    /// Required alignment of the queue's base within the region, in bytes. Must
    /// be a non-zero power of two.
    pub align: usize,
}

impl QueueRequest {
    /// The number of bytes this queue occupies measured from its own base: the
    /// highest end of its three parts (the two index words and the ring). This
    /// is the span the planner reserves for the queue; any gap below the lowest
    /// part is reserved too, so the queue owns a single contiguous `[base,
    /// base + footprint)` extent.
    fn footprint(&self) -> usize {
        // `ring_extent` returns `(start, end)` relative to base 0; we reserve
        // `[0, end)` so the queue owns one contiguous run from its base.
        ring_extent(&self.layout).1
    }
}

/// The planned placement of the command and status queues within one carveout.
///
/// `command` and `status` are **absolute** [`QueueLayout`]s — their offsets are
/// measured from the start of the mapped region — ready to hand to the
/// transport. They are guaranteed disjoint ([`crate::channel::rings_disjoint`]).
/// `total_len` is the number of bytes the region must span, rounded up to the
/// region alignment; it is what the Stage-3 `DRIVER_MAP` call is asked to map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Carveout {
    /// The command queue's absolute layout (host writes, GSP drains).
    pub command: QueueLayout,
    /// The status queue's absolute layout (GSP writes, host drains).
    pub status: QueueLayout,
    /// Total bytes the carveout must span, rounded up to the region alignment.
    pub total_len: usize,
}

impl Carveout {
    /// Whether this plan fits within a region of `region_len` bytes.
    ///
    /// The kernel may hand back a region rounded up to whole pages, so a plan
    /// whose `total_len` is at most the allocated size fits.
    pub fn fits(&self, region_len: usize) -> bool {
        self.total_len <= region_len
    }
}

/// Why a carveout could not be planned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarveoutError {
    /// A queue's relative [`QueueLayout`] is not internally consistent (zero
    /// slot size or count, or too many slots) — the error
    /// [`QueueLayout::validate`] returned, tagged with which queue.
    BadQueue {
        /// Which queue failed validation.
        which: Queue,
        /// The underlying layout error.
        err: QueueError,
    },
    /// An alignment was zero or not a power of two. Every alignment (each
    /// queue's base and the whole region) must be a non-zero power of two.
    BadAlign {
        /// Which alignment was rejected.
        which: Align,
        /// The offending value.
        value: usize,
    },
    /// The placement arithmetic overflowed a `usize`: a queue footprint, an
    /// alignment round-up or the running cursor ran past the address space. A
    /// real carveout is far smaller than this; it guards a caller that passed a
    /// pathological geometry.
    Overflow,
    /// Internal invariant check failed: the two planned layouts overlap despite
    /// being placed end to end. This cannot happen for valid inputs and is
    /// surfaced rather than asserted so the planner never returns a plan whose
    /// queues could corrupt each other.
    Overlap,
}

/// Which queue an error refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Queue {
    /// The command queue (host → GSP).
    Command,
    /// The status queue (GSP → host).
    Status,
}

/// Which alignment an error refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    /// The command queue's base alignment.
    Command,
    /// The status queue's base alignment.
    Status,
    /// The whole region's alignment.
    Region,
}

/// Round `value` up to the next multiple of `align`, which must be a power of
/// two. Returns `None` on overflow.
fn align_up(value: usize, align: usize) -> Option<usize> {
    // align is a power of two, checked by the caller.
    let mask = align - 1;
    value.checked_add(mask).map(|v| v & !mask)
}

/// Whether `n` is a non-zero power of two.
fn is_pow2(n: usize) -> bool {
    n != 0 && (n & (n - 1)) == 0
}

/// Shift a queue's relative layout by `base`, producing the absolute layout, or
/// `None` if any offset addition overflows.
fn shift(layout: &QueueLayout, base: usize) -> Option<QueueLayout> {
    Some(QueueLayout {
        write_index_off: layout.write_index_off.checked_add(base)?,
        read_index_off: layout.read_index_off.checked_add(base)?,
        ring_off: layout.ring_off.checked_add(base)?,
        slot_size: layout.slot_size,
        slot_count: layout.slot_count,
    })
}

/// Plan a carveout holding the command and status queues, packed end to end.
///
/// Both queues' relative layouts are validated, each queue is placed at the next
/// position meeting its alignment (command first, then status after the command
/// queue's footprint), and the region is sized to the end of the status queue
/// rounded up to `region_align`. The two absolute layouts are disjoint by
/// construction and re-checked against [`crate::channel::rings_disjoint`] before
/// the plan is returned.
///
/// `region_align` is the alignment of the whole carveout's base (typically a
/// page); the returned [`Carveout::total_len`] is a whole multiple of it, so a
/// region mapped at any `region_align`-aligned base holds the plan at the
/// absolute offsets given.
///
/// # Errors
///
/// - [`CarveoutError::BadQueue`] if either relative layout fails
///   [`QueueLayout::validate`].
/// - [`CarveoutError::BadAlign`] if any alignment is zero or not a power of two.
/// - [`CarveoutError::Overflow`] if the placement arithmetic overflows a
///   `usize`.
/// - [`CarveoutError::Overlap`] only if the end-to-end placement somehow
///   produced overlapping layouts (never for valid inputs).
pub fn plan(
    command: &QueueRequest,
    status: &QueueRequest,
    region_align: usize,
) -> Result<Carveout, CarveoutError> {
    command
        .layout
        .validate()
        .map_err(|err| CarveoutError::BadQueue {
            which: Queue::Command,
            err,
        })?;
    status
        .layout
        .validate()
        .map_err(|err| CarveoutError::BadQueue {
            which: Queue::Status,
            err,
        })?;

    if !is_pow2(command.align) {
        return Err(CarveoutError::BadAlign {
            which: Align::Command,
            value: command.align,
        });
    }
    if !is_pow2(status.align) {
        return Err(CarveoutError::BadAlign {
            which: Align::Status,
            value: status.align,
        });
    }
    if !is_pow2(region_align) {
        return Err(CarveoutError::BadAlign {
            which: Align::Region,
            value: region_align,
        });
    }
    // Offsets are aligned within the region; for that to imply an aligned
    // *address*, the region (mapped at a `region_align`-aligned base) must be
    // aligned at least as strongly as either queue needs. With all three powers
    // of two, that is exactly `region_align` being a multiple of each queue's
    // alignment. A region too weakly aligned to honour a queue is rejected here
    // rather than silently producing an offset whose absolute address is not
    // aligned.
    if !region_align.is_multiple_of(command.align) || !region_align.is_multiple_of(status.align) {
        return Err(CarveoutError::BadAlign {
            which: Align::Region,
            value: region_align,
        });
    }

    // Command queue: placed at the first command-aligned offset at or after 0.
    let cmd_base = align_up(0, command.align).ok_or(CarveoutError::Overflow)?;
    let cmd_abs = shift(&command.layout, cmd_base).ok_or(CarveoutError::Overflow)?;
    let cmd_end = cmd_base
        .checked_add(command.footprint())
        .ok_or(CarveoutError::Overflow)?;

    // Status queue: placed at the first status-aligned offset at or after the
    // command queue's end, so the two extents cannot meet.
    let status_base = align_up(cmd_end, status.align).ok_or(CarveoutError::Overflow)?;
    let status_abs = shift(&status.layout, status_base).ok_or(CarveoutError::Overflow)?;
    let status_end = status_base
        .checked_add(status.footprint())
        .ok_or(CarveoutError::Overflow)?;

    let total_len = align_up(status_end, region_align).ok_or(CarveoutError::Overflow)?;

    // Guaranteed by placing status after the command queue's footprint; checked
    // so the planner never hands back a plan whose queues could overlap.
    if !rings_disjoint(&cmd_abs, &status_abs) {
        return Err(CarveoutError::Overlap);
    }

    Ok(Carveout {
        command: cmd_abs,
        status: status_abs,
        total_len,
    })
}
