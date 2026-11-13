//! # freehold-rtx
//!
//! Clean-room driver logic for NVIDIA GeForce RTX 20-50 GPUs (Turing,
//! Ampere, Ada, Blackwell) on the FREEHOLD operating system.
//!
//! This crate is written against the *documented, public* hardware
//! interface — PCI identity and the chip's own identification register —
//! and the published behaviour of NVIDIA's open kernel modules. No code is
//! copied or translated from any driver; FREEHOLD is clean-room
//! (`DIRECTION.md`), and so is this.
//!
//! It is `no_std`, allocation-free and `forbid(unsafe_code)`, so the exact
//! same logic runs inside a FREEHOLD user-mode driver and inside host unit
//! tests. Every hardware access goes through the [`Mmio`] trait, so each
//! code path here is exercised on the development machine against a
//! [`mock`] GPU before it ever reaches silicon.
//!
//! ## Why RTX 20 and newer
//!
//! From Turing on, these GPUs carry a GSP (GPU System Processor): a RISC-V
//! core on the card that runs most of the resource-manager logic as signed
//! firmware. A driver loads that firmware, starts it, and talks to it over
//! a message queue. This crate builds the ground a driver stands on before
//! that conversation begins — knowing which card it is and that the card is
//! alive — and the later stages (firmware load, boot, display) build up
//! from here. See `ROADMAP.md`.
//!
//! ## Stages implemented
//!
//! * **Stage 1 — identify.** Decide from PCI identity whether a device is
//!   an NVIDIA display GPU this driver claims, and name it when the model
//!   is catalogued. See [`identify`].
//! * **Stage 2 — probe.** Through the device's BAR0 register aperture, read
//!   `PMC_BOOT_0`, confirm the card responds, and decode the silicon
//!   (family, implementation, revision). See [`probe`].
//! * **Stage 3 (groundwork) — device description.** Parse the FREEHOLD
//!   `DRIVER_DEVICE` record a user-mode driver is handed, and select the
//!   memory BAR0 register aperture to be mapped before a probe. See
//!   [`device`]. The kernel's mapping call and the runtime's `unsafe` BAR0
//!   accessor live in FREEHOLD; this crate stays allocation-free and
//!   `forbid(unsafe_code)`.
//! * **Stage 4 — devinit / GFW boot-complete + VRAM.** Wait for the card's
//!   own boot firmware (GFW / devinit) to report complete, then read back the
//!   usable VRAM size it computed. Pure polling-and-decode logic over the
//!   `Mmio` trait. See [`boot`].
//! * **Stage 5 (groundwork) — GSP firmware selection + digest.** From a probe,
//!   derive which GSP firmware file a card needs, parse FREEHOLD's firmware
//!   sidecar (vendor / version / hash / licence), and decide whether a sidecar
//!   is the right, verifiable firmware to load. A from-scratch, `no_std`
//!   SHA-256 ([`sha256`]) then verifies the firmware bytes against the
//!   sidecar's digest ([`firmware::FirmwareSelection::verify`]) — the last
//!   host-side gate before the firmware is read and booted. See [`firmware`]
//!   and [`sha256`].
//! * **Bring-up orchestration.** Run the stages above *in order* — claim,
//!   probe, wait for boot, read VRAM, choose and verify firmware — into one
//!   typed outcome, degrading gracefully when a card that does not need the GSP
//!   for display has no firmware, and failing only when one that does is
//!   missing it. The host-verifiable core of `NVRTX.EXE`. See [`bringup`].
//! * **Stage 5 (groundwork) — GSP message-queue transport.** The
//!   single-producer / single-consumer ring buffer the host and the GSP use to
//!   pass commands and status once the firmware is booted: index arithmetic,
//!   full/empty accounting and the slot copy, over a [`msgq::SharedMem`] region
//!   and parameterised by a [`msgq::QueueLayout`] so the ring logic is settled
//!   on the host and the firmware's concrete wire offsets are filled in on
//!   hardware. See [`msgq`].
//! * **Stage 5 (groundwork) — GSP message framing.** The layer above the ring
//!   that carries *variable-length* messages over its fixed-size slots: split a
//!   payload across as many consecutive slots as it needs, stamp the first with
//!   a sequence / count / length / checksum header, publish and consume the
//!   whole run atomically, and reassemble and checksum-verify it on the far
//!   side. Pure splitting-and-reassembly over a [`msgq::SharedMem`] region,
//!   with the header offsets ([`frame::FrameLayout`]) and checksum
//!   ([`frame::Framer`]) supplied as data so the firmware's concrete format is
//!   filled in on hardware. See [`frame`].
//! * **Stage 5 (groundwork) — GSP RPC request/response.** The layer above the
//!   framing that gives a message *meaning*: an envelope (kind / function /
//!   request id / status, then arguments) over the command/status ring pair, and
//!   the fixed-capacity bookkeeping ([`rpc::Outstanding`]) that matches a reply
//!   back to the request it answers and tells an unsolicited event apart from a
//!   reply. Pure envelope codec and correlation over a [`msgq::SharedMem`]
//!   region, with the envelope's field offsets ([`rpc::RpcLayout`]) and the real
//!   function numbers supplied as data so the firmware's concrete format is
//!   filled in on hardware. See [`rpc`].
//! * **Stage 5 (groundwork) — GSP doorbell + command signalling.** The signal
//!   the host rings to wake the GSP once a command is on the ring:
//!   [`channel::CommandChannel::send_command`] publishes a command frame and
//!   *then* rings the [`channel::Doorbell`] — but only once the whole frame is
//!   published, never on a refused send — so the firmware is never woken for a
//!   command that was not posted. [`channel::rings_disjoint`] checks a command
//!   ring and a status ring laid out in one shared region do not overlap. The
//!   doorbell's concrete register is supplied on hardware. See [`channel`].
//! * **Stage 5 (groundwork) — typed wire serialization.** The layer that gives
//!   an RPC's opaque argument bytes a *shape*: a [`codec::Writer`] appends
//!   little-endian typed fields to a buffer and a [`codec::Reader`] consumes
//!   them back, both bounds-checked on every field, with [`codec::WireEncode`] /
//!   [`codec::WireDecode`] letting the function catalog's request and result
//!   structs (still to come) be written field-by-field rather than as
//!   hand-computed offsets. Packing integers into a byte run and reading them
//!   back is ordinary computer science, so this is host-verified with no
//!   hardware caveat; *which* fields each RPC carries stays the firmware's data.
//!   See [`codec`].
//! * **Stage 5 (groundwork) — typed RPC calls.** The bridge that joins the
//!   typed serializer to the envelope: an [`call::RpcCall`] ties a function
//!   number to the request type it takes and the result type it returns, and
//!   generic [`RpcEndpoint`](rpc::RpcEndpoint) methods encode a typed request,
//!   post it, encode a typed reply, and decode a received message's arguments
//!   back into a value — so a future function catalog is written as a set of
//!   typed entries rather than hand-assembled argument bytes. Composing an
//!   encoder, an envelope and a decoder is ordinary computer science, so this is
//!   host-verified with no hardware caveat; *which* functions exist and what
//!   they carry stays the firmware's data. See [`call`].
//! * **Stage 5 (groundwork) — RPC function catalog.** Where the concrete
//!   functions live: one [`call::RpcCall`] impl per call, naming its selector and
//!   its typed request and result. The machinery is host-proven, so the only
//!   firmware-dictated part of each entry is its *data* — the function number and
//!   the field layout — read from NVIDIA's published GSP interface and confirmed
//!   on a card. The first entries are FREEHOLD's own diagnostics, which depend on
//!   no vendor layout: [`catalog::Keepalive`] (a [`catalog::NoArgs`] null request
//!   and reply — the liveness probe) and [`catalog::Echo`] (a 32-bit cookie the
//!   far side returns unchanged — the loopback integrity check). Defining a
//!   function's shape and round-tripping it through the stack is ordinary
//!   computer science, so these are host-verified with no hardware caveat; the
//!   vendor functional calls join them as their layouts are confirmed. See
//!   [`catalog`].
//! * **Stage 5 (groundwork) — typed RPC events.** The event-direction analogue
//!   of the typed call layer: an [`event::RpcEvent`] ties a function number to
//!   the single typed payload a GSP→host *event* carries, and generic
//!   [`RpcEndpoint`](rpc::RpcEndpoint) methods post a typed event
//!   ([`emit_event`](rpc::RpcEndpoint::emit_event)) and decode a received message
//!   as a typed event if it is one ([`decode_event`](rpc::RpcEndpoint::decode_event),
//!   which returns `None` for a reply or a different event so event types chain
//!   cleanly). This closes the asymmetry where requests and replies were typed
//!   ([`call`]) but events were handled only as raw bytes. Tagging a byte run and
//!   reading the tag back to pick a decode type is ordinary computer science, so
//!   this is host-verified with no hardware caveat; *which* events a GSP posts and
//!   what they carry stays the firmware's data. See [`event`].
//! * **Stage 5 (groundwork) — RPC client transaction driver.** The host side of
//!   the conversation: an [`RpcClient`](client::RpcClient) owns the endpoint, the
//!   in-flight request ids and the id/sequence counters, so
//!   [`call`](client::RpcClient::call) issues a typed call under a fresh,
//!   non-colliding request id and [`pump`](client::RpcClient::pump) drains the
//!   status ring, routing each message to the call it answers (a
//!   [`Pump::Reply`](client::Pump)) or reporting it as an unsolicited event, and
//!   [`transact`](client::RpcClient::transact) packages that loop into one
//!   blocking typed call — post a request, drive the ring through a caller
//!   [`Responder`](client::Responder), and return the decoded reply or a
//!   [`TimedOut`](client::Transact).
//!   Handing out an unused id, matching an echoed one back, and telling a reply
//!   from an event are ordinary computer science, so this too is host-verified
//!   with no hardware caveat. See [`client`].
//! * **Stage 5 (groundwork) — large-message segmentation.** A single framed
//!   message is bounded by the ring ([`frame::Framer::capacity`]); a logical
//!   payload larger than that is carried as an *ordered sequence* of frames,
//!   each tagged with a FREEHOLD segment header (message id / index / count /
//!   total length) and reassembled in order on the far side.
//!   [`segment::Segmenter::send`] is resumable — it posts as many segments as
//!   the ring holds and returns where to continue, so a message streams under
//!   flow control — and [`segment::Segmenter::push`] advances a
//!   [`segment::Reassembly`] accumulator one frame at a time. Chunking a byte
//!   run and stitching it back in order is ordinary computer science, defined
//!   wholly above the framer, so this is host-verified with no hardware caveat.
//!   See [`segment`].
//! * **Stage 5 (groundwork) — segmented RPC envelopes.** The layer that joins
//!   the two framing siblings above the framer: it assembles an RPC envelope —
//!   header then arguments — into one buffer and carries it, via the
//!   [`segment::Segmenter`], across as many frames as it needs, then reassembles
//!   and decodes it on the far side, so a request, reply or event larger than one
//!   ring message (which the single-frame [`rpc::RpcEndpoint`] refuses with
//!   [`frame::FrameError::TooLarge`]) travels as an ordered segment run. A
//!   message that fits in one frame is a one-segment message, so this generalises
//!   the single-frame path rather than replacing it. Composing an envelope codec
//!   with the segmenter is ordinary computer science, so this is host-verified
//!   with no hardware caveat; which functions the catalog carries stays the
//!   firmware's data. See [`xfer`].
//! * **Stage 5 (groundwork) — segmented calls in the client.** The host driver
//!   that joins the segmented envelope ([`xfer`]) to the in-flight-id bookkeeping
//!   the single-frame client uses: a [`SegmentedClient`](segclient::SegmentedClient)
//!   issues a typed call whose encoded request may exceed one ring message,
//!   driving the resumable send to completion under flow control as the command
//!   ring drains ([`start_call`](segclient::SegmentedClient::start_call) /
//!   [`resume_send`](segclient::SegmentedClient::resume_send)) under a fresh,
//!   reserved request id, and reassembles a segmented reply across successive
//!   [`pump`](segclient::SegmentedClient::pump)s before matching its echoed id.
//!   [`transact`](segclient::SegmentedClient::transact) packages that into one
//!   blocking typed call — post a (possibly multi-frame) request, drive both rings
//!   through a caller [`SegResponder`](segclient::SegResponder) that drains the
//!   command ring between send resumes and waits on the status ring between pumps,
//!   and return the decoded reply or a [`TimedOut`](segclient::SegTransact).
//!   Driving a resumable send and stitching a reply back together are ordinary
//!   computer science, so this too is host-verified with no hardware caveat. See
//!   [`segclient`].
//! * **Stage 5 (groundwork) — GSP channel health-check.** The first real caller
//!   of the catalog's diagnostics: before `NVRTX.EXE` issues any functional call
//!   over a freshly booted GSP channel, it confirms the channel is trustworthy.
//!   [`ping`](health::ping) posts one [`Keepalive`](catalog::Keepalive) and
//!   reports whether a correlated reply came back — *liveness*, the proof the GSP
//!   is draining the command ring and posting status — and
//!   [`check_channel`](health::check_channel) runs that liveness call and then an
//!   [`Echo`](catalog::Echo) sweep whose cookies must each come back bit-for-bit
//!   ([`PROBE_COOKIES`](health::PROBE_COOKIES)) — *integrity*, the proof the
//!   transport carries payload faithfully. The verdict depends on no firmware
//!   data (liveness is a reply arriving, integrity is a cookie unchanged; the
//!   status word is carried out only as information, never judged on), so, like
//!   the diagnostics it composes, this is host-verified with no hardware caveat.
//!   See [`health`].
//! * **Stage 5 (groundwork) — GSP carveout planner.** The step *before* the
//!   shared region exists: the host and GSP talk over a command/status ring pair
//!   in one block of system memory the host allocates and maps (the Stage-3
//!   `DRIVER_MAP` call), and before that block can be asked for, its size and the
//!   placement of each queue inside it must be worked out.
//!   [`plan`](carveout::plan) packs the two queues end to end — each at the next
//!   offset meeting its required alignment — into one carveout, returning the two
//!   *absolute* [`QueueLayout`](msgq::QueueLayout)s (disjoint by construction, and
//!   re-checked against [`rings_disjoint`](channel::rings_disjoint)) and the total
//!   byte size to map. It consumes each queue's firmware-dictated internal shape
//!   and alignment as data and invents no offsets; packing aligned sub-regions and
//!   summing their sizes is ordinary arithmetic, so this is host-verified with no
//!   hardware caveat. See [`carveout`].
//! * **Stage 5 (groundwork) — channel bring-up orchestrator.** The post-boot
//!   analogue of [`bring_up`](bringup::bring_up): the three steps a host runs
//!   after the GSP is booted — [`plan`](carveout::plan) the shared region,
//!   [`establish`](establish::establish) a validated endpoint over the mapped
//!   region, and [`check_channel`](health::check_channel) its liveness and
//!   integrity — composed into one typed [`bring_channel_online`](online::bring_channel_online)
//!   with a single [`ChannelError`](online::ChannelError) tagging the phase that
//!   failed, returning the ready, health-proven [`RpcClient`](client::RpcClient)
//!   for the first functional call. Composing three host-verified steps adds no
//!   dependence on a card, so this is host-verified with no hardware caveat; the
//!   one gated step it spans — the kernel allocating and mapping the region — is
//!   the caller's (a mock region on the host). See [`online`].
//! * **Stage 5 (groundwork) — whole-device bring-up.** The top of the bring-up
//!   tree: [`bring_device_online`](lifecycle::bring_device_online) joins the
//!   pre-GSP [`bring_up`](bringup::bring_up) and the post-GSP
//!   [`bring_channel_online`](online::bring_channel_online) across the one step
//!   only hardware can do — loading the verified firmware, booting the GSP and
//!   mapping the shared region — expressed as a caller-supplied
//!   [`GspBoot`](lifecycle::GspBoot) seam, exactly as each composed orchestrator
//!   leaves its own gated step as a parameter. It returns a
//!   [`DeviceOnline`](lifecycle::DeviceOnline) that is
//!   [`Full`](lifecycle::DeviceOnline::Full) (verified firmware → GSP booted,
//!   channel online and health-proven) or
//!   [`Degraded`](lifecycle::DeviceOnline::Degraded) (no GSP needed, running
//!   without it) — the two states mapping one-to-one onto the two firmware
//!   outcomes [`bring_up`](bringup::bring_up) can succeed with — with one
//!   [`LifecycleError`](lifecycle::LifecycleError) tagging the phase that failed.
//!   Sequencing host-verified steps and branching on their results adds no
//!   dependence on a card, so this is host-verified with no hardware caveat; the
//!   seam is the only hole, filled by a mock on the host and `NVRTX.EXE`'s
//!   firmware-load / boot / `DRIVER_MAP` code on FREEHOLD. See [`lifecycle`].
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod boot;
pub mod bringup;
pub mod call;
pub mod carveout;
pub mod catalog;
pub mod channel;
pub mod client;
pub mod codec;
pub mod device;
pub mod establish;
pub mod event;
pub mod firmware;
pub mod frame;
pub mod health;
pub mod identify;
pub mod ids;
pub mod lifecycle;
pub mod mmio;
pub mod mock;
pub mod msgq;
pub mod online;
pub mod probe;
pub mod regs;
pub mod rpc;
pub mod segclient;
pub mod segment;
pub mod sha256;
pub mod xfer;

pub use boot::{gfw_boot_status, vram_size, wait_gfw_boot, BootError, VramInfo};
pub use bringup::{
    bring_up, Bringup, BringupConfig, BringupError, FirmwareFiles, FirmwareStatus, FirmwareStore,
    FirmwareUnavailable,
};
pub use call::{CallScratch, RpcCall};
pub use carveout::{plan as plan_carveout, Align, Carveout, CarveoutError, Queue, QueueRequest};
pub use catalog::{Echo, Keepalive, NoArgs};
pub use channel::{ring_extent, rings_disjoint, CommandChannel, Doorbell, NullDoorbell};
pub use client::{Pending, Pump, Responder, RpcClient, Transact};
pub use codec::{decode_exact, encode_into, CodecError, Reader, WireDecode, WireEncode, Writer};
pub use device::{BarInfo, BarKind, DeviceInfo, ParseError, BAR_COUNT, DEVICE_BYTES};
pub use establish::{establish, ChannelGeometry, EstablishError, Ring};
pub use event::RpcEvent;
pub use firmware::{
    required_firmware, select, FirmwareHash, FirmwareRequest, FirmwareSelection, FirmwareSidecar,
    HashAlgo, SelectError, SidecarError, SidecarField, VerifyError, SHA256_LEN,
};
pub use frame::{checksum32, FrameError, FrameHeader, FrameLayout, Framer};
pub use health::{check_channel, ping, Health, HealthError, PROBE_COOKIES};
pub use identify::{is_supported, model_name, GpuId};
pub use ids::{Family, NVIDIA_VENDOR, PCI_CLASS_DISPLAY};
pub use lifecycle::{bring_device_online, DeviceOnline, GspBoot, LifecycleError};
pub use mmio::{Bar, Mmio};
pub use msgq::{
    dequeue, enqueue, free_slots, init as queue_init, is_empty as queue_is_empty,
    is_full as queue_is_full, occupancy, QueueError, QueueLayout, SharedMem,
};
pub use online::{bring_channel_online, ChannelConfig, ChannelError, ChannelOnline};
pub use probe::{probe, GpuProbe, ProbeError};
pub use rpc::{Outstanding, RpcEndpoint, RpcError, RpcHeader, RpcInbound, RpcKind, RpcLayout};
pub use segclient::{Posting, SegPump, SegResponder, SegTransact, SegmentedClient, SendCursor};
pub use segment::{
    Reassembly, RecvStep, SegError, SegHeader, Segmenter, SendProgress, SEG_HEADER_LEN,
};
pub use sha256::{sha256, Sha256};
pub use xfer::{SegRecv, SegmentedEndpoint, XferError};
